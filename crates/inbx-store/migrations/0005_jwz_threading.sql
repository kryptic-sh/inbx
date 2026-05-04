-- One row per message-id ever seen (whether the message body is in
-- the `messages` table or only referenced by another message).
CREATE TABLE thread_containers (
    message_id   TEXT PRIMARY KEY,    -- the canonical Message-ID
    parent_id    TEXT,                -- containers.message_id of the parent, or NULL for root
    root_id      TEXT NOT NULL,       -- denormalised: id of the thread root container
    subject_norm TEXT,                -- normalised subject for loose-grouping
    has_message  INTEGER NOT NULL DEFAULT 0,  -- 1 if a `messages` row exists
    FOREIGN KEY (parent_id) REFERENCES thread_containers(message_id)
);

CREATE INDEX thread_containers_parent ON thread_containers(parent_id);
CREATE INDEX thread_containers_root   ON thread_containers(root_id);
CREATE INDEX thread_containers_subj   ON thread_containers(subject_norm);
