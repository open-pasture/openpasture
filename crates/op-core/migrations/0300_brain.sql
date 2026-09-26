-- Keys this server issues to other openpasture servers that use it as their
-- hosted brain (`POST /v1/decide`). Only the sha256 of each key is kept.

CREATE TABLE brain_hosted_keys (
    id          TEXT PRIMARY KEY,
    label       TEXT NOT NULL DEFAULT '',
    key_hash    TEXT NOT NULL UNIQUE,
    created_at  TEXT NOT NULL,
    last_used   TEXT
);
