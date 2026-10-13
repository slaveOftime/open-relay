-- Scoped machine credentials (ADR-0007, M5-4).
-- Keys created before this migration were used exclusively for node joins,
-- so they default to the 'node' scope. New keys carry an explicit
-- comma-separated scope list ('observe,control,manage,node' or 'all').
ALTER TABLE api_keys ADD COLUMN scopes TEXT NOT NULL DEFAULT 'node';
