-- op-analytics (G): where fixes land and how good they are, per herd per UTC
-- day per 10 m cell, from op-ingest's `fixes`. Cells are counted east (cx)
-- and north (cy) from `coverage_grid`'s origin, which is set once, so a
-- cell means the same ground on every day.
CREATE TABLE coverage_grid (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    lon0       REAL NOT NULL,
    lat0       REAL NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE coverage_days (
    date     TEXT NOT NULL,                   -- YYYY-MM-DD, UTC
    herd_id  TEXT NOT NULL,                   -- '' when the fix had no herd
    cx       INTEGER NOT NULL,
    cy       INTEGER NOT NULL,
    n        INTEGER NOT NULL,                -- fixes with an accuracy
    acc_hist TEXT NOT NULL,                   -- JSON, 8 counts: <1, <2, <3, <5, <8, <12, <20, >=20 m
    expected INTEGER NOT NULL,                -- fixes the collars' cadence called for
    got      INTEGER NOT NULL,                -- fixes that arrived
    PRIMARY KEY (date, herd_id, cx, cy)
) WITHOUT ROWID;
