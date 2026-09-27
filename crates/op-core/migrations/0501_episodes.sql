-- op-ingest (protocol v1): episodes a collar reports, a run of armed warning
-- cues ending turned_back | crossed | rest | boundary_changed. A resent
-- episode (same collar and start) is stored once.
CREATE TABLE episodes (
    id               TEXT PRIMARY KEY,           -- epi_…
    collar_id        TEXT NOT NULL,
    herd_id          TEXT,
    animal_id        TEXT,
    start_t          INTEGER NOT NULL,           -- unix ms
    end_t            INTEGER NOT NULL,
    start_at         TEXT NOT NULL,
    end_at           TEXT NOT NULL,
    boundary_version INTEGER,
    ring             INTEGER NOT NULL,           -- 0 outer, 1.. holes
    cues             INTEGER NOT NULL,
    max_level        INTEGER NOT NULL,
    min_margin_m     REAL NOT NULL,
    outcome          TEXT NOT NULL
);
CREATE UNIQUE INDEX episodes_collar_start ON episodes(collar_id, start_t);
