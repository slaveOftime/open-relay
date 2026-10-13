#!/usr/bin/env python3
"""Offline release-workflow regressions using tiny archives and a fake npm.

Run from any directory: python3 tests/release_workflow.py
No registry access or publishing occurs.
"""

import importlib.util
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import tomllib
import unittest
import zipfile


WORKFLOW = (
    Path(__file__).resolve().parents[1] / ".github/workflows/release.yml"
).read_text()


def step(name):
    match = re.search(
        rf"^      - name: {re.escape(name)}\n(.*?)(?=^      - |^  [\w-]+:|\Z)",
        WORKFLOW,
        re.MULTILINE | re.DOTALL,
    )
    if not match:
        raise AssertionError(f"Missing step: {name}")
    return match.group(1)


def block(text, key, indent):
    match = re.search(
        rf"^{' ' * indent}{key}: \|\n((?:^{' ' * (indent + 2)}.*\n|^\n)+)",
        text,
        re.MULTILINE,
    )
    if not match:
        raise AssertionError(f"Missing shell block: {key}")
    return "\n".join(line[indent + 2:] for line in match.group(1).splitlines())


class ReleaseWorkflowTests(unittest.TestCase):
    def test_cargo_version_steps_use_shared_locked_transform(self):
        self.assertEqual(WORKFLOW.count('run: python3 scripts/release-version.py "$EFFECTIVE_REF"'), 2)
        self.assertIn("cargo build --release --locked --target", WORKFLOW)
        self.assertIn("python-version: \"3.13\"", WORKFLOW)

    def test_cargo_version_transform_is_exact_repeatable_and_validated(self):
        script = Path(__file__).resolve().parents[1] / "scripts/release-version.py"
        spec = importlib.util.spec_from_file_location("release_version", script)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        for tag in ("v0.5.0-beta.1", "v0.5.0-rc.1", "v0.5.0", "v0.6.0"):
            with self.subTest(tag=tag), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                manifest = '[package]\nname = "oly"\nversion = "0.5.0"\nedition = "2024"\n'
                lock = ('version = 4\n\n[[package]]\nname = "dependency"\nversion = "0.5.0"\n'
                        'source = "registry+https://example.invalid"\n\n[[package]]\n'
                        'name = "oly"\nversion = "0.5.0"\ndependencies = ["dependency"]\n')
                (root / "Cargo.toml").write_text(manifest)
                (root / "Cargo.lock").write_text(lock)
                self.assertEqual(module.set_version(root, tag), tag[1:])
                expected_lock = tomllib.loads(lock)
                expected_lock["package"][1]["version"] = tag[1:]
                self.assertEqual(tomllib.loads((root / "Cargo.lock").read_text()), expected_lock)
                self.assertEqual((root / "Cargo.toml").read_text(), manifest.replace('version = "0.5.0"', f'version = "{tag[1:]}"'))
                before = (root / "Cargo.lock").read_bytes()
                module.set_version(root, tag)
                self.assertEqual((root / "Cargo.lock").read_bytes(), before)
                for invalid in ("v0.5", "0.5.0", "v01.5.0", "v0.5.0-beta.01", "v0.5.0;echo bad"):
                    with self.assertRaises(ValueError):
                        module.set_version(root, invalid)
                    self.assertEqual((root / "Cargo.lock").read_bytes(), before)
                (root / "Cargo.toml").write_text(manifest.replace('"0.5.0"', '"9.0.0"'))
                with self.assertRaises(ValueError):
                    module.set_version(root, tag)
                self.assertEqual((root / "Cargo.lock").read_bytes(), before)

    @unittest.skipUnless(shutil.which("cargo"), "Cargo unavailable")
    def test_tagged_fixture_passes_locked_cargo_metadata(self):
        script = Path(__file__).resolve().parents[1] / "scripts/release-version.py"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "src").mkdir()
            (root / "src/main.rs").write_text("fn main() {}\n")
            (root / "Cargo.toml").write_text('[package]\nname = "oly"\nversion = "0.5.0"\nedition = "2024"\n')
            subprocess.run(["cargo", "generate-lockfile", "--offline"], cwd=root, check=True, capture_output=True)
            for tag in ("v0.5.0-beta.1", "v0.5.0-rc.1", "v0.5.0"):
                subprocess.run([sys.executable, str(script), tag], cwd=root, check=True, capture_output=True)
                result = subprocess.run(["cargo", "metadata", "--locked", "--offline", "--format-version", "1"], cwd=root, check=True, capture_output=True)
                self.assertIn(f'"version":"{tag[1:]}"'.encode(), result.stdout)

    def test_prerelease_uploads_never_promote_latest(self):
        for name in ("Upload to GitHub Release", "Attach SHA256SUMS to release"):
            self.assertIn("prerelease:", step(name))
            self.assertIn("make_latest:", step(name))
            self.assertIn("'false'", step(name))

    def test_retry_actions_have_required_attempt_timeouts(self):
        for name in ("Publish to crates.io", "Bundle binaries + publish to npm"):
            with self.subTest(step=name):
                self.assertRegex(step(name), r"timeout_(?:minutes|seconds): [1-9]\d*")

    def test_manual_release_uploads_target_requested_tag(self):
        self.assertIn("tag_name: ${{ env.EFFECTIVE_REF }}", step("Upload to GitHub Release"))
        self.assertIn(
            "tag_name: ${{ github.event.inputs.tag || github.ref_name }}",
            step("Attach SHA256SUMS to release"),
        )

    def test_concurrency_uses_effective_tag(self):
        self.assertIn(
            "group: ${{ github.workflow }}-${{ github.event.inputs.tag || github.ref_name }}",
            WORKFLOW,
        )

    def test_package_archive_contains_binary_and_valid_checksum(self):
        command = block(step("Package binary + per-asset checksum"), "run", 8)
        command = command.replace("${{ matrix.target }}", "test-target")
        command = command.replace("${{ matrix.artifact_name }}", "oly")
        command = command.replace("${{ matrix.asset_name }}", "oly-test.zip")
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "scripts").mkdir()
            shutil.copyfile(Path(__file__).resolve().parents[1] / "scripts/release-package.py", root / "scripts/release-package.py")
            binary = root / "target/test-target/release/oly"
            binary.parent.mkdir(parents=True)
            binary.write_bytes(b"benign binary fixture")
            subprocess.run(
                ["bash", "-c", command],
                cwd=root,
                env={**os.environ, "GITHUB_WORKSPACE": str(root)},
                check=True,
                capture_output=True,
            )
            with zipfile.ZipFile(root / "oly-test.zip") as archive:
                self.assertEqual(archive.namelist(), ["oly"])
                self.assertEqual(archive.read("oly"), binary.read_bytes())
                self.assertEqual(archive.getinfo("oly").external_attr >> 16, 0o100755)
            before = (root / "oly-test.zip").read_bytes()
            subprocess.run([sys.executable, str(root / "scripts/release-package.py"), str(binary), "oly-test.zip"], cwd=root, check=True)
            self.assertEqual((root / "oly-test.zip").read_bytes(), before)
            subprocess.run(
                ["sha256sum", "--check", "oly-test.zip.sha256"],
                cwd=root,
                check=True,
                capture_output=True,
            )

    def test_checksums_from_flattened_download(self):
        command = block(step("Aggregate per-asset checksums"), "run", 8)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            dist = root / "dist"
            dist.mkdir()
            for name in ("linux", "macos", "windows"):
                (dist / f"oly-{name}.zip.sha256").write_text(f"abc123  oly-{name}.zip\n")
            subprocess.run(["bash", "-c", command], cwd=root, check=True, capture_output=True)
            lines = (root / "SHA256SUMS.txt").read_text().splitlines()
            self.assertEqual(len(lines), 3)
            self.assertEqual(
                set(lines),
                {f"abc123  oly-{name}.zip" for name in ("linux", "macos", "windows")},
            )

    def test_npm_retry_runs_from_workspace_root_and_can_repeat(self):
        command = block(step("Bundle binaries + publish to npm"), "command", 10)
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            dist = root / "dist"
            dist.mkdir()
            bin_dir = root / "npm/bin"
            bin_dir.mkdir(parents=True)
            (bin_dir / "oly.js").write_text("// benign fixture\n")
            (root / "README.md").write_text("release fixture\n")
            for platform, entry in (
                ("linux-amd64", "oly"),
                ("macos-arm64", "oly"),
                ("windows-amd64", "oly.exe"),
            ):
                with zipfile.ZipFile(dist / f"oly-{platform}.zip", "w") as archive:
                    archive.writestr(entry, f"fixture for {platform}\n")
            tools = root / "tools"
            tools.mkdir()
            npm = tools / "npm"
            npm.write_text(
                '#!/bin/bash\nset -eu\n'
                'test "$PWD" = "$GITHUB_WORKSPACE/npm"\n'
                'test "$*" = "publish --access public --tag beta"\n'
                'test "$NODE_AUTH_TOKEN" = "offline-test-token"\n'
                'printf "%s\\n" "$*" >> "$GITHUB_WORKSPACE/publish-calls"\n'
            )
            npm.chmod(0o755)
            env = {
                **os.environ,
                "GITHUB_WORKSPACE": str(root),
                "PATH": f"{tools}:{os.environ['PATH']}",
                "DIST_TAG": "beta",
                "NODE_AUTH_TOKEN": "offline-test-token",
            }
            # A JavaScript action starts at the workspace root, NOT the
            # defaults.run.working-directory. Run twice to exercise retries.
            for _ in range(2):
                subprocess.run(
                    ["bash", "-c", command], cwd=root, env=env,
                    check=True, capture_output=True,
                )
            self.assertEqual(len((root / "publish-calls").read_text().splitlines()), 2)
            for name, platform in (
                ("oly-linux-x64", "linux-amd64"),
                ("oly-darwin-arm64", "macos-arm64"),
                ("oly-win32-x64.exe", "windows-amd64"),
            ):
                self.assertEqual((bin_dir / name).read_text(), f"fixture for {platform}\n")
            self.assertEqual((root / "npm/README.md").read_text(), "release fixture\n")


if __name__ == "__main__":
    unittest.main()
