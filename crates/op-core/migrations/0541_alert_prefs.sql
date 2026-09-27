-- op-alerts (A-engine): who gets which alerts, per person. No row = the
-- defaults below. Quiet hours are farm-local HH:MM; NULL = the farm's own
-- (alerts.policy). `sms_opt_out` mirrors a STOP texted to the farm's number
-- (set by inbound texting); routing never texts an opted-out phone.
-- PII-adjacent: never exposed to the SQL console or export.
CREATE TABLE alert_prefs (
    user_id           TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    channels          TEXT NOT NULL DEFAULT '["sms"]',   -- JSON: sms | whatsapp | email
    min_severity      TEXT NOT NULL DEFAULT 'warning',
    herds             TEXT,                              -- JSON array of herd ids; NULL = every herd
    muted_kinds       TEXT NOT NULL DEFAULT '[]',        -- JSON array of rule kinds
    quiet_start       TEXT,
    quiet_end         TEXT,
    critical_in_quiet INTEGER NOT NULL DEFAULT 1,
    on_duty           INTEGER NOT NULL DEFAULT 0,
    sms_opt_out       INTEGER NOT NULL DEFAULT 0,
    updated_at        TEXT NOT NULL
);
