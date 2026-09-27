-- op-ingest (protocol v1): each collar's current signed config command (the
-- body is the ConfigCommand JSON as sent). `reject_version`/`reject_code`:
-- the collar refused that version, so it isn't sent again.
CREATE TABLE collar_config (
    collar_id      TEXT PRIMARY KEY,
    version        INTEGER NOT NULL,
    body           TEXT NOT NULL,
    updated_at     TEXT NOT NULL,
    reject_version INTEGER,
    reject_code    TEXT
);
