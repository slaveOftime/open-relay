#!/usr/bin/env bash
# PLAN2 §P1.2 regression guard: the CPU-bound `finish_render` step of
# the live log renderer must run on `tokio::task::spawn_blocking`. It
# could starve the PTY reader's write lock if it ran while holding the
# runtime read guard on a tokio worker. The grep here flags any call
# `finish_render(` that is *not* lexically inside a spawn_blocking
# closure, except inside `src/session/logs/render.rs` itself (the
# definition and the wrapper helpers used by tests).
#
# Brace-aware: counts `{` and `}` per line so we can track the depth
# of nested blocks; any spawn_blocking we open increments it, the
# matching `}` decrements it. `finish_render(` calls outside the
# lexically open region are reported.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

REPORT=$(mktemp)
trap 'rm -f "$REPORT"' EXIT

# Files that may legitimately call `finish_render` outside
# spawn_blocking: the module that defines it (single-source helper
# composition) and tests that exercise the closure directly.
ALLOWED_RX='^src/session/logs/(render|tests)\.rs$'

grep -rln "finish_render(" src/ --include='*.rs' \
    | grep -Ev "$ALLOWED_RX" \
    | while read -r file; do
        awk -v FILE="$file" '
            {
                # Track whether we are inside a `spawn_blocking`
                # closure. When we hit a `tokio::task::spawn_blocking`
                # opener, parse its line for braces and bump the
                # depth; for any other line, adjust the depth by the
                # delta of `{` and `}` on the line so nested blocks
                # resolve correctly.
                if (match($0, /tokio::task::spawn_blocking[(]/)) {
                    in_block = 1
                    brace_depth = 0
                    # Continue scanning the rest of this same line for
                    # `{` characters.
                    rest = substr($0, RSTART)
                    opens  = gsub(/\{/, "{", rest)
                    closes = gsub(/\}/, "}", rest)
                    brace_depth += opens - closes
                    if (brace_depth == 0) {
                        # No `{` in the same line; subsequent lines
                        # count until we hit the matching `}`.
                    }
                    next
                }
                if (in_block) {
                    opens  = gsub(/\{/, "{", $0)
                    closes = gsub(/\}/, "}", $0)
                    brace_depth += opens - closes
                    if (brace_depth <= 0) {
                        in_block = 0
                        brace_depth = 0
                    }
                }
                if ($0 ~ /finish_render[(]/) {
                    if (!in_block) {
                        printf("%s:%d: finish_render() outside spawn_blocking: %s\n", FILE, NR, $0)
                    }
                }
            }
        ' "$file"
    done > "$REPORT"

if [[ -s "$REPORT" ]]; then
    cat "$REPORT" >&2
    echo "live-log-render-blocking lint: $(wc -l < "$REPORT") violation(s); see PLAN2 §P1.2" >&2
    exit 1
fi

echo "live-log-render-blocking lint: ok"
exit 0
