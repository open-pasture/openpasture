-- S: strip schedules. A herd walked across a paddock's strips on a cadence;
-- the strips are copied (a layout re-cuts when its paddock is reshaped).
-- Cadence and back fence are JSON (op_core::schedule). At most one schedule
-- per herd is running (active or paused).
CREATE TABLE schedules (
    id          TEXT PRIMARY KEY,               -- sch_…
    herd_id     TEXT NOT NULL REFERENCES herds(id) ON DELETE CASCADE,
    paddock_id  TEXT NOT NULL,
    layout_id   TEXT,
    strips      TEXT NOT NULL,                  -- [Polygon] JSON
    next_index  INTEGER NOT NULL DEFAULT 0,
    cadence     TEXT NOT NULL,                  -- Cadence JSON
    starts_at   TEXT NOT NULL,
    back_fence  TEXT NOT NULL,                  -- BackFence JSON
    status      TEXT NOT NULL,                  -- active | paused | done
    planned_end TEXT,                           -- the plan's end when made (reports)
    created_by  TEXT NOT NULL,                  -- Actor JSON
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    ended_at    TEXT
);
CREATE INDEX schedules_herd ON schedules(herd_id, created_at);
CREATE INDEX schedules_paddock ON schedules(paddock_id, created_at);
CREATE UNIQUE INDEX schedules_one_running ON schedules(herd_id) WHERE status != 'done';
