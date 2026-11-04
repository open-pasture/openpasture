-- op-ingest: app-facing collar telemetry the report already carries.
-- charging and signal (LTE RSRP, dBm) from health; last_cue_at from cues.
-- NULL until the collar reports them; never invent a default.
ALTER TABLE collars ADD COLUMN charging INTEGER;             -- 0 | 1 when reported
ALTER TABLE collars ADD COLUMN signal REAL;                  -- cell RSRP, dBm
ALTER TABLE collars ADD COLUMN last_cue_at TEXT;             -- RFC 3339 of newest stored cue
