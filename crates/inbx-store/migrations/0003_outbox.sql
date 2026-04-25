CREATE TABLE outbox (
    id              INTEGER PRIMARY KEY,
    enqueued_unix   INTEGER NOT NULL,
    raw             BLOB    NOT NULL,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_retry_unix INTEGER NOT NULL,
    last_error      TEXT    NULL
);

CREATE INDEX outbox_next_retry ON outbox(next_retry_unix);
