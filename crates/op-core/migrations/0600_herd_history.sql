-- op-reports: where each herd was and how many head it had, over time.
-- Written by triggers on `herds` (no Rust hooks), so every path that creates,
-- moves, recounts or deletes a herd leaves a row. `name` and `species` are the
-- herd's at that time, so records outlive a deleted herd.
--
-- source: created | changed | deleted (triggers), backfill (moves recorded
-- before this migration, from applied MOVE decisions, counted at the herd's
-- count on install day), install (each herd as it stood on install day).
CREATE TABLE herd_history (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    herd_id    TEXT NOT NULL,
    at         TEXT NOT NULL,
    count      INTEGER NOT NULL,
    paddock_id TEXT,
    name       TEXT NOT NULL,
    species    TEXT NOT NULL,
    source     TEXT NOT NULL
);
CREATE INDEX herd_history_herd ON herd_history(herd_id, at);
CREATE INDEX herd_history_paddock ON herd_history(paddock_id, at);

-- Backfill 1: where each herd started. A herd that moved started where its
-- first applied move says it came from (unknown for farmer-drawn moves); one
-- that never moved has always been where it is now.
INSERT INTO herd_history (herd_id, at, count, paddock_id, name, species, source)
SELECT h.id, h.created_at, h.count,
       CASE WHEN EXISTS (SELECT 1 FROM decisions d
                         WHERE d.herd_id = h.id AND d.status = 'applied' AND d.action = 'MOVE' AND d.to_paddock_id IS NOT NULL)
            THEN (SELECT json_extract(d.inputs, '$.from_paddock_id') FROM decisions d
                  WHERE d.herd_id = h.id AND d.status = 'applied' AND d.action = 'MOVE' AND d.to_paddock_id IS NOT NULL
                  ORDER BY COALESCE(d.responded_at, d.created_at), d.id LIMIT 1)
            ELSE h.paddock_id END,
       h.name, h.species, 'backfill'
FROM herds h;

-- Backfill 2: every applied move, at the time it was applied (the activity
-- log's decision.applied entry, else the farmer's response, else creation).
INSERT INTO herd_history (herd_id, at, count, paddock_id, name, species, source)
SELECT d.herd_id,
       COALESCE((SELECT MIN(e.occurred_at) FROM events e JOIN event_targets t ON t.event_id = e.id
                 WHERE e.kind = 'decision.applied' AND t.target_type = 'decision' AND t.target_id = d.id),
                d.responded_at, d.created_at),
       h.count, d.to_paddock_id, h.name, h.species, 'backfill'
FROM decisions d JOIN herds h ON h.id = d.herd_id
WHERE d.status = 'applied' AND d.action = 'MOVE' AND d.to_paddock_id IS NOT NULL
ORDER BY 2, d.id;

-- Backfill 3: each herd as it stands now, so a move made by hand (no
-- decision) shows from today and the report can say when history starts.
INSERT INTO herd_history (herd_id, at, count, paddock_id, name, species, source)
SELECT h.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), h.count, h.paddock_id, h.name, h.species, 'install'
FROM herds h;

CREATE TRIGGER herd_history_insert AFTER INSERT ON herds
BEGIN
    INSERT INTO herd_history (herd_id, at, count, paddock_id, name, species, source)
    VALUES (NEW.id, NEW.created_at, NEW.count, NEW.paddock_id, NEW.name, NEW.species, 'created');
END;

-- The store rewrites every column on update, so only a real change counts.
CREATE TRIGGER herd_history_update AFTER UPDATE OF count, paddock_id ON herds
WHEN OLD.count IS NOT NEW.count OR OLD.paddock_id IS NOT NEW.paddock_id
BEGIN
    INSERT INTO herd_history (herd_id, at, count, paddock_id, name, species, source)
    VALUES (NEW.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.count, NEW.paddock_id, NEW.name, NEW.species, 'changed');
END;

CREATE TRIGGER herd_history_delete AFTER DELETE ON herds
BEGIN
    INSERT INTO herd_history (herd_id, at, count, paddock_id, name, species, source)
    VALUES (OLD.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 0, NULL, OLD.name, OLD.species, 'deleted');
END;
