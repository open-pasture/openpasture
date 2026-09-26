-- op-engine: land report cache and the farm's own lessons.

-- One row per fetched land report. `cache_key` covers geometry and requested
-- sections; freshness is matched on `as_of`.
CREATE TABLE land_reports (
    id          TEXT PRIMARY KEY,                 -- lr_…
    paddock_id  TEXT,
    cache_key   TEXT NOT NULL,
    source      TEXT NOT NULL,                    -- alexandria | open_data
    as_of       TEXT NOT NULL,
    report      TEXT NOT NULL,                    -- JSON: { report_id, source, as_of, geometry, sections }
    created_at  TEXT NOT NULL
);
CREATE INDEX land_reports_key ON land_reports(cache_key, as_of);
CREATE INDEX land_reports_paddock ON land_reports(paddock_id, as_of);

-- Lessons learned on this farm (farmer notes on decisions, outcomes). Indexed
-- for knowledge search next to the seed corpus.
CREATE TABLE lessons (
    id          TEXT PRIMARY KEY,                 -- les_…
    title       TEXT NOT NULL,
    body        TEXT NOT NULL,
    kind        TEXT NOT NULL,                    -- lesson | outcome | farmer
    source      TEXT NOT NULL,                    -- e.g. decision dec_…
    decision_id TEXT,
    paddock_id  TEXT,
    created_at  TEXT NOT NULL
);
CREATE INDEX lessons_created ON lessons(created_at);
