-- A clean federation configuration model. Legacy authorizations are not
-- copied: operators must explicitly provision both peers with verified keys.
DROP TABLE IF EXISTS node_identities;

CREATE TABLE nodes (
    name TEXT PRIMARY KEY NOT NULL,
    role TEXT NOT NULL CHECK (role IN ('primary', 'secondary')),
    public_key TEXT NOT NULL UNIQUE,
    url TEXT,
    as_name TEXT,
    enabled INTEGER NOT NULL CHECK (enabled IN (0, 1)),
    revision TEXT NOT NULL,
    CHECK ((role = 'primary' AND url IS NOT NULL AND as_name IS NOT NULL)
        OR (role = 'secondary' AND url IS NULL AND as_name IS NULL))
);
