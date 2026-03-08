-- Initial sessions table.
-- All timestamps are stored as ISO-8601 text (RFC 3339) in UTC.
-- `args` is stored as a JSON array text.
-- Session directories are derived at runtime as <sessions_dir>/<id>/.
CREATE TABLE IF NOT EXISTS sessions (
    id          TEXT NOT NULL PRIMARY KEY,
    title       TEXT,
    command     TEXT NOT NULL,
    args        TEXT NOT NULL DEFAULT '[]',
    cwd         TEXT,
    status      TEXT NOT NULL,
    pid         INTEGER,
    exit_code   INTEGER,
    created_at  TEXT NOT NULL,
    started_at  TEXT,
    ended_at    TEXT
);

CREATE INDEX IF NOT EXISTS idx_sessions_status     ON sessions (status);
CREATE INDEX IF NOT EXISTS idx_sessions_created_at ON sessions (created_at DESC);
