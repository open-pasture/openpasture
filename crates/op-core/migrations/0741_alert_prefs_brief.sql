-- op-alerts (A3): the morning brief by text, per person (off by default).
-- `alert_prefs.sms_opt_out` (STOP / START) already exists (0541).
ALTER TABLE alert_prefs ADD COLUMN brief INTEGER NOT NULL DEFAULT 0;
