-- op-core: map features. Exclusions become boundary holes while active;
-- water, gates, shade, hazards, roads, neighbour lines and the farm boundary
-- are checked before a send. paddock_id NULL = farm-wide.
CREATE TABLE features (
    id           TEXT PRIMARY KEY,                -- fea_…
    kind         TEXT NOT NULL,                   -- exclusion | water | gate | shade | hazard | road | neighbour_line | farm_boundary
    name         TEXT,
    geometry     TEXT NOT NULL,                   -- GeoJSON Point | LineString | Polygon
    paddock_id   TEXT REFERENCES paddocks(id) ON DELETE CASCADE,
    notes        TEXT,
    props        TEXT NOT NULL DEFAULT '{}',
    active_from  TEXT,
    active_until TEXT,
    created_at   TEXT NOT NULL,
    updated_at   TEXT NOT NULL
);
CREATE INDEX features_kind ON features(kind);
CREATE INDEX features_paddock ON features(paddock_id);
CREATE UNIQUE INDEX features_one_farm_boundary ON features(kind) WHERE kind = 'farm_boundary';
