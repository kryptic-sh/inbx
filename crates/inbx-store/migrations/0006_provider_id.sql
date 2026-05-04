ALTER TABLE messages ADD COLUMN provider_id TEXT;
CREATE INDEX IF NOT EXISTS idx_messages_provider_id
  ON messages(folder, provider_id)
  WHERE provider_id IS NOT NULL;
