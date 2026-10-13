#!/usr/bin/env python3
"""Set the root Cargo package version from a release tag without dependency churn.

Usage: python3 scripts/release-version.py v0.5.0-beta.1
Run from the checkout root. Both files are validated before either is written.
"""

from pathlib import Path
import re
import sys
import tomllib


TAG = re.compile(
    r"v(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)"
    r"(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?"
)


def set_version(root: Path, tag: str) -> str:
    match = TAG.fullmatch(tag)
    if not match or any(
        part.isdigit() and len(part) > 1 and part.startswith("0")
        for part in (match.group(4) or "").split(".")
    ):
        raise ValueError("expected vMAJOR.MINOR.PATCH[-PRERELEASE], without leading numeric zeroes")
    version = tag[1:]
    manifest_path = root / "Cargo.toml"
    lock_path = root / "Cargo.lock"
    manifest = manifest_path.read_text(encoding="utf-8")
    lock = lock_path.read_text(encoding="utf-8")
    package = tomllib.loads(manifest)["package"]
    if package["name"] != "oly":
        raise ValueError("release checkout must contain the oly root package")
    old_version = package["version"]
    packages = tomllib.loads(lock)["package"]
    roots = [p for p in packages if p["name"] == "oly" and "source" not in p]
    if len(roots) != 1 or roots[0]["version"] != old_version:
        raise ValueError("Cargo.toml and the root oly entry in Cargo.lock must match before release")

    # Match only the [package] section and the source-less oly lock entry.
    # Never regenerate the dependency graph from a registry during publication.
    manifest_pattern = r"(?ms)(^\[package\]\n(?:(?!^\[).)*?^version = )\"[^\"]+\""
    manifest_next, count = re.subn(manifest_pattern, lambda m: f'{m[1]}"{version}"', manifest)
    if count != 1:
        raise ValueError("could not identify the root manifest version uniquely")
    lock_blocks = re.split(r"(?m)(?=^\[\[package\]\]$)", lock)
    count = 0
    for index, block in enumerate(lock_blocks):
        if block.startswith("[[package]]"):
            parsed = tomllib.loads(block)["package"][0]
            if parsed["name"] == "oly" and "source" not in parsed:
                block, replacements = re.subn(
                    r'^version = "[^"]+"$', f'version = "{version}"', block, count=1, flags=re.M
                )
                count += replacements
                lock_blocks[index] = block
    if count != 1:
        raise ValueError("could not identify the root lockfile version uniquely")
    lock_next = "".join(lock_blocks)
    # Prove the transformation changes only the intended TOML values.
    expected_manifest = tomllib.loads(manifest)
    expected_manifest["package"]["version"] = version
    expected_lock = tomllib.loads(lock)
    next(p for p in expected_lock["package"] if p["name"] == "oly" and "source" not in p)["version"] = version
    if tomllib.loads(manifest_next) != expected_manifest or tomllib.loads(lock_next) != expected_lock:
        raise ValueError("version transformation unexpectedly changed other fields")
    if manifest_next != manifest:
        manifest_path.write_text(manifest_next, encoding="utf-8", newline="\n")
    if lock_next != lock:
        lock_path.write_text(lock_next, encoding="utf-8", newline="\n")
    return version


if __name__ == "__main__":
    try:
        if len(sys.argv) != 2:
            raise ValueError("usage: python3 scripts/release-version.py vMAJOR.MINOR.PATCH[-PRERELEASE]")
        print(f"Cargo release version: {set_version(Path.cwd(), sys.argv[1])}")
    except (ValueError, OSError, KeyError) as error:
        sys.exit(f"release version error: {error}")
