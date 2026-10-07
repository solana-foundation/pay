#!/usr/bin/env python3
"""Run deployment payment tests against owned, disposable local services."""

import argparse
import base64
from contextlib import ExitStack
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import urllib.request

from build_payment_channels import PROGRAM_ID, SOURCE_REF, SOURCE_ARCHIVE_SHA256


MINT = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
TOKEN_PROGRAM = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
LOADER = "BPFLoader2111111111111111111111111111111111"
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def rpc(url, method, params):
    request = urllib.request.Request(
        url,
        data=json.dumps(
            {"jsonrpc": "2.0", "id": 1, "method": method, "params": params}
        ).encode(),
        headers={"Content-Type": "application/json"},
    )
    with HTTP.open(request, timeout=5) as response:
        result = json.load(response)
    if "error" in result:
        raise RuntimeError(f"{method}: {result['error']}")
    return result["result"]


def unused_ports(count):
    # Hold reservations together so none of the selected ports can be reused.
    with ExitStack() as stack:
        sockets = [stack.enter_context(socket.socket()) for _ in range(count)]
        for sock in sockets:
            sock.bind(("127.0.0.1", 0))
        return [sock.getsockname()[1] for sock in sockets]


def stop(process):
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()

def run_test_process(command, cwd, env, timeout):
    # Cargo spawns the test binary, which may spawn workers. Own the entire
    # group so a hung test cannot outlive its local validator and Redis.
    process = subprocess.Popen(command, cwd=cwd, env=env, start_new_session=True)
    try:
        code = process.wait(timeout=timeout)
        if code:
            raise subprocess.CalledProcessError(code, command)
    finally:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except ProcessLookupError:
            pass
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            pass
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass
        process.wait()


def local_environment(base):
    env = {
        key: value for key, value in base.items()
        if key.lower() not in {"http_proxy", "https_proxy", "all_proxy", "no_proxy"}
    }
    env.pop("SURFPOOL_DATASOURCE_RPC_URL", None)
    env["NO_PROXY"] = "*"
    env["no_proxy"] = "*"
    return env


def wait_ready(process, probe, name):
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if process.poll() is not None:
            raise RuntimeError(f"{name} exited before readiness")
        try:
            if probe():
                return
        except (OSError, ValueError, RuntimeError):
            pass
        time.sleep(0.1)
    raise RuntimeError(f"{name} did not become ready")


def redis_ready(port):
    with socket.create_connection(("127.0.0.1", port), timeout=1) as sock:
        sock.sendall(b"*1\r\n$4\r\nPING\r\n")
        return sock.recv(32) == b"+PONG\r\n"


def main():
    rust = Path(__file__).resolve().parents[3]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--program-so",
        type=Path,
        default=rust / "target" / "deployment-e2e" / "payment_channels.so",
    )
    parser.add_argument("--timeout-seconds", type=int, default=180)
    args = parser.parse_args()
    if os.name != "posix":
        parser.error("the local harness requires POSIX process-group cleanup")
    if args.timeout_seconds <= 0:
        parser.error("--timeout-seconds must be positive")
    for command in ("cargo", "surfpool", "redis-server"):
        if shutil.which(command) is None:
            parser.error(f"{command} must already be installed")
    artifact = args.program_so.resolve()
    metadata = json.loads(artifact.with_suffix(".fixture.json").read_text())
    program = artifact.read_bytes()
    if (
        metadata["source_ref"] != SOURCE_REF
        or metadata["source_archive_sha256"] != SOURCE_ARCHIVE_SHA256
        or metadata["program_id"] != PROGRAM_ID
        or metadata["sha256"] != hashlib.sha256(program).hexdigest()
    ):
        parser.error("program fixture provenance/digest mismatch; rebuild the fixture")
    output = rust / "target" / "deployment-e2e"
    output.mkdir(parents=True, exist_ok=True)
    env = local_environment(os.environ)
    env.update(
        CARGO_PROFILE_DEV_DEBUG="0",
        CARGO_PROFILE_TEST_DEBUG="0",
        CARGO_INCREMENTAL="0",
        CARGO_BUILD_JOBS="2",
        CARGO_TARGET_DIR=str(rust / "target"),
    )
    # Build serially before starting the short-lived services.
    subprocess.run(
        ["cargo", "build", "--locked", "-p", "pay-worker", "--bin", "settle-sessions"],
        cwd=rust,
        env=env,
        check=True,
    )
    rpc_port, ws_port, studio_port, redis_port = unused_ports(4)
    url = f"http://127.0.0.1:{rpc_port}"
    with ExitStack() as stack:
        temporary = Path(stack.enter_context(tempfile.TemporaryDirectory(dir=output)))
        surf_log = stack.enter_context((output / "surfpool.log").open("w"))
        redis_log = stack.enter_context((output / "redis.log").open("w"))
        surfpool = subprocess.Popen(
            [
                "surfpool", "start", "--offline", "--ci", "--no-deploy",
                "--host", "127.0.0.1", "--airdrop-amount", "0",
                "--port", str(rpc_port), "--ws-port", str(ws_port),
                "--studio-port", str(studio_port),
            ],
            cwd=temporary,
            env=env,
            stdin=subprocess.DEVNULL,
            stdout=surf_log,
            stderr=surf_log,
        )
        stack.callback(stop, surfpool)
        redis = subprocess.Popen(
            [
                "redis-server", "--bind", "127.0.0.1", "--port", str(redis_port),
                "--save", "", "--appendonly", "no", "--dir", str(temporary),
            ],
            stdin=subprocess.DEVNULL,
            stdout=redis_log,
            stderr=redis_log,
        )
        stack.callback(stop, redis)
        wait_ready(surfpool, lambda: rpc(url, "getHealth", []) == "ok", "Surfpool")
        wait_ready(redis, lambda: redis_ready(redis_port), "Redis")
        # Install the local ELF at its declared ID via Surfpool's fixture API.
        # This seeds executable code, not channel state or payment outcomes.
        rpc(url, "surfnet_setAccount", [PROGRAM_ID, {
            "lamports": 1_000_000_000, "owner": LOADER,
            "executable": True, "data": program.hex(),
        }])
        loaded = rpc(url, "getAccountInfo", [PROGRAM_ID, {"encoding": "base64"}])["value"]
        if (
            loaded is None
            or not loaded["executable"]
            or loaded["owner"] != LOADER
            or base64.b64decode(loaded["data"][0]) != program
        ):
            raise RuntimeError("local program installation did not round-trip")
        # SPL Mint: no authority, supply zero, six decimals, initialized.
        mint = bytearray(82)
        mint[44:46] = bytes([6, 1])
        rpc(url, "surfnet_setAccount", [MINT, {
            "lamports": 1_461_600, "owner": TOKEN_PROGRAM,
            "executable": False, "data": mint.hex(),
        }])
        env.update(
            PAY_DEPLOYMENT_TEST_RPC_URL=url,
            PAY_DEPLOYMENT_TEST_REDIS_URL=f"redis://127.0.0.1:{redis_port}/",
            PAY_DEPLOYMENT_TEST_WORKER=str(rust / "target" / "debug" / "settle-sessions"),
        )
        print(f"Local validator: {url}; program SHA256: {metadata['sha256']}", flush=True)
        run_test_process(
            [
                "cargo", "test", "--locked", "-p", "pay-integration",
                "--features", "deployment-e2e", "--test", "deployment_policy_e2e",
                "--", "--nocapture", "--test-threads=1",
            ],
            cwd=rust,
            env=env,
            timeout=args.timeout_seconds,
        )


if __name__ == "__main__":
    main()
