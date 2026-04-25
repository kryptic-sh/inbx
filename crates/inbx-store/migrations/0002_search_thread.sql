ALTER TABLE messages ADD COLUMN in_reply_to TEXT;
ALTER TABLE messages ADD COLUMN refs        TEXT;
ALTER TABLE messages ADD COLUMN thread_id   TEXT;

CREATE INDEX messages_thread ON messages(thread_id);

CREATE VIRTUAL TABLE messages_fts USING fts5(
    subject,
    from_addr,
    to_addrs,
    body,
    tokenize = 'porter ascii'
);
