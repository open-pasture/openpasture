-- op-analytics (H): episodes the server rebuilt from cues and fixes for a
-- collar that doesn't report them (firmware 0.1), beside those collars
-- report. Welfare records mark animals whose episodes are derived.
ALTER TABLE episodes ADD COLUMN derived INTEGER NOT NULL DEFAULT 0;
