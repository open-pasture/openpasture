-- op-core: simulated is decided when a herd or collar is made, recorded on
-- what it produced, and never changes (`op_core::sim`).
--
-- - herds.simulated: made by the simulator (`collar-sim farm`) on a server
--   with no real herds or collars. Its history rows carry it, so a report
--   leaves the herd out for as long as it existed, whatever happens to its
--   collars or the herd later.
-- - collars.simulated (0907): linked into a simulated herd. A report never
--   sets it; a real collar never becomes simulated.
-- - fixes, cues, episodes: from a simulated collar, marked as they land.
-- - Simulated collars and animals stay in simulated herds and real ones in
--   real herds: a move across is refused here as well as by the routes.
ALTER TABLE herds ADD COLUMN simulated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE herd_history ADD COLUMN simulated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE fixes ADD COLUMN simulated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE cues ADD COLUMN simulated INTEGER NOT NULL DEFAULT 0;
ALTER TABLE episodes ADD COLUMN simulated INTEGER NOT NULL DEFAULT 0;

-- Before this, a herd counted as simulated while all its collars were.
UPDATE herds SET simulated = 1 WHERE id IN (SELECT herd_id FROM collars GROUP BY herd_id HAVING MIN(simulated) = 1);
UPDATE herd_history SET simulated = 1 WHERE herd_id IN (SELECT id FROM herds WHERE simulated = 1);
UPDATE fixes SET simulated = 1 WHERE collar_id IN (SELECT id FROM collars WHERE simulated = 1);
UPDATE cues SET simulated = 1 WHERE collar_id IN (SELECT id FROM collars WHERE simulated = 1);
UPDATE episodes SET simulated = 1 WHERE collar_id IN (SELECT id FROM collars WHERE simulated = 1);

DROP TRIGGER herd_history_insert;
DROP TRIGGER herd_history_update;
DROP TRIGGER herd_history_delete;

CREATE TRIGGER herd_history_insert AFTER INSERT ON herds
BEGIN
    INSERT INTO herd_history (herd_id, at, count, paddock_id, name, species, source, simulated)
    VALUES (NEW.id, NEW.created_at, NEW.count, NEW.paddock_id, NEW.name, NEW.species, 'created', NEW.simulated);
END;

-- The store rewrites every column on update, so only a real change counts.
CREATE TRIGGER herd_history_update AFTER UPDATE OF count, paddock_id ON herds
WHEN OLD.count IS NOT NEW.count OR OLD.paddock_id IS NOT NEW.paddock_id
BEGIN
    INSERT INTO herd_history (herd_id, at, count, paddock_id, name, species, source, simulated)
    VALUES (NEW.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), NEW.count, NEW.paddock_id, NEW.name, NEW.species, 'changed', NEW.simulated);
END;

CREATE TRIGGER herd_history_delete AFTER DELETE ON herds
BEGIN
    INSERT INTO herd_history (herd_id, at, count, paddock_id, name, species, source, simulated)
    VALUES (OLD.id, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), 0, NULL, OLD.name, OLD.species, 'deleted', OLD.simulated);
END;

CREATE TRIGGER herds_simulated_fixed BEFORE UPDATE OF simulated ON herds
WHEN NEW.simulated IS NOT OLD.simulated
BEGIN
    SELECT RAISE(ABORT, 'simulated is set when a herd is made');
END;

CREATE TRIGGER collars_simulated_fixed BEFORE UPDATE OF simulated ON collars
WHEN NEW.simulated IS NOT OLD.simulated
BEGIN
    SELECT RAISE(ABORT, 'simulated is set when a collar is linked');
END;

CREATE TRIGGER collars_simulated_herd BEFORE INSERT ON collars
WHEN NEW.simulated IS NOT COALESCE((SELECT simulated FROM herds WHERE id = NEW.herd_id), 0)
BEGIN
    SELECT RAISE(ABORT, 'simulated collars go on simulated herds and real collars on real herds');
END;

CREATE TRIGGER collars_simulated_move BEFORE UPDATE OF herd_id ON collars
WHEN NEW.simulated IS NOT COALESCE((SELECT simulated FROM herds WHERE id = NEW.herd_id), 0)
BEGIN
    SELECT RAISE(ABORT, 'simulated collars go on simulated herds and real collars on real herds');
END;

CREATE TRIGGER animals_simulated_move BEFORE UPDATE OF herd_id ON animals
WHEN COALESCE((SELECT simulated FROM herds WHERE id = OLD.herd_id), 0) IS NOT COALESCE((SELECT simulated FROM herds WHERE id = NEW.herd_id), 0)
BEGIN
    SELECT RAISE(ABORT, 'an animal stays on the simulated or real side it was made on');
END;

CREATE TRIGGER fixes_simulated AFTER INSERT ON fixes
WHEN EXISTS (SELECT 1 FROM collars WHERE id = NEW.collar_id AND simulated = 1)
BEGIN
    UPDATE fixes SET simulated = 1 WHERE id = NEW.id;
END;

CREATE TRIGGER cues_simulated AFTER INSERT ON cues
WHEN EXISTS (SELECT 1 FROM collars WHERE id = NEW.collar_id AND simulated = 1)
BEGIN
    UPDATE cues SET simulated = 1 WHERE id = NEW.id;
END;

CREATE TRIGGER episodes_simulated AFTER INSERT ON episodes
WHEN EXISTS (SELECT 1 FROM collars WHERE id = NEW.collar_id AND simulated = 1)
BEGIN
    UPDATE episodes SET simulated = 1 WHERE id = NEW.id;
END;
