-- op-ingest: the collar endpoint (collar address + /collar/v1) a collar is
-- known to use: set when it reports holding a config that carried it, within
-- a day of that config going out (after a day of failing there the collar
-- goes back to its old one). NULL: not known (no config yet, or a collar that
-- takes none). Settings, Server lists collars still on an older address, so
-- that address is kept working until they have moved.
ALTER TABLE collars ADD COLUMN endpoint TEXT;
