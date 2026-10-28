#!/usr/bin/env python3
"""Render versioned release notes as a GitHub Release body.

GitHub Releases render Markdown detached from the repository, so relative
links such as ``./whats-new-0.5.md`` do not resolve there. This script pins
every repository-relative link and image to the release tag and refuses to
run when the notes file is missing.

Usage:
    python3 scripts/release-notes.py v0.5.0 --output release-body.md
    python3 scripts/release-notes.py v0.5.0-beta.2 --from-git

Run from the repository root. ``--repo`` defaults to ``GITHUB_REPOSITORY``.
"""

from __future__ import annotations

import argparse
import os
from pathlib import Path
import posixpath
import re
import subprocess
import sys
from urllib.parse import quote


TAG = re.compile(
    r"v(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)\.(?:0|[1-9][0-9]*)"
    r"(?:-[0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*)?"
)
REPO = re.compile(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+")
NOTES_DIR = Path("docs/releases")
FENCE = re.compile(r"(?ms)^[ \t]*```[^\n]*\n.*?^[ \t]*```[ \t]*$")
INLINE = re.compile(
    r"(?P<image>!?)\[(?P<text>(?:\\.|[^\]\\])*)\]"
    r"\((?P<dest><[^>\n]*>|[^\s()]+)(?P<title>\s+(?:\"[^\"]*\"|'[^']*'|\([^()]*\)))?\)"
)
REFERENCE = re.compile(r"(?m)^(?P<prefix>\[[^\]]+\]:[ \t]*)(?P<dest>\S+)(?P<rest>.*)$")
EXTERNAL = re.compile(r"[A-Za-z][A-Za-z0-9+.-]*:")


def notes_path(tag: str) -> Path:
    if not TAG.fullmatch(tag):
        raise ValueError("expected vMAJOR.MINOR.PATCH[-PRERELEASE]")
    return NOTES_DIR / f"release-notes-{tag[1:]}.md"


def rewrite_target(target: str, source: Path, repo: str, ref: str, image: bool) -> str:
    if target.startswith("<") and target.endswith(">"):
        target = target[1:-1]
    path, separator, fragment = target.partition("#")
    if not path or path.startswith("#"):
        return target
    if EXTERNAL.match(path) or path.startswith("//"):
        return target
    if path.startswith("/"):
        joined = path.lstrip("/")
    else:
        joined = posixpath.normpath(posixpath.join(source.parent.as_posix(), path))
    if joined in ("", ".", "..") or joined.startswith("../"):
        raise ValueError(f"{source}: link escapes the repository: {target}")
    quoted = quote(joined, safe="/")
    if image:
        url = f"https://raw.githubusercontent.com/{repo}/{ref}/{quoted}"
    else:
        url = f"https://github.com/{repo}/blob/{ref}/{quoted}"
    return f"{url}#{fragment}" if separator else url


def _rewrite_inline(text: str, source: Path, repo: str, ref: str) -> str:
    def replace(match: re.Match[str]) -> str:
        target = rewrite_target(
            match["dest"], source, repo, ref, bool(match["image"])
        )
        return f'{match["image"]}[{match["text"]}]({target}{match["title"] or ""})'

    return INLINE.sub(replace, text)


def _rewrite_reference(text: str, source: Path, repo: str, ref: str) -> str:
    def replace(match: re.Match[str]) -> str:
        target = rewrite_target(match["dest"], source, repo, ref, False)
        return f'{match["prefix"]}{target}{match["rest"]}'

    return REFERENCE.sub(replace, text)


def _rewrite_segment(text: str, source: Path, repo: str, ref: str) -> str:
    return _rewrite_reference(_rewrite_inline(text, source, repo, ref), source, repo, ref)


def render(text: str, source: Path, repo: str, ref: str) -> str:
    """Rewrite repository-relative links outside fenced code blocks."""
    parts = []
    last = 0
    for fence in FENCE.finditer(text):
        parts.append(_rewrite_segment(text[last : fence.start()], source, repo, ref))
        parts.append(fence.group(0))
        last = fence.end()
    parts.append(_rewrite_segment(text[last:], source, repo, ref))
    return "".join(parts)


def read_notes(tag: str, source: Path, from_git: bool) -> str:
    if from_git:
        result = subprocess.run(
            ["git", "show", f"{tag}:{source.as_posix()}"],
            capture_output=True,
            text=True,
            encoding="utf-8",
        )
        if result.returncode != 0:
            raise ValueError(f"{source} not found at {tag}; commit it before tagging")
        return result.stdout
    if not source.is_file():
        raise ValueError(f"{source} not found; commit it before tagging")
    return source.read_text(encoding="utf-8")


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("tag", help="release tag, for example v0.5.0")
    parser.add_argument(
        "--repo",
        default=os.environ.get("GITHUB_REPOSITORY", ""),
        help="GitHub repository as OWNER/NAME (default: GITHUB_REPOSITORY)",
    )
    parser.add_argument("--output", help="write the body here instead of stdout")
    parser.add_argument(
        "--from-git",
        action="store_true",
        help="read the notes from the tag's git object instead of the working tree",
    )
    args = parser.parse_args(argv)

    try:
        if not REPO.fullmatch(args.repo):
            raise ValueError("--repo OWNER/NAME (or GITHUB_REPOSITORY) is required")
        source = notes_path(args.tag)
        text = read_notes(args.tag, source, args.from_git)
        if not text.strip():
            raise ValueError(f"{source} is empty")
        body = render(text, source, args.repo, args.tag)
        if args.output:
            Path(args.output).write_text(body, encoding="utf-8", newline="\n")
            print(f"Wrote {args.output} from {source}")
        else:
            sys.stdout.reconfigure(encoding="utf-8")
            sys.stdout.write(body)
    except (OSError, ValueError) as error:
        sys.exit(f"release notes error: {error}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
