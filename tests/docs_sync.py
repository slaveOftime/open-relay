#!/usr/bin/env python3
"""Offline documentation contracts. Run: python3 tests/docs_sync.py.

Checks local links, Markdown structure, copied prompt defaults, configuration
coverage, protocol version, and README scope without starting a daemon.
It does not execute CLI examples or validate remote URLs.
"""

import json
from pathlib import Path
import re
import unittest
from urllib.parse import unquote, urlsplit


ROOT = Path(__file__).resolve().parents[1]
DOCS = [
    ROOT / "README.md",
    ROOT / "ARCHITECTURE.md",
    ROOT / "CONTRIBUTING.md",
    *sorted((ROOT / "docs").rglob("*.md")),
    *sorted((ROOT / "skills").glob("*/SKILL.md")),
]
CONFIG = (ROOT / "src/config.rs").read_text()


def unfenced(text):
    return re.sub(r"^```[^\n]*\n.*?^```\s*$", "", text, flags=re.M | re.S)


def heading_ids(text):
    seen = {}
    result = set()
    for heading in re.findall(r"^#{1,6}\s+(.+?)\s*#*\s*$", unfenced(text), re.M):
        slug = re.sub(r"[^\w\- ]", "", heading.lower()).replace(" ", "-")
        count = seen.get(slug, 0)
        result.add(f"{slug}-{count}" if count else slug)
        seen[slug] = count + 1
    return result


class DocsSyncTests(unittest.TestCase):
    def test_local_links_and_fragments(self):
        for path in DOCS:
            for target in re.findall(r"\[[^\]\n]*\]\(([^)\s]+)\)", unfenced(path.read_text())):
                url = urlsplit(target)
                if url.scheme or url.netloc:
                    continue
                with self.subTest(document=str(path.relative_to(ROOT)), target=target):
                    dest = (path.parent / unquote(url.path)).resolve() if url.path else path
                    self.assertTrue(dest.is_file(), f"Missing local target: {dest}")
                    if url.fragment and dest.suffix == ".md":
                        self.assertIn(unquote(url.fragment), heading_ids(dest.read_text()))

    def test_code_fences_are_balanced(self):
        for path in DOCS:
            with self.subTest(document=str(path.relative_to(ROOT))):
                opened = False
                for line in path.read_text().splitlines():
                    if line.startswith("```"):
                        if opened:
                            self.assertEqual(line.strip(), "```", "Nested or unclosed code fence")
                        opened = not opened
                self.assertFalse(opened, "Unclosed code fence")

    def test_json_examples_parse(self):
        for path in DOCS:
            for example in re.findall(r"^```json\n(.*?)^```\s*$", path.read_text(), re.M | re.S):
                with self.subTest(document=str(path.relative_to(ROOT)), example=example):
                    # The copied defaults are a field fragment for an existing config.
                    value = example.strip()
                    if value.startswith('"'):
                        value = "{" + value + "}"
                    json.loads(value)

    def test_readme_remains_an_entry_point(self):
        readme = (ROOT / "README.md").read_text()
        self.assertLessEqual(len(readme.splitlines()), 160)
        self.assertNotIn("## Command reference", readme)
        for target in ("./docs/guides/cli.md", "./docs/guides/remote-access.md", "./docs/releases/migration-0.5.md"):
            self.assertIn(target, readme)
        self.assertIn("npm install -g @slaveoftime/oly@beta", readme)
        self.assertIn("Node.js/npm", readme)

    def test_docs_index_covers_nested_documents(self):
        index = ROOT / "docs/README.md"
        linked = {
            (index.parent / target).resolve()
            for target in re.findall(r"\[[^\]\n]*\]\(([^)\s]+)\)", index.read_text())
            if not urlsplit(target).scheme
        }
        for path in (ROOT / "docs").rglob("*.md"):
            if path != index:
                with self.subTest(document=str(path.relative_to(ROOT))):
                    self.assertIn(path.resolve(), linked, "Document missing from docs index")
                    self.assertIn(path, DOCS, "Nested document must be checked")
                    self.assertRegex(path.name, r"^[a-z0-9][a-z0-9.-]*\.md$")

    def test_skill_frontmatter_and_embedding(self):
        main = (ROOT / "src/main.rs").read_text()
        for path in sorted((ROOT / "skills").glob("*/SKILL.md")):
            text = path.read_text()
            with self.subTest(skill=path.parent.name):
                self.assertTrue(text.startswith(f"---\nname: {path.parent.name}\n"))
                self.assertRegex(text, r"\ndescription: .+\n---\n")
                # Rust `include_str!` paths in src/main.rs always use '/'
                # (rustc resolves them cross-platform), while `Path` renders
                # with '\' on Windows — compare in the source representation.
                self.assertIn(f'include_str!("../{path.relative_to(ROOT).as_posix()}")', main)

    def test_prompt_defaults_match_source(self):
        source_list = CONFIG.split("const DEFAULT_PROMPT_PATTERNS: &[&str] = &[", 1)[1].split("\n];", 1)[0]
        expected = re.findall(r'r"(.*?)"', source_list)
        skill = (ROOT / "skills/oly-daemon/SKILL.md").read_text()
        fragment = re.search(r'```json\n("prompt_patterns": \[.*?\])\n```', skill, re.S)
        self.assertIsNotNone(fragment)
        self.assertEqual(json.loads("{" + fragment.group(1) + "}")["prompt_patterns"], expected)

    def test_flat_config_table_covers_supported_keys(self):
        fields = []
        for name in ("Http", "Notify", "Resume", "WebPush", "Limits"):
            body = re.search(rf"struct {name}Overrides \{{(.*?)\n\}}", CONFIG, re.S).group(1)
            fields.extend(re.findall(r"^    (\w+): Option<", body, re.M))
        fields.extend(["log_level", "trusted_apps_enabled"])
        skill = (ROOT / "skills/oly-daemon/SKILL.md").read_text()
        table_fields = re.findall(r"^\| `(\w+)` \|", skill, re.M)
        # The startup-override table has flag names, not these config-key rows.
        self.assertEqual(set(table_fields), set(fields))
        self.assertIn("not nested under `limits`", skill)
        self.assertIn("silence-based attention hints/notifications remain", skill)

    def test_migration_protocol_version_matches_source(self):
        protocol = (ROOT / "src/protocol.rs").read_text()
        version = re.search(r"pub const PROTOCOL_VERSION: u16 = (\d+);", protocol).group(1)
        migration = (ROOT / "docs/releases/migration-0.5.md").read_text()
        compatibility = (ROOT / "docs/releases/compatibility-0.5.md").read_text()
        self.assertIn(f"Protocol **{version}**", migration)
        self.assertIn(f"Protocol **{version}**", compatibility)
        self.assertIn(f'{{"version":{version},"payload":...}}', compatibility)
        for path in DOCS:
            self.assertNotIn("src/session/journal.rs", path.read_text())


if __name__ == "__main__":
    unittest.main()
