-- op-analytics (H): fix rate and cell signal on the coverage grid, from the
-- collars' health reports. Each report counts in the cell of that collar's
-- fix nearest in time. `rsrp_hist` is JSON [[dBm, reports], …] (whole dBm),
-- NULL when no report there measured the cell.
ALTER TABLE coverage_days ADD COLUMN fix_attempts INTEGER NOT NULL DEFAULT 0;
ALTER TABLE coverage_days ADD COLUMN fix_ok INTEGER NOT NULL DEFAULT 0;
ALTER TABLE coverage_days ADD COLUMN rsrp_hist TEXT;
