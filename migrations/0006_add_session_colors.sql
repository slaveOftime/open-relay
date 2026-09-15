-- Terminal-emitted default foreground/background colours (OSC 10 / OSC 11).
-- Stored as the raw colour spec payload (e.g. "rgb:ffff/ffff/ffff", "#ffffff",
-- or a colour name); NULL when the session never set one.
ALTER TABLE sessions ADD COLUMN foreground_color TEXT;
ALTER TABLE sessions ADD COLUMN background_color TEXT;
