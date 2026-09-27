-- op-engine (B): forage heights measured in a paddock with a ruler or plate meter.
-- The latest one no older than 21 days replaces the imagery estimate in the
-- grazing signals. `residual_cm` is the height left behind, when it was measured.
CREATE TABLE paddock_heights (
    id          TEXT PRIMARY KEY,                -- hgt_…
    paddock_id  TEXT NOT NULL REFERENCES paddocks(id) ON DELETE CASCADE,
    at          TEXT NOT NULL,                   -- when it was measured
    height_cm   REAL NOT NULL,
    residual_cm REAL,
    by          TEXT NOT NULL,                   -- Actor JSON: who recorded it
    created_at  TEXT NOT NULL
);
CREATE INDEX paddock_heights_paddock ON paddock_heights(paddock_id, at);
