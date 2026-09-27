-- op-analytics (G): what the day aggregator has read. `analytics_day_marks`
-- holds the highest row id taken from each raw table, so a run only redoes
-- the days new rows touched; `analytics_days` holds, per aggregated day, the
-- highest id it included and the Parquet file it read, so a day file that
-- gains late rows is redone too.
CREATE TABLE analytics_day_marks (
    source     TEXT PRIMARY KEY,              -- fixes | health
    max_id     INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);

CREATE TABLE analytics_days (
    source     TEXT NOT NULL,                 -- fixes | health
    date       TEXT NOT NULL,                 -- YYYY-MM-DD, UTC
    max_id     INTEGER NOT NULL,
    file_mtime INTEGER,                       -- unix ms of the day file read, if any
    updated_at TEXT NOT NULL,
    PRIMARY KEY (source, date)
) WITHOUT ROWID;
