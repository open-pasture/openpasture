-- op-core: a collar run by the simulator (`simulation/collar-sim`). Set when
-- the collar is linked as simulated or reports the `sim` cap, and never
-- cleared: what a simulated collar recorded stays marked. Reports leave
-- simulated collars out; alerts about them are not sent to anyone.
ALTER TABLE collars ADD COLUMN simulated INTEGER NOT NULL DEFAULT 0;
