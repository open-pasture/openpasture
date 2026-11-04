-- Cross-farm handover: owner-generated one-time transfer codes (op-ingest).
CREATE TABLE collar_releases (
    id TEXT PRIMARY KEY,
    collar_id TEXT NOT NULL,
    serial TEXT,
    device_pubkey TEXT,
    code_hash TEXT NOT NULL UNIQUE,
    expires_at TEXT NOT NULL,
    created_at TEXT NOT NULL,
    created_by TEXT,
    used_at TEXT,
    revoked_at TEXT
);
CREATE INDEX collar_releases_collar ON collar_releases(collar_id, created_at);

ALTER TABLE collars ADD COLUMN released_at TEXT;
