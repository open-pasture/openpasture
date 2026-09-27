-- op-reports: supplemental feed given to a herd, in kg of dry matter, for the
-- organic dry-matter check. `date` is the farm-local day (YYYY-MM-DD);
-- `created_by` is an Actor (JSON). No foreign key: the log outlives a herd.
CREATE TABLE feed_log (
    id         TEXT PRIMARY KEY,                 -- fed_…
    herd_id    TEXT NOT NULL,
    date       TEXT NOT NULL,
    kg_dm      REAL NOT NULL,
    kind       TEXT NOT NULL,
    note       TEXT,
    created_by TEXT NOT NULL DEFAULT '{}',
    created_at TEXT NOT NULL
);
CREATE INDEX feed_log_herd_date ON feed_log(herd_id, date);
