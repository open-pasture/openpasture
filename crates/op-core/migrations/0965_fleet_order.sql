-- op-fleet, after review: which test run counts, unit generations, the
-- typo check on claims, and firmware offers that can be retried.

-- A unit's generation goes up when it is sent back (RMA), retired or gets
-- a new device key. Only runs of its current generation can make it
-- eligible for provisioning: a repaired unit needs a fresh pass.
-- `claim_checked`: the claim code was checked (this server drew it, or it
-- matched a factory record's claim check). A code from a card with no
-- record is taken at its word until the collar reports.
ALTER TABLE fleet_units ADD COLUMN generation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE fleet_units ADD COLUMN generation_at TEXT;               -- server time it began
ALTER TABLE fleet_units ADD COLUMN claim_checked INTEGER NOT NULL DEFAULT 0;
UPDATE fleet_units SET claim_checked = 1 WHERE origin = 'station';

-- `seq`: the server's order, given inside the write that stores the run,
-- so "latest" never depends on a station's clock or on delivery delays.
-- `run_key`: the station's own id for a run; a retry with the same key is
-- the same run. `counted = 0`: a pass that arrived after a run the station
-- finished later (a delayed retry) or that finished before the unit's
-- generation began; it is kept and shown, but never makes the unit eligible.
ALTER TABLE fleet_test_runs ADD COLUMN seq INTEGER NOT NULL DEFAULT 0;
ALTER TABLE fleet_test_runs ADD COLUMN run_key TEXT;
ALTER TABLE fleet_test_runs ADD COLUMN generation INTEGER NOT NULL DEFAULT 0;
ALTER TABLE fleet_test_runs ADD COLUMN counted INTEGER NOT NULL DEFAULT 1;
UPDATE fleet_test_runs SET seq = rowid;
CREATE UNIQUE INDEX fleet_test_runs_seq ON fleet_test_runs(seq);
CREATE UNIQUE INDEX fleet_test_runs_key ON fleet_test_runs(serial, run_key) WHERE run_key IS NOT NULL;
CREATE INDEX fleet_test_runs_latest ON fleet_test_runs(serial, generation, seq);

-- A firmware offer: its id, how many times it went out without an answer,
-- and when it may go again.
ALTER TABLE fleet_rollout_units ADD COLUMN offer_id TEXT;
ALTER TABLE fleet_rollout_units ADD COLUMN attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE fleet_rollout_units ADD COLUMN next_try_at TEXT;
