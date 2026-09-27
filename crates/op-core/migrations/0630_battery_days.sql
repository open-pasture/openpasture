-- op-analytics (G): battery per collar per UTC day, from op-ingest's `health`,
-- so fleet trends never read raw telemetry. The day aggregator rewrites a
-- day's rows whenever new readings land in it.
CREATE TABLE battery_days (
    collar_id TEXT NOT NULL,
    date      TEXT NOT NULL,                  -- YYYY-MM-DD, UTC
    min       REAL NOT NULL,                  -- battery, 0-1
    max       REAL NOT NULL,
    mean      REAL NOT NULL,
    last      REAL NOT NULL,                  -- the day's latest reading
    n         INTEGER NOT NULL,               -- readings
    first_t   INTEGER NOT NULL,               -- unix ms of the first and last reading
    last_t    INTEGER NOT NULL,
    PRIMARY KEY (collar_id, date)
) WITHOUT ROWID;
CREATE INDEX battery_days_date ON battery_days(date);
