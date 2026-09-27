-- FX-DATA (op-engine rest days and last grazed): a paddock is grazed on a
-- day when a real share of a herd's tracked day is in it (op-analytics'
-- pasture rule, 1/24), not when one fix lands there. A cow lying along a
-- fence puts some fixes across it every hour; 250 collars make that
-- certain. Two small per-day summaries, kept by triggers, so rest days read
-- a few rows per paddock and day instead of the collar days or the fixes.
--
-- Hot fixes: per herd, UTC day and paddock ('' = outside every paddock),
-- how many fixes and the newest. It replaces X1's fix_paddock_last (one row
-- per herd and paddock, any single fix): the same one small UPSERT per fix.
-- Rows stay after the rollup deletes the fixes (a few per herd and day).
CREATE TABLE fix_paddock_days (
    herd_id    TEXT NOT NULL,
    day        INTEGER NOT NULL,                  -- unix ms / 86,400,000 (UTC)
    paddock_id TEXT NOT NULL,
    fixes      INTEGER NOT NULL,
    last_t     INTEGER NOT NULL,                  -- unix ms of the newest fix
    PRIMARY KEY (herd_id, day, paddock_id)
) WITHOUT ROWID;
CREATE INDEX fix_paddock_days_paddock ON fix_paddock_days(paddock_id, day);

INSERT INTO fix_paddock_days (herd_id, day, paddock_id, fixes, last_t)
SELECT herd_id, t / 86400000, COALESCE(paddock_id, ''), COUNT(*), MAX(t) FROM fixes
WHERE herd_id IS NOT NULL AND herd_id != ''
GROUP BY herd_id, t / 86400000, COALESCE(paddock_id, '');

DROP TRIGGER IF EXISTS fix_paddock_last_insert;
DROP TABLE IF EXISTS fix_paddock_last;

CREATE TRIGGER fix_paddock_days_insert AFTER INSERT ON fixes WHEN NEW.herd_id IS NOT NULL AND NEW.herd_id != ''
BEGIN
    INSERT INTO fix_paddock_days (herd_id, day, paddock_id, fixes, last_t) VALUES (NEW.herd_id, NEW.t / 86400000, COALESCE(NEW.paddock_id, ''), 1, NEW.t)
    ON CONFLICT (herd_id, day, paddock_id) DO UPDATE SET fixes = fixes + 1, last_t = MAX(last_t, excluded.last_t);
END;

-- Rolled-up and imported collar days (the paddock_days view's two tables),
-- summed over collars: per herd, UTC date, paddock and source, the dwell and
-- the newest fix. A day the rollup redoes, or an import undone, takes its
-- rows off again.
CREATE TABLE paddock_day_dwell (
    herd_id    TEXT NOT NULL,
    date       TEXT NOT NULL,                     -- YYYY-MM-DD, UTC
    paddock_id TEXT NOT NULL,
    source     TEXT NOT NULL,                     -- rolled | imported
    dwell_s    REAL NOT NULL,
    last_t     INTEGER NOT NULL,
    PRIMARY KEY (herd_id, date, paddock_id, source)
) WITHOUT ROWID;
CREATE INDEX paddock_day_dwell_paddock ON paddock_day_dwell(paddock_id, date);

INSERT INTO paddock_day_dwell (herd_id, date, paddock_id, source, dwell_s, last_t)
SELECT herd_id, date, paddock_id, 'rolled', SUM(dwell_s), MAX(last_t) FROM analytics_paddock_days GROUP BY herd_id, date, paddock_id;
INSERT INTO paddock_day_dwell (herd_id, date, paddock_id, source, dwell_s, last_t)
SELECT herd_id, date, paddock_id, 'imported', SUM(dwell_s), MAX(last_t) FROM imported_paddock_days GROUP BY herd_id, date, paddock_id;

CREATE TRIGGER paddock_day_dwell_rolled AFTER INSERT ON analytics_paddock_days
BEGIN
    INSERT INTO paddock_day_dwell (herd_id, date, paddock_id, source, dwell_s, last_t) VALUES (NEW.herd_id, NEW.date, NEW.paddock_id, 'rolled', NEW.dwell_s, NEW.last_t)
    ON CONFLICT (herd_id, date, paddock_id, source) DO UPDATE SET dwell_s = dwell_s + excluded.dwell_s, last_t = MAX(last_t, excluded.last_t);
END;

CREATE TRIGGER paddock_day_dwell_rolled_gone AFTER DELETE ON analytics_paddock_days
BEGIN
    UPDATE paddock_day_dwell SET dwell_s = dwell_s - OLD.dwell_s
    WHERE herd_id = OLD.herd_id AND date = OLD.date AND paddock_id = OLD.paddock_id AND source = 'rolled';
    DELETE FROM paddock_day_dwell
    WHERE herd_id = OLD.herd_id AND date = OLD.date AND paddock_id = OLD.paddock_id AND source = 'rolled'
      AND NOT EXISTS (SELECT 1 FROM analytics_paddock_days a WHERE a.herd_id = OLD.herd_id AND a.date = OLD.date AND a.paddock_id = OLD.paddock_id);
END;

CREATE TRIGGER paddock_day_dwell_imported AFTER INSERT ON imported_paddock_days
BEGIN
    INSERT INTO paddock_day_dwell (herd_id, date, paddock_id, source, dwell_s, last_t) VALUES (NEW.herd_id, NEW.date, NEW.paddock_id, 'imported', NEW.dwell_s, NEW.last_t)
    ON CONFLICT (herd_id, date, paddock_id, source) DO UPDATE SET dwell_s = dwell_s + excluded.dwell_s, last_t = MAX(last_t, excluded.last_t);
END;

CREATE TRIGGER paddock_day_dwell_imported_gone AFTER DELETE ON imported_paddock_days
BEGIN
    UPDATE paddock_day_dwell SET dwell_s = dwell_s - OLD.dwell_s
    WHERE herd_id = OLD.herd_id AND date = OLD.date AND paddock_id = OLD.paddock_id AND source = 'imported';
    DELETE FROM paddock_day_dwell
    WHERE herd_id = OLD.herd_id AND date = OLD.date AND paddock_id = OLD.paddock_id AND source = 'imported'
      AND NOT EXISTS (SELECT 1 FROM imported_paddock_days i WHERE i.herd_id = OLD.herd_id AND i.date = OLD.date AND i.paddock_id = OLD.paddock_id);
END;
