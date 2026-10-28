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


def release_notes_module():
    script = Path(__file__).resolve().parents[1] / "scripts/release-notes.py"
    spec = importlib.util.spec_from_file_location("release_notes", script)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


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

    def test_prerelease_release_never_promotes_latest(self):
        release = step("Create GitHub release")
        self.assertIn("prerelease:", release)
        self.assertIn("make_latest:", release)
        self.assertIn("'false'", release)

    def test_retry_actions_have_required_attempt_timeouts(self):
        for name in ("Publish to crates.io", "Bundle binaries + publish to npm"):
            with self.subTest(step=name):
                self.assertRegex(step(name), r"timeout_(?:minutes|seconds): [1-9]\d*")

    def test_manual_release_uploads_target_requested_tag(self):
        self.assertIn("tag_name: ${{ env.EFFECTIVE_REF }}", step("Create GitHub release"))

    def test_release_body_is_the_versioned_notes_file(self):
        release = step("Create GitHub release")
        self.assertIn("body_path: release-body.md", release)
        self.assertIn("generate_release_notes: false", release)
        self.assertNotIn("generate_release_notes: true", WORKFLOW)
        # Release creation belongs to one job; per-matrix uploads are what
        # appended the generated section four times.
        self.assertNotIn("Upload to GitHub Release", WORKFLOW)
        self.assertIn(
            'scripts/release-notes.py "$EFFECTIVE_REF"',
            step("Render release notes"),
        )

    def test_release_notes_render_pins_relative_links_to_the_tag(self):
        module = release_notes_module()
        rendered = module.render(
            "[guide](./whats-new-0.5.md)\n"
            "[cli](../guides/cli.md#logs-views-waits-and-byte-continuation)\n"
            "![shot](./img/shot.png)\n"
            "[site](https://example.com/a)\n"
            "[jump](#highlights) [mail](mailto:release@example.com)\n"
            "```sh\n"
            "[not a link](./ignored.md)\n"
            "```\n",
            Path("docs/releases/release-notes-0.5.0.md"),
            "owner/repo",
            "v0.5.0",
        )
        self.assertIn(
            "[guide](https://github.com/owner/repo/blob/v0.5.0/docs/releases/whats-new-0.5.md)",
            rendered,
        )
        self.assertIn(
            "[cli](https://github.com/owner/repo/blob/v0.5.0/docs/guides/cli.md#logs-views-waits-and-byte-continuation)",
            rendered,
        )
        self.assertIn(
            "![shot](https://raw.githubusercontent.com/owner/repo/v0.5.0/docs/releases/img/shot.png)",
            rendered,
        )
        self.assertIn("[site](https://example.com/a)", rendered)
        self.assertIn("[jump](#highlights)", rendered)
        self.assertIn("[mail](mailto:release@example.com)", rendered)
        self.assertIn("[not a link](./ignored.md)", rendered)

    def test_release_notes_reject_links_that_escape_the_repository(self):
        module = release_notes_module()
        with self.assertRaises(ValueError):
            module.rewrite_target(
                "../../../../etc/passwd",
                Path("docs/releases/release-notes-0.5.0.md"),
                "owner/repo",
                "v0.5.0",
                False,
            )

    def test_release_notes_renderer_requires_the_versioned_notes(self):
        script = Path(__file__).resolve().parents[1] / "scripts/release-notes.py"
        with tempfile.TemporaryDirectory() as tmp:
            result = subprocess.run(
                [sys.executable, str(script), "v9.9.9", "--repo", "owner/repo"],
                cwd=tmp,
                capture_output=True,
                text=True,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(str(Path("docs/releases/release-notes-9.9.9.md")), result.stderr)
        self.assertIn("commit it before tagging", result.stderr)

    def test_checked_in_release_notes_have_no_relative_links(self):
        module = release_notes_module()
        root = Path(__file__).resolve().parents[1]
        notes = sorted((root / "docs/releases").glob("release-notes-*.md"))
        self.assertTrue(notes)
        for note in notes:
            tag = "v" + note.name[len("release-notes-"):-len(".md")]
            with self.subTest(note=note.name):
                rendered = module.render(
                    note.read_text(encoding="utf-8"),
                    note.relative_to(root),
                    "owner/repo",
                    tag,
                )
                self.assertNotIn("](./", rendered)
                self.assertNotIn("](../", rendered)

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
            for name in ("linux", "macos", "windows", "linux-arm"):
                (dist / f"oly-{name}.zip.sha256").write_text(f"abc123  oly-{name}.zip\n")
            subprocess.run(["bash", "-c", command], cwd=root, check=True, capture_output=True)
            lines = (root / "SHA256SUMS.txt").read_text().splitlines()
            self.assertEqual(len(lines), 4)
            self.assertEqual(
                set(lines),
                {f"abc123  oly-{name}.zip" for name in ("linux", "macos", "windows", "linux-arm")},
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
                ("linux-arm64", "oly"),
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
                ("oly-linux-arm64", "linux-arm64"),
                ("oly-darwin-arm64", "macos-arm64"),
                ("oly-win32-x64.exe", "windows-amd64"),
            ):
                self.assertEqual((bin_dir / name).read_text(), f"fixture for {platform}\n")
            self.assertEqual((root / "npm/README.md").read_text(), "release fixture\n")

    def test_npm_bundle_and_launcher_agree_on_supported_platforms(self):
        """The launcher resolves one file per platform; the publish step must
        bundle exactly those files. A mismatch ships a package whose launcher
        exits with 'unsupported platform/arch' on a platform npm accepted."""
        root = Path(__file__).resolve().parents[1]
        launcher = (root / "npm/bin/oly.js").read_text()
        mapped = set(
            re.findall(r'^\s*"(?:linux|darwin|win32):(?:x64|arm64)": "([^"]+)"', launcher, re.MULTILINE)
        )
        self.assertTrue(mapped, "oly.js declares no platform map")

        command = block(step("Bundle binaries + publish to npm"), "command", 10)
        bundled = set(re.findall(r"bin/(oly-[A-Za-z0-9._-]+)", command))
        self.assertEqual(
            bundled,
            mapped,
            f"bundle {sorted(bundled)} != launcher map {sorted(mapped)}",
        )

    def test_homebrew_formula_declares_only_built_assets(self):
        """The formula rewrites checksums for assets it names; naming an asset
        the release matrix does not build would leave a stale checksum."""
        root = Path(__file__).resolve().parents[1]
        formula = (root / "Formula/oly.rb").read_text()
        declared = set(re.findall(r"/(oly-[A-Za-z0-9._-]+\.zip)\"", formula))
        built = set(re.findall(r"^\s+asset_name: (oly-\S+)$", WORKFLOW, re.MULTILINE))
        self.assertTrue(declared, "formula declares no download url")
        self.assertTrue(built, "release matrix declares no asset_name")
        self.assertEqual(declared - built, set(), "formula names assets the release does not build")

    def formula_update_script(self):
        script = block(step("Update Formula/oly.rb"), "run", 8)
        match = re.search(r"ruby <<'RUBY'\n(.*?)\nRUBY", script, re.DOTALL)
        self.assertIsNotNone(match, "formula updater heredoc missing")
        return script, match.group(1)

    @unittest.skipUnless(shutil.which("ruby"), "Ruby unavailable")
    @unittest.skipUnless(shutil.which("bash"), "bash unavailable")
    def test_homebrew_formula_update_rewrites_every_declared_checksum(self):
        script, _ = self.formula_update_script()
        root = Path(__file__).resolve().parents[1]
        original = (root / "Formula/oly.rb").read_text()
        shas = {"oly-macos-arm64.zip": "a" * 64, "oly-linux-amd64.zip": "b" * 64}
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            (work / "Formula").mkdir()
            (work / "Formula/oly.rb").write_text(original)
            dist = work / "dist"
            dist.mkdir()
            for asset, sha in shas.items():
                (dist / f"{asset}.sha256").write_text(f"{sha}  {asset}\n")
            subprocess.run(
                ["bash", "-c", script],
                cwd=work,
                env={**os.environ, "VERSION": "v9.9.9"},
                check=True,
                capture_output=True,
            )
            updated = (work / "Formula/oly.rb").read_text()

        self.assertIn('  version "9.9.9"', updated)
        declared = set(re.findall(r"/(oly-[A-Za-z0-9._-]+\.zip)\"", original))
        self.assertEqual(
            set(shas),
            declared,
            "formula platforms and the workflow's checksum variables drifted apart",
        )
        for asset, sha in shas.items():
            with self.subTest(asset=asset):
                # The checksum is rewritten on the first `sha256` line after
                # the matching url. `Formula/oly.rb` documents there that its
                # checksum is generated, so that comment sits directly above
                # it; the updater tolerates comment lines in between and this
                # assertion has to allow exactly the same thing. Taking the
                # first non-comment line keeps it strict: a platform whose
                # checksum line disappeared must not borrow the next one.
                lines = updated.splitlines()
                index = next(i for i, line in enumerate(lines) if f"/{asset}\"" in line)
                following = [
                    line
                    for line in lines[index + 1 :]
                    if line.strip() and not line.strip().startswith("#")
                ]
                self.assertTrue(following, f"{asset}: nothing follows its url")
                self.assertIn(f'sha256 "{sha}"', following[0])
        # Exactly the version line plus one checksum line per declared asset.
        before, after = original.splitlines(), updated.splitlines()
        self.assertEqual(len(before), len(after))
        self.assertEqual(
            len([i for i, (a, b) in enumerate(zip(before, after)) if a != b]), 1 + len(shas)
        )

    @unittest.skipUnless(shutil.which("ruby"), "Ruby unavailable")
    @unittest.skipUnless(shutil.which("bash"), "bash unavailable")
    def test_homebrew_formula_update_rejects_an_undeclared_asset(self):
        """A formula missing a platform must fail the release loudly rather
        than silently publish a stale checksum for it."""
        script, _ = self.formula_update_script()
        root = Path(__file__).resolve().parents[1]
        original = (root / "Formula/oly.rb").read_text()
        with tempfile.TemporaryDirectory() as tmp:
            work = Path(tmp)
            (work / "Formula").mkdir()
            (work / "Formula/oly.rb").write_text(
                original.replace('url "https://github.com/slaveOftime/open-relay/releases/download/v#{version}/oly-linux-amd64.zip"\n', "")
            )
            dist = work / "dist"
            dist.mkdir()
            for asset in ("oly-macos-arm64.zip", "oly-linux-amd64.zip"):
                (dist / f"{asset}.sha256").write_text(f"{'c' * 64}  {asset}\n")
            result = subprocess.run(
                ["bash", "-c", script],
                cwd=work,
                env={**os.environ, "VERSION": "v9.9.9"},
                capture_output=True,
                text=True,
            )
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("oly-linux-amd64.zip", result.stderr)


if __name__ == "__main__":
    unittest.main()
