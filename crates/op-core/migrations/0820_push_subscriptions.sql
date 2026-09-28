-- M: Web Push. One row per browser or installed app that turned alerts on
-- (the PushSubscription it handed over). The endpoint is the push service's
-- URL for that browser; p256dh and auth are its message encryption keys
-- (base64url, RFC 8291). Personal data: never in op-analytics' SQL_TABLES.
CREATE TABLE push_subscriptions (
    id         TEXT PRIMARY KEY,                                        -- psh_…
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    endpoint   TEXT NOT NULL UNIQUE,
    p256dh     TEXT NOT NULL,
    auth       TEXT NOT NULL,
    created_at TEXT NOT NULL,
    last_ok    TEXT                                                     -- the push service last took a message for it
);
CREATE INDEX push_subscriptions_user ON push_subscriptions(user_id, created_at);
