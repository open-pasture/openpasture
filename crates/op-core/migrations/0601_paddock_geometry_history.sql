-- op-reports: each paddock's shape, area and name over time, written by
-- triggers on `paddocks` (no Rust hooks). Reports use the area a paddock had
-- when it was grazed, and a deleted paddock keeps its name in old records.
--
-- source: created | changed | deleted (triggers), backfill (each paddock as it
-- stood on install day, dated from its creation).
CREATE TABLE paddock_geometry_history (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    paddock_id TEXT NOT NULL,
    at         TEXT NOT NULL,
    geometry   TEXT NOT NULL,
    area_ha    REAL NOT NULL,
    name       TEXT NOT NULL,
    source     TEXT NOT NULL
);
CREATE INDEX paddock_geometry_history_paddock ON paddock_geometry_history(paddock_id, at);

INSERT INTO paddock_geometry_history (paddock_id, at, geometry, area_ha, name, source)
SELECT id, created_at, geometry, area_ha, name, 'backfill' FROM paddocks;

CREATE TRIGGER paddock_geometry_history_insert AFTER INSERT ON paddocks
BEGIN
    INSERT INTO paddock_geometry_history (paddock_id, at, geometry, area_ha, name, source)
    VALUES (NEW.id, NEW.created_at, NEW.geometry, NEW.area_ha, NEW.name, 'created');
END;

-- The store rewrites every column on update, so only a real change counts.
CREATE TRIGGER paddock_geometry_history_update AFTER UPDATE OF geometry, name ON paddocks
WHEN OLD.geometry IS NOT NEW.geometry OR OLD.name IS NOT NEW.name
BEGIN
    INSERT INTO paddock_geometry_history (paddock_id, at, geometry, area_ha, name, source)
    VALUES (NEW.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.geometry, NEW.area_ha, NEW.name, 'changed');
END;

CREATE TRIGGER paddock_geometry_history_delete AFTER DELETE ON paddocks
BEGIN
    INSERT INTO paddock_geometry_history (paddock_id, at, geometry, area_ha, name, source)
    VALUES (OLD.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), OLD.geometry, OLD.area_ha, OLD.name, 'deleted');
END;
