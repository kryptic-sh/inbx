CREATE TABLE folders (
    id           INTEGER PRIMARY KEY,
    name         TEXT    NOT NULL UNIQUE,
    delim        TEXT    NULL,
    special_use  TEXT    NULL,
    attrs        TEXT    NULL,
    uidvalidity  INTEGER NULL,
    uidnext      INTEGER NULL
);

CREATE TABLE messages (
    id              INTEGER PRIMARY KEY,
    folder          TEXT    NOT NULL,
    uid             INTEGER NOT NULL,
    uidvalidity     INTEGER NOT NULL,
    message_id      TEXT    NULL,
    subject         TEXT    NULL,
    from_addr       TEXT    NULL,
    to_addrs        TEXT    NULL,
    date_unix       INTEGER NULL,
    flags           TEXT    NOT NULL DEFAULT '',
    maildir_path    TEXT    NULL,
    headers_only    INTEGER NOT NULL DEFAULT 1,
    fetched_at_unix INTEGER NOT NULL,
    UNIQUE(folder, uid, uidvalidity)
);

CREATE INDEX messages_folder_date ON messages(folder, date_unix DESC);
CREATE INDEX messages_message_id   ON messages(message_id);
