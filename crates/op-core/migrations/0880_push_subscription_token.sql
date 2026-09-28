-- FX-FINAL: the sign-in a browser's push subscription was made under, so
-- signing that browser out (or the person out everywhere) takes its alerts
-- off it. NULL: made without a token of its own (this machine).
ALTER TABLE push_subscriptions ADD COLUMN token_id TEXT;
CREATE INDEX push_subscriptions_token ON push_subscriptions(token_id);
