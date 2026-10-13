#!/usr/bin/env python3
"""Create a one-binary release ZIP and SHA-256 sidecar on every platform.

Usage: python3 scripts/release-package.py BINARY_PATH ASSET_PATH
No shell zip/sha256sum installation is required on Windows.
"""

import hashlib
from pathlib import Path
import sys
import zipfile


def package(binary: Path, asset: Path) -> None:
    if not binary.is_file():
        raise ValueError(f"release binary missing: {binary}")
    if binary.name not in ("oly", "oly.exe"):
        raise ValueError("release archive must contain oly or oly.exe")
    info = zipfile.ZipInfo(binary.name)
    info.create_system = 3  # Unix metadata understood by unzip/npm on all hosts.
    info.external_attr = 0o100755 << 16
    info.compress_type = zipfile.ZIP_DEFLATED
    # Stream the binary; no archive path or full binary is retained in memory.
    with zipfile.ZipFile(asset, "w") as archive, binary.open("rb") as source:
        with archive.open(info, "w", force_zip64=True) as target:
            while chunk := source.read(1024 * 1024):
                target.write(chunk)
    with asset.open("rb") as source:
        digest = hashlib.file_digest(source, "sha256").hexdigest()
    Path(f"{asset}.sha256").write_text(f"{digest}  {asset.name}\n", encoding="utf-8", newline="\n")


if __name__ == "__main__":
    try:
        if len(sys.argv) != 3:
            raise ValueError("usage: release-package.py BINARY_PATH ASSET_PATH")
        package(Path(sys.argv[1]), Path(sys.argv[2]))
    except (ValueError, OSError) as error:
        sys.exit(f"release packaging error: {error}")
