-- op-core: protocol v1 telemetry columns. Cue kind (warn | outside), the
-- ring it was about and tone length; fix HDOP; the reject code on acks.
-- Rows written before protocol v1 have NULL kind: readers use
-- COALESCE(kind, CASE WHEN margin_m < 0 THEN 'outside' ELSE 'warn' END).
ALTER TABLE cues ADD COLUMN kind TEXT;
ALTER TABLE cues ADD COLUMN ring INTEGER;
ALTER TABLE cues ADD COLUMN dur_ms INTEGER;
ALTER TABLE fixes ADD COLUMN hdop REAL;
ALTER TABLE acks ADD COLUMN code TEXT;
