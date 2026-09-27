-- op-import (K-files): position history imported from files (CSV, GPX,
-- GeoJSON), kept apart from collar fixes. One row per animal per instant, so
-- importing the same file twice stores each point once. Day summaries go to
-- imported_paddock_days (0489); the telemetry rollup never touches either.
CREATE TABLE imported_fixes (
    import_id  TEXT NOT NULL,                     -- imp_…
    animal_id  TEXT NOT NULL,
    herd_id    TEXT NOT NULL,                     -- the animal's herd at import
    t          INTEGER NOT NULL,                  -- unix ms
    at         TEXT NOT NULL,                     -- the same instant, RFC 3339 UTC
    lon        REAL NOT NULL,
    lat        REAL NOT NULL,
    accuracy_m REAL,
    source     TEXT NOT NULL,                     -- csv | gpx | geojson
    PRIMARY KEY (animal_id, t)
) WITHOUT ROWID;
CREATE INDEX imported_fixes_import ON imported_fixes(import_id);
