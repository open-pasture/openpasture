-- X1 (op-engine rest days): the newest fix of each herd in each paddock, kept
-- by a trigger as fixes land, so "last grazed" reads one row per paddock
-- instead of the herd's hot fixes (250 collars at a fix every 5 s are 4.3 M
-- rows a day, hot for several days). paddock_id '' is a fix outside every
-- paddock when it landed, herd_id '' one without a herd. Rows only move
-- forward: the rollup's deletes leave them, and rolled days are in
-- analytics_paddock_days as well.
CREATE TABLE fix_paddock_last (
    herd_id    TEXT NOT NULL,
    paddock_id TEXT NOT NULL,
    last_t     INTEGER NOT NULL,                  -- unix ms of the newest fix
    PRIMARY KEY (herd_id, paddock_id)
) WITHOUT ROWID;

INSERT INTO fix_paddock_last (herd_id, paddock_id, last_t)
SELECT COALESCE(herd_id, ''), COALESCE(paddock_id, ''), MAX(t) FROM fixes GROUP BY COALESCE(herd_id, ''), COALESCE(paddock_id, '');

CREATE TRIGGER fix_paddock_last_insert AFTER INSERT ON fixes
BEGIN
    INSERT INTO fix_paddock_last (herd_id, paddock_id, last_t) VALUES (COALESCE(NEW.herd_id, ''), COALESCE(NEW.paddock_id, ''), NEW.t)
    ON CONFLICT (herd_id, paddock_id) DO UPDATE SET last_t = excluded.last_t WHERE excluded.last_t > fix_paddock_last.last_t;
END;
