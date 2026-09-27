-- op-core: per-day paddock dwell from imported position history, kept apart
-- from the rollup's own days (the rollup never touches it), and one view
-- over both that pasture history reads.
CREATE TABLE imported_paddock_days (
    date       TEXT NOT NULL,                     -- YYYY-MM-DD, UTC
    herd_id    TEXT NOT NULL,
    collar_id  TEXT NOT NULL,
    paddock_id TEXT NOT NULL,
    fixes      INTEGER NOT NULL,
    dwell_s    REAL NOT NULL,
    last_t     INTEGER NOT NULL,
    import_id  TEXT NOT NULL,                     -- imp_…
    PRIMARY KEY (import_id, date, herd_id, collar_id, paddock_id)
);
CREATE INDEX imported_paddock_days_paddock ON imported_paddock_days(paddock_id, date);
CREATE INDEX imported_paddock_days_herd ON imported_paddock_days(herd_id, date);

CREATE VIEW paddock_days AS
    SELECT date, herd_id, collar_id, paddock_id, fixes, dwell_s, last_t FROM analytics_paddock_days
    UNION ALL
    SELECT date, herd_id, collar_id, paddock_id, fixes, dwell_s, last_t FROM imported_paddock_days;
