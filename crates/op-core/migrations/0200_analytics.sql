-- op-analytics. Range scans by time for the Parquet rollup and exports.
CREATE INDEX IF NOT EXISTS fixes_t ON fixes(t);
CREATE INDEX IF NOT EXISTS cues_t ON cues(t);
CREATE INDEX IF NOT EXISTS acks_at ON acks(at);

-- Battery history. `collars.battery` only holds the latest value.
CREATE TABLE analytics_battery (
    collar_id TEXT NOT NULL,
    t         INTEGER NOT NULL,               -- unix ms
    battery   REAL NOT NULL,
    PRIMARY KEY (collar_id, t)
) WITHOUT ROWID;

-- Per day dwell of each collar in each paddock, written when a day of fixes
-- moves to Parquet so pasture history needs no Parquet scan. paddock_id ''
-- is outside every paddock, herd_id '' is unknown.
CREATE TABLE analytics_paddock_days (
    date       TEXT NOT NULL,                 -- YYYY-MM-DD, UTC
    herd_id    TEXT NOT NULL,
    collar_id  TEXT NOT NULL,
    paddock_id TEXT NOT NULL,
    fixes      INTEGER NOT NULL,
    dwell_s    REAL NOT NULL,
    last_t     INTEGER NOT NULL,
    PRIMARY KEY (date, herd_id, collar_id, paddock_id)
);
CREATE INDEX analytics_paddock_days_paddock ON analytics_paddock_days(paddock_id, date);
