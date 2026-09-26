-- op-ingest: moves. A target the herd is swept into, one active boundary step
-- at a time. `sweep` is the driver's own state (JSON): the sweep frame, the
-- back line, the last step time and each animal's progress.
CREATE TABLE moves (
    id           TEXT PRIMARY KEY,               -- mov_…
    herd_id      TEXT NOT NULL,
    decision_id  TEXT NOT NULL,
    target       TEXT NOT NULL,                  -- Polygon JSON
    status       TEXT NOT NULL,                  -- sweeping | done | stopped
    step         INTEGER NOT NULL DEFAULT 0,
    remaining_m  REAL NOT NULL DEFAULT 0,
    stragglers   TEXT NOT NULL DEFAULT '[]',     -- collar ids, JSON
    warn_m       REAL NOT NULL,
    hysteresis_m REAL NOT NULL,
    sweep        TEXT NOT NULL DEFAULT '{}',
    started_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
CREATE INDEX moves_herd ON moves(herd_id, started_at);
-- At most one running move per herd.
CREATE UNIQUE INDEX moves_one_sweeping ON moves(herd_id) WHERE status = 'sweeping';
