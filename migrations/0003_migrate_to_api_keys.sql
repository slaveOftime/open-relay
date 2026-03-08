-- Decouple authentication from node identity.
-- API keys are now independent of node names; any valid key allows a secondary
-- to join the primary under a self-chosen (unique) name.
DROP TABLE IF EXISTS nodes;

CREATE TABLE IF NOT EXISTS api_keys (
    name         TEXT NOT NULL PRIMARY KEY,
    api_key_hash TEXT NOT NULL,
    created_at   TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%SZ', 'now'))
);
