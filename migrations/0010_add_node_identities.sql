-- Federation identity cleanup — step 1: add the new table.
--
-- This migration only creates the `node_identities` table. Dropping the old
-- `api_keys` and `ssh_keys` tables happens in a later migration (or PR)
-- once every reader of those tables is gone.
--
-- Schema choice:
--   * `name` is the PRIMARY KEY (an operator-supplied primary identifier
--     for a registered secondary; each name maps to at most one identity).
--   * `public_key` is `NOT NULL UNIQUE`: one identity cannot be bound to
--     multiple registered names. Bind-by-name is checked on every join;
--     bind-by-key is a uniqueness invariant.
--   * `created_at` is informational; trust decisions do not depend on it.
--
-- The literal column value is the canonical textual form:
--   "ed25519 <standard-base64(raw32)>"
-- i.e. oly's own typed encoding; see src/federation_crypto.rs.
CREATE TABLE IF NOT EXISTS node_identities (
    name        TEXT PRIMARY KEY,
    public_key  TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
