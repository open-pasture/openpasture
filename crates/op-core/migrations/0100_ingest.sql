-- op-ingest.

-- One row per position report: battery and receiver health over time.
CREATE TABLE health (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    collar_id        TEXT NOT NULL,
    herd_id          TEXT,
    at               TEXT NOT NULL,              -- server time
    t                INTEGER NOT NULL,
    battery          REAL,
    sats             INTEGER,
    cn0              REAL,
    ttf_s            REAL,
    fixes            INTEGER NOT NULL DEFAULT 0,
    cues             INTEGER NOT NULL DEFAULT 0,
    boundary_version INTEGER
);
CREATE INDEX health_collar_t ON health(collar_id, t);
CREATE INDEX health_herd_t ON health(herd_id, t);
