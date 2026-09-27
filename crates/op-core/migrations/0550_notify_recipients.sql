-- op-alerts (hosted relay): the phones and email addresses each hosted key may
-- text, each proven by a one-time code. PII: never exposed to the SQL console
-- or export.
CREATE TABLE notify_recipients (
    id          TEXT PRIMARY KEY,                -- nrc_…
    key_id      TEXT NOT NULL REFERENCES brain_hosted_keys(id) ON DELETE CASCADE,
    channel     TEXT NOT NULL,                   -- sms | whatsapp | email
    address     TEXT NOT NULL,                   -- E.164 phone or email address
    code_hash   TEXT,                            -- sha256 of the pending code; NULL once used
    attempts    INTEGER NOT NULL DEFAULT 0,      -- wrong tries against the pending code
    sent_at     TEXT,                            -- when the pending code went out
    verified_at TEXT,
    deadman     INTEGER NOT NULL DEFAULT 0       -- texted when the key's server goes quiet
);
CREATE UNIQUE INDEX notify_recipients_address ON notify_recipients(key_id, channel, address);
