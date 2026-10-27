-- op-ingest: a collar report stored twice (the server committed, the reply
-- was lost, the collar sent it again) keeps one copy of each fix and cue.
--
-- Only an identical record is the same record. A fix is the same when the
-- collar, its instant (unix ms) and what it measured (lon, lat, accuracy)
-- are; a cue when the collar, instant, kind as sent, level, margin and
-- position are. Two records at one instant that differ (a clock step, a
-- truncated sub-millisecond time) are both kept: ingest logs them and
-- counts them in `health.conflicts`. What the server works out for a fix
-- (fence state, margin, paddock) is not part of it, since a resend after a
-- boundary change works out differently.
--
-- Cues are held to it by a unique index. Fixes are held to it by ingest,
-- the one place that stores them (op-ingest device.rs `insert_fixes`, inside
-- the report's write transaction, looking it up by `fixes_collar_t`): a
-- unique index over 10.8 M fixes (250 collars x 30 days) took 17 s to build
-- on upgrade, longer on slower disks. Anything else that ever inserts fixes
-- must check the same way.
--
-- Protocol v2 numbers every record a collar makes, across all record types,
-- from 0 for the life of the collar (`seq`, 32-bit, never reused). A record
-- relayed twice (two lead collars heard one node) is the same record even
-- if its time differs: `collar_records` holds each (collar, seq) stored, and
-- v2 ingest stores a record only when its row goes in. One table for every
-- record type, as the numbering is; no column or index on fixes, which on a
-- large farm would mean another pass over the table now.
--
-- Large farms: duplicates are looked for only among rows that share
-- `(collar_id, t)`, found from the existing index without reading the table.

-- Records a report added that share their instant with a different record.
ALTER TABLE health ADD COLUMN conflicts INTEGER;

CREATE TABLE collar_records (
    collar_id TEXT NOT NULL,
    seq       INTEGER NOT NULL,
    -- record type (v2 registry name) and its time, unix ms
    type      TEXT NOT NULL,
    t         INTEGER NOT NULL,
    PRIMARY KEY (collar_id, seq)
) WITHOUT ROWID;

-- Identical copies go; the earliest stays.
DELETE FROM fixes WHERE id IN (
    SELECT f.id
    FROM (SELECT collar_id, t FROM fixes GROUP BY collar_id, t HAVING COUNT(*) > 1) d
    JOIN fixes f ON f.collar_id = d.collar_id AND f.t = d.t
    WHERE EXISTS (
        SELECT 1 FROM fixes g
        WHERE g.collar_id = f.collar_id AND g.t = f.t AND g.id < f.id
          AND g.lon = f.lon AND g.lat = f.lat AND g.accuracy_m = f.accuracy_m
    )
);

DELETE FROM cues WHERE id IN (
    SELECT c.id
    FROM (SELECT collar_id, t FROM cues GROUP BY collar_id, t HAVING COUNT(*) > 1) d
    JOIN cues c ON c.collar_id = d.collar_id AND c.t = d.t
    WHERE EXISTS (
        SELECT 1 FROM cues o
        WHERE o.collar_id = c.collar_id AND o.t = c.t AND o.id < c.id
          AND o.kind IS c.kind AND o.level = c.level AND o.margin_m = c.margin_m AND o.lon IS c.lon AND o.lat IS c.lat
    )
);

-- cues_collar_t (collar_id, t) stays: the rollup reads a collar's day by it in (t, id) order.
CREATE UNIQUE INDEX cues_collar_t_same ON cues(collar_id, t, IFNULL(kind, ''), level, margin_m, IFNULL(lon, 0), IFNULL(lat, 0));
