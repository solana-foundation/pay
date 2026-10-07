#!/usr/bin/env python3
"""Build the same local-only payment program fixture as pay-kit's CI action."""

import argparse
import hashlib
import json
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tarfile
import tempfile
import urllib.request


# Keep aligned with pay-kit/.github/actions/build-payment-channels/action.yml.
SOURCE_REF = "0c07d5751c8972abf6a219570a3f39a72f46f879"
SOURCE_ARCHIVE_SHA256 = "8aaba1dbc9f95512382720817f3064a6262ad49483b481c46bc664433ff042a1"
PROGRAM_ID = "CHNLxYvVA28MJP9PrFuDXccuoGXAx7jBacfLEkahyGsX"
TREASURY = """const TREASURY_OWNER_SENTINEL: [u8; 32] = [
    0xb0, 0x41, 0xd9, 0xd3, 0x37, 0xb7, 0x21, 0xbe, 0x57, 0x89, 0x4e, 0xb6, 0x9c, 0x3b, 0x68, 0x09,
    0xa5, 0x3a, 0x0e, 0x2b, 0x6a, 0x23, 0x99, 0xfc, 0x7d, 0x5b, 0x7e, 0xda, 0x8c, 0xac, 0x89, 0xaa,
];"""

def verify_archive(archive):
    digest = hashlib.sha256()
    with archive.open("rb") as source:
        for chunk in iter(lambda: source.read(64 * 1024), b""):
            digest.update(chunk)
    if digest.hexdigest() != SOURCE_ARCHIVE_SHA256:
        raise RuntimeError("source archive digest differs from the reviewed pinned archive")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", type=Path, help="Previously downloaded pinned source tarball")
    parser.add_argument(
        "--output",
        type=Path,
        default=Path(__file__).resolve().parents[3] / "target" / "deployment-e2e",
    )
    args = parser.parse_args()
    if shutil.which("cargo-build-sbf") is None:
        parser.error("cargo-build-sbf must already be installed; this script installs no tools")
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="program-build-", dir=output) as temporary:
        root = Path(temporary)
        archive = args.archive
        if archive is None:
            archive = root / "source.tar.gz"
            url = f"https://api.github.com/repos/solana-foundation/payment-channels/tarball/{SOURCE_REF}"
            request = urllib.request.Request(url, headers={"User-Agent": "pay-local-integration"})
            with urllib.request.urlopen(request, timeout=60) as response:
                with archive.open("wb") as target:
                    shutil.copyfileobj(response, target)
        verify_archive(archive)
        source = root / "source"
        source.mkdir()
        with tarfile.open(archive, "r:gz") as tar:
            # Also support Python versions before tarfile's data filter.
            for member in tar.getmembers():
                path = PurePosixPath(member.name)
                if (
                    path.is_absolute()
                    or ".." in path.parts
                    or not (member.isfile() or member.isdir())
                ):
                    raise RuntimeError("source archive contains an unsafe entry")
            tar.extractall(source)
        children = list(source.iterdir())
        if len(children) != 1 or not children[0].is_dir():
            raise RuntimeError("expected one root directory in the source archive")
        checkout = children[0]
        if not checkout.name.endswith(SOURCE_REF[:7]):
            raise RuntimeError("archive root does not match the pinned source revision")
        program = checkout / "program" / "payment_channels"
        if f'declare_id!("{PROGRAM_ID}")' not in (program / "src" / "lib.rs").read_text():
            raise RuntimeError("source program ID differs from the expected fixture")
        constants = program / "src" / "constants.rs"
        patched, count = re.subn(
            r"const TREASURY_OWNER_SENTINEL: \[u8; 32\] = \[\n.*?\n\];",
            TREASURY,
            constants.read_text(),
            flags=re.DOTALL,
        )
        if count != 1:
            raise RuntimeError("localnet treasury patch did not match exactly once")
        constants.write_text(patched)
        env = os.environ.copy()
        env["CARGO_INCREMENTAL"] = "0"
        # Do not inherit a target directory belonging to another workspace.
        env["CARGO_TARGET_DIR"] = str(checkout / "target")
        subprocess.run(
            ["cargo", "build-sbf", "--tools-version", "v1.52", "--arch", "v1"],
            cwd=program,
            env=env,
            check=True,
        )
        built = checkout / "target" / "deploy" / "payment_channels.so"
        artifact = output / "payment_channels.so"
        shutil.copy2(built, artifact)
        metadata = {
            "source_ref": SOURCE_REF,
            "source_archive_sha256": SOURCE_ARCHIVE_SHA256,
            "program_id": PROGRAM_ID,
            "tools_version": "v1.52",
            "arch": "v1",
            "treasury": "pay-kit CI localnet sentinel",
            "sha256": hashlib.sha256(artifact.read_bytes()).hexdigest(),
        }
        (output / "payment_channels.fixture.json").write_text(
            json.dumps(metadata, indent=2) + "\n"
        )
        print(f"Local test artifact: {artifact}")
        print(f"SHA256: {metadata['sha256']}")


if __name__ == "__main__":
    main()
