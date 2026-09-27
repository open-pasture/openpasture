-- op-core: receiver, cell, motion and power health from protocol v1 reports.
ALTER TABLE health ADD COLUMN fix_attempts INTEGER;
ALTER TABLE health ADD COLUMN fix_ok INTEGER;
ALTER TABLE health ADD COLUMN rsrp_dbm REAL;
ALTER TABLE health ADD COLUMN rsrq_db REAL;
ALTER TABLE health ADD COLUMN snr_db REAL;
ALTER TABLE health ADD COLUMN cell_mode TEXT;                -- ltem | nbiot
ALTER TABLE health ADD COLUMN band INTEGER;
ALTER TABLE health ADD COLUMN cell_id TEXT;
ALTER TABLE health ADD COLUMN tac INTEGER;
ALTER TABLE health ADD COLUMN still_s REAL;
ALTER TABLE health ADD COLUMN tilt_deg REAL;
ALTER TABLE health ADD COLUMN temp_c REAL;
ALTER TABLE health ADD COLUMN battery_v REAL;
ALTER TABLE health ADD COLUMN charging INTEGER;              -- 0 | 1
ALTER TABLE health ADD COLUMN uptime_s INTEGER;
ALTER TABLE health ADD COLUMN reset TEXT;                    -- last reset cause
