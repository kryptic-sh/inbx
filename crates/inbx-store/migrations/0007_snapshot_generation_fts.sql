ALTER TABLE folders ADD COLUMN snapshot_generation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE folders ADD COLUMN latest_reserved_generation INTEGER NOT NULL DEFAULT 0;

CREATE TRIGGER messages_delete_fts
AFTER DELETE ON messages
BEGIN
    DELETE FROM messages_fts WHERE rowid = OLD.id;
END;

DELETE FROM messages_fts
WHERE rowid NOT IN (SELECT id FROM messages);
