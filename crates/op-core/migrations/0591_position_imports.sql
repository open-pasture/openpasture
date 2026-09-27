-- op-import (K-files): one row per committed position-history file, for the
-- list of imports, their replay and undo.
CREATE TABLE position_imports (
    id         TEXT PRIMARY KEY,                  -- imp_…
    file_name  TEXT NOT NULL,
    source     TEXT NOT NULL,                     -- csv | gpx | geojson
    zone       TEXT,                              -- IANA zone read into timestamps without an offset
    fixes      INTEGER NOT NULL,                  -- points stored (repeats of earlier imports not counted)
    animals    INTEGER NOT NULL,
    from_t     INTEGER,                           -- unix ms of the first and last stored point
    to_t       INTEGER,
    created_by TEXT NOT NULL,                     -- Actor JSON
    created_at TEXT NOT NULL
);
CREATE INDEX position_imports_created ON position_imports(created_at);
