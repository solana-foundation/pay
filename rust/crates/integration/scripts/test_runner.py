"""Safety regressions for the local integration runner; no external services."""

import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import time
import unittest

from build_payment_channels import verify_archive
from run_deployment_e2e import local_environment, run_test_process


class RunnerTests(unittest.TestCase):
    def test_ambient_proxies_and_remote_datasource_are_removed(self):
        env = local_environment({
            "HTTP_PROXY": "http://remote.invalid",
            "https_proxy": "http://remote.invalid",
            "All_Proxy": "http://remote.invalid",
            "NO_PROXY": "",
            "SURFPOOL_DATASOURCE_RPC_URL": "https://remote.invalid",
            "PATH": "/usr/bin",
        })
        self.assertFalse(any(key.lower() in {"http_proxy", "https_proxy", "all_proxy"} for key in env))
        self.assertEqual(env["NO_PROXY"], "*")
        self.assertEqual(env["no_proxy"], "*")
        self.assertNotIn("SURFPOOL_DATASOURCE_RPC_URL", env)
        self.assertEqual(env["PATH"], "/usr/bin")

    def test_unreviewed_source_is_not_attested_as_the_pinned_commit(self):
        with tempfile.TemporaryDirectory() as directory:
            archive = Path(directory) / "forged.tar.gz"
            archive.write_bytes(b"solana-foundation-payment-channels-0c07d57")
            with self.assertRaisesRegex(RuntimeError, "digest differs"):
                verify_archive(archive)

    @unittest.skipUnless(os.name == "posix", "requires POSIX process groups")
    def test_timeout_kills_owned_descendants_even_if_they_ignore_termination(self):
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        with tempfile.TemporaryDirectory() as directory:
            ready = Path(directory) / "ready"
            child = """
import os, signal, socket, sys, time
from pathlib import Path
if os.fork() == 0:
    signal.signal(signal.SIGTERM, signal.SIG_IGN)
    listener = socket.socket()
    listener.bind(("127.0.0.1", int(sys.argv[1])))
    listener.listen()
    Path(sys.argv[2]).write_text("ready")
time.sleep(60)
"""
            with self.assertRaises(subprocess.TimeoutExpired):
                run_test_process(
                    [sys.executable, "-c", child, str(port), str(ready)],
                    cwd=directory,
                    env=local_environment(os.environ),
                    timeout=1,
                )
            self.assertTrue(ready.exists(), "descendant must have started before timeout")
            # SIGKILL delivery to an orphaned descendant is asynchronous.
            deadline = time.monotonic() + 2
            while True:
                try:
                    with socket.socket() as probe:
                        probe.bind(("127.0.0.1", port))
                    break
                except OSError:
                    if time.monotonic() >= deadline:
                        self.fail("descendant still owns its socket after group cleanup")
                    time.sleep(0.01)


if __name__ == "__main__":
    unittest.main()
