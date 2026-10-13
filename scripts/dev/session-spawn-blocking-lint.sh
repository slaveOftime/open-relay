#!/usr/bin/env bash
# PLAN2 §P1.1 regression guard: every `spawn_session(` invocation in a
# file OTHER than the runtime module must run inside a
# `tokio::task::spawn_blocking` closure, because spawn_session performs
# mkdir + ShadowJournal::open (recovery scan + fsyncs) + PATH walk + PTY
# spawn, all of which block a tokio worker thread. A regression that
# surfaces the call inside an async fn would re-stall live attach pumps
# on the 4-worker runtime.
#
# Brace-aware: walks each .rs file, marks a line as "inside a
# spawn_blocking closure" when we are between the opening `{` of a
# `spawn_blocking` block and its matching `}`. Any `spawn_session(`
# encountered while that flag is set is OK; otherwise it's reported.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

REPORT=$(mktemp)
trap 'rm -f "$REPORT"' EXIT

# Aggregate every "bad" call site. `awk` opens and closes braces
# correctly when fed an entire file.
grep -rln "spawn_session(" src/ --include='*.rs' \
    | grep -v "^src/session/runtime\.rs$" \
    | while read -r file; do
        awk -v FILE="$file" '
            function depth_for(line) {
                n = gsub(/\{/, "{", line)
                m = gsub(/\}/, "}", line)
                return n - m
            }
            {
                # Opening a new spawn_blocking block?
                if (match($0, /tokio::task::spawn_blocking[^;]*\{/)) {
                    depth += depth_for(substr($0, RSTART))
                    in_block = (depth > 0)
                    next_loop = 1
                }
            }
            # Outside awk brace block, fall through.
            /^[^ \t].*\{/ && !/tokio::task::spawn_blocking/ {
                depth += depth_for($0)
            }
            {
                if ($0 ~ /spawn_session\(/) {
                    if (in_block) {
                        # allowed
                    } else {
                        printf("%s:%d: spawn_session() outside spawn_blocking: %s\n", FILE, NR, $0)
                    }
                }
            }
        ' "$file"
    done > "$REPORT"

if [[ -s "$REPORT" ]]; then
    cat "$REPORT" >&2
    echo "spawn-blocking lint: $(wc -l < "$REPORT") violation(s); see PLAN2 §P1.1" >&2
    exit 1
fi

echo "spawn-blocking lint: ok"
exit 0
