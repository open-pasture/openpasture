-- op-alerts (A-engine): every notification an alert caused, for
-- re-notification and escalation. `user_id` NULL = the farm's webhook.
-- `tier` 0 = the first send, 1… = escalations; a re-notification repeats the
-- tier the person was first reached at.
CREATE TABLE alert_notifications (
    alert_id   TEXT NOT NULL REFERENCES alerts(id) ON DELETE CASCADE,
    user_id    TEXT,
    channel    TEXT NOT NULL,
    tier       INTEGER NOT NULL,
    sent_at    TEXT NOT NULL,
    message_id TEXT
);
CREATE INDEX alert_notifications_alert ON alert_notifications(alert_id, user_id);
