-- op-ingest: escapes. An animal that stays outside its herd's boundary gets a
-- boundary of its own: its herd's boundary joined to a pen around it, which
-- closes in behind it until it is back. The rest of the herd keeps the herd's
-- boundary, so nobody follows it out. `pen` is the driver's own state (JSON).
CREATE TABLE escapes (
    id           TEXT PRIMARY KEY,               -- esc_…
    herd_id      TEXT NOT NULL,
    collar_id    TEXT NOT NULL,
    status       TEXT NOT NULL,                  -- returning | back | stopped
    boundary_id  TEXT,                           -- the collar's own boundary now
    step         INTEGER NOT NULL DEFAULT 0,
    remaining_m  REAL NOT NULL DEFAULT 0,
    pen          TEXT NOT NULL DEFAULT '{}',
    started_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL,
    ended_at     TEXT
);
CREATE INDEX escapes_herd ON escapes(herd_id, started_at);
CREATE INDEX escapes_collar ON escapes(collar_id, started_at);
-- At most one open escape per collar.
CREATE UNIQUE INDEX escapes_one_returning ON escapes(collar_id) WHERE status = 'returning';

-- A boundary with `collar_id` belongs to that collar alone (an escape), not
-- the herd. `copy_of` is the herd version whose shape it carries, when a
-- collar is handed back to its herd's boundary.
ALTER TABLE boundaries ADD COLUMN collar_id TEXT;
ALTER TABLE boundaries ADD COLUMN copy_of INTEGER;
CREATE INDEX boundaries_collar ON boundaries(collar_id, version) WHERE collar_id IS NOT NULL;
