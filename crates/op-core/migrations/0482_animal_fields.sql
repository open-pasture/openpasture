-- op-core: animal records: electronic ID, breed, sex, birth date, notes, and
-- removal (sold, died, culled, moved off) kept for the record.
ALTER TABLE animals ADD COLUMN eid TEXT;                     -- 15 digits
ALTER TABLE animals ADD COLUMN breed TEXT;
ALTER TABLE animals ADD COLUMN sex TEXT;                     -- female | male | castrated
ALTER TABLE animals ADD COLUMN born TEXT;                    -- YYYY-MM-DD
ALTER TABLE animals ADD COLUMN notes TEXT;
ALTER TABLE animals ADD COLUMN removed_at TEXT;
ALTER TABLE animals ADD COLUMN removed_reason TEXT;          -- sold | died | culled | moved_off
CREATE INDEX animals_herd_tag ON animals(herd_id, tag);
CREATE UNIQUE INDEX animals_eid ON animals(eid) WHERE eid IS NOT NULL;
