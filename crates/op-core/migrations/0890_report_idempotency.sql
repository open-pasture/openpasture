-- op-ingest: a collar report stored twice (the server committed, the reply
-- was lost, the collar sent it again) keeps one copy of each fix and cue.
--
-- A fix is one collar at one instant (unix ms): `(collar_id, t)`.
-- A cue is one collar, one instant and one kind (warn | outside). The kind
-- is part of the key because a collar can warn and cue outside on the same
-- fix; two cues of the same kind at the same millisecond can't both be real.
-- Level, ring and margin are not: a resent cue carries the same ones. A cue
-- from before protocol v1 has no kind; its kind is what readers derive
-- (outside past the line, else warn), as 0483 says.
--
-- `seq` is the collar's own sequence number for a fix or cue (protocol v2):
-- 32-bit, increasing for the life of the collar, never reused. When present
-- a row is also unique by `(collar_id, seq)`, which catches the same record
-- relayed twice (two lead collars heard one node) even if its time differs.

-- Earlier copies win; duplicates found by the (collar_id, t) indexes.
DELETE FROM fixes
WHERE EXISTS (SELECT 1 FROM fixes f WHERE f.collar_id = fixes.collar_id AND f.t = fixes.t AND f.id < fixes.id);

DELETE FROM cues
WHERE EXISTS (
    SELECT 1 FROM cues c
    WHERE c.collar_id = cues.collar_id AND c.t = cues.t AND c.id < cues.id
      AND COALESCE(c.kind, CASE WHEN c.margin_m < 0 THEN 'outside' ELSE 'warn' END)
        = COALESCE(cues.kind, CASE WHEN cues.margin_m < 0 THEN 'outside' ELSE 'warn' END)
);

DROP INDEX IF EXISTS fixes_collar_t;
CREATE UNIQUE INDEX fixes_collar_t ON fixes(collar_id, t);

-- cues_collar_t (collar_id, t) stays: the rollup reads a collar's day by it in (t, id) order.
CREATE UNIQUE INDEX cues_collar_t_kind ON cues(collar_id, t, COALESCE(kind, CASE WHEN margin_m < 0 THEN 'outside' ELSE 'warn' END));

ALTER TABLE fixes ADD COLUMN seq INTEGER;
ALTER TABLE cues ADD COLUMN seq INTEGER;
CREATE UNIQUE INDEX fixes_collar_seq ON fixes(collar_id, seq) WHERE seq IS NOT NULL;
CREATE UNIQUE INDEX cues_collar_seq ON cues(collar_id, seq) WHERE seq IS NOT NULL;
