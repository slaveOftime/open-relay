#!/usr/bin/env bash
# PLAN2 §P1.3 regression guard: the request handlers in `src/http/mod.rs`
# must not call `std::fs::*` directly. All filesystem interactions go
# through one of:
#
#   * `tokio::fs::*`  (already async; correct on the runtime)
#   * `apps::resolve_app_request` / `apps::find_existing_local_asset`
#     (both spawn-blocked under §P1.3)
#   * `try_read_static_file` / `try_read_local_asset` (use tokio::fs)
#
# This lint therefore rejects any new direct `std::fs::*` call inside
# the `mod.rs` async handlers. The same discipline applies recursively
# to the apps module, but a coarse lint would over-flag sync helpers
# that the apps module invokes *from* an outer spawn_blocking closure
# (e.g. `file_exists`, `load_app_manifest`); we trust the existing
# comment trail and pinned test for those.

set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$REPO_ROOT"

REPORT=$(mktemp)
trap 'rm -f "$REPORT"' EXIT

# Only the top-level HTTP router file. The apps module is reviewed by
# code review — its helpers are wrapped at their public boundary and
# collapsing the diff to ship this stand-alone rule keeps the guard
# surgical.
TARGET_FILE="src/http/mod.rs"

if [[ ! -f "$TARGET_FILE" ]]; then
    echo "http-static-blocking lint: target $TARGET_FILE missing" >&2
    exit 1
fi

awk -v FILE="$TARGET_FILE" '
    {
        # Permit test fakes (most live in #[cfg(test)] blocks) and the
        # create_or_load startup path whose std::fs::set_permissions
        # call rotates the key file permissions once at startup before
        # the async runtime even begins serving. Allow the same
        # permission tweak in create_or_load by line tag matching.
        if ($0 ~ /(tokio::fs::|fs::Permissions::from_mode|^\s*_ = tk)/) next
        if ($0 ~ /std::fs::/) {
            printf("%s:%d: std::fs::* call in src/http/mod.rs handlers: %s\n", FILE, NR, $0)
        }
    }
' "$TARGET_FILE" > "$REPORT"

if [[ -s "$REPORT" ]]; then
    cat "$REPORT" >&2
    echo "http-static-blocking lint: $(wc -l < "$REPORT") violation(s); see PLAN2 §P1.3" >&2
    exit 1
fi

echo "http-static-blocking lint: ok"
exit 0
