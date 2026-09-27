-- FX-DATA (op-analytics welfare learning status): per animal, how many of its
-- episodes were added or taken off after they had settled (began more than
-- six days before). The herd's learning status keeps each animal's settled
-- episodes folded and reads only the days since; a change here tells it to
-- fold that animal again. The server rebuilds derived episodes over the last
-- five days only, so in practice this is a collar uploading old ones late.
CREATE TABLE welfare_late (
    animal_id TEXT PRIMARY KEY,
    changes   INTEGER NOT NULL
) WITHOUT ROWID;

CREATE TRIGGER welfare_late_insert AFTER INSERT ON episodes
WHEN NEW.animal_id IS NOT NULL AND NEW.start_t < CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) - 518400000
BEGIN
    INSERT INTO welfare_late (animal_id, changes) VALUES (NEW.animal_id, 1) ON CONFLICT (animal_id) DO UPDATE SET changes = changes + 1;
END;

CREATE TRIGGER welfare_late_delete AFTER DELETE ON episodes
WHEN OLD.animal_id IS NOT NULL AND OLD.start_t < CAST((julianday('now') - 2440587.5) * 86400000.0 AS INTEGER) - 518400000
BEGIN
    INSERT INTO welfare_late (animal_id, changes) VALUES (OLD.animal_id, 1) ON CONFLICT (animal_id) DO UPDATE SET changes = changes + 1;
END;
