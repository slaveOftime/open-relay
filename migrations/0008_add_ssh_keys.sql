-- 0008_add_ssh_keys.sql
-- Table for SSH public keys authorized for node join.
CREATE TABLE IF NOT EXISTS ssh_keys (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    name      TEXT UNIQUE NOT NULL,
    key_data  TEXT NOT NULL,  -- SSH public key in OpenSSH format, e.g. "ssh-ed25519 AAAA..."
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_ssh_keys_key_data ON ssh_keys(key_data);