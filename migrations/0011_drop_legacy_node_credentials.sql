-- Clean break: obsolete credentials are not copied to node_identities.
-- Historical migration files and unrelated session/push data are preserved.
DROP TABLE api_keys;
DROP TABLE ssh_keys;
