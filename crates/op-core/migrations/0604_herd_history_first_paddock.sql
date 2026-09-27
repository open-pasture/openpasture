-- op-reports: where a backfilled herd started when its first move didn't say.
-- Farmer-drawn moves record no from_paddock_id, but moving the herd on the
-- record marks the paddock it left `grazed_until` at that moment. So a herd's
-- first backfill row with no paddock takes the one paddock (other than the
-- move's target) whose grazed_until falls within a minute after that move.
-- Nothing changes when no paddock or more than one fits (a later move out of
-- the same paddock overwrote it, or two herds moved at once).
WITH first AS (
    SELECT hh.id,
           (SELECT n.at FROM herd_history n WHERE n.herd_id = hh.herd_id AND n.id > hh.id AND n.source = 'backfill' ORDER BY n.id LIMIT 1) AS moved_at,
           (SELECT n.paddock_id FROM herd_history n WHERE n.herd_id = hh.herd_id AND n.id > hh.id AND n.source = 'backfill' ORDER BY n.id LIMIT 1) AS moved_to
    FROM herd_history hh
    WHERE hh.source = 'backfill' AND hh.paddock_id IS NULL
      AND hh.id = (SELECT MIN(m.id) FROM herd_history m WHERE m.herd_id = hh.herd_id)
),
left_behind AS (
    SELECT f.id, p.id AS paddock_id
    FROM first f JOIN paddocks p
      ON f.moved_at IS NOT NULL AND p.grazed_until IS NOT NULL AND p.id IS NOT f.moved_to
     AND julianday(p.grazed_until) BETWEEN julianday(f.moved_at) AND julianday(f.moved_at) + 60.0 / 86400.0
)
UPDATE herd_history SET paddock_id = (SELECT l.paddock_id FROM left_behind l WHERE l.id = herd_history.id)
WHERE id IN (SELECT id FROM left_behind GROUP BY id HAVING COUNT(*) = 1);
