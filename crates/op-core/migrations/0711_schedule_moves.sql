-- S: each open (step 0) and back-fence close step (1..) of a schedule, with
-- the time it takes effect and the boundary that carries it once staged.
-- `occurrence` is an open's place in the cadence (occurrence 0 is the
-- schedule's `starts_at`). Held occurrences are rows with skipped = 'held'.
CREATE TABLE schedule_moves (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    schedule_id      TEXT NOT NULL REFERENCES schedules(id) ON DELETE CASCADE,
    strip            INTEGER NOT NULL,
    step             INTEGER NOT NULL,
    occurrence       INTEGER,
    at               TEXT NOT NULL,
    geometry         TEXT NOT NULL,             -- Polygon JSON as planned, before prepare
    state            TEXT NOT NULL,             -- planned | staged | done | skipped
    skipped          TEXT,                      -- late | skipped | held
    boundary_id      TEXT,
    boundary_version INTEGER,
    applied_at       TEXT,                      -- the first collar's apply time (acks)
    updated_at       TEXT NOT NULL
);
CREATE INDEX schedule_moves_at ON schedule_moves(schedule_id, at);
CREATE INDEX schedule_moves_version ON schedule_moves(boundary_version) WHERE boundary_version IS NOT NULL;
