CREATE TABLE contacts (
    id              INTEGER PRIMARY KEY,
    email           TEXT    NOT NULL UNIQUE COLLATE NOCASE,
    name            TEXT    NULL,
    frecency_count  INTEGER NOT NULL DEFAULT 0,
    last_used_unix  INTEGER NOT NULL DEFAULT 0
);

CREATE INDEX contacts_frecency
    ON contacts(frecency_count DESC, last_used_unix DESC);
