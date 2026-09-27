-- op-core: what a collar reports about itself (firmware, capabilities,
-- limits), since when it has been outside, and whether it is parked
-- (charging, on the shelf, in repair). `limits` is JSON read by op-ingest.
ALTER TABLE collars ADD COLUMN fw TEXT;
ALTER TABLE collars ADD COLUMN caps TEXT;                    -- JSON array of strings
ALTER TABLE collars ADD COLUMN limits TEXT;                  -- JSON CollarLimits
ALTER TABLE collars ADD COLUMN outside_since TEXT;
ALTER TABLE collars ADD COLUMN parked_at TEXT;
ALTER TABLE collars ADD COLUMN parked_reason TEXT;           -- charging | shelf | repair
