-- op-core: every text, email, webhook and push going out (the outbox the
-- sender claims from) and every text coming in. PII: never exposed to the
-- SQL console or export.
CREATE TABLE messages (
    id              TEXT PRIMARY KEY,             -- ntf_…
    direction       TEXT NOT NULL,                -- out | in
    channel         TEXT NOT NULL,                -- sms | whatsapp | email | webhook | relay | push
    address         TEXT NOT NULL,                -- phone, email, URL or push endpoint id
    user_id         TEXT,
    kind            TEXT NOT NULL,                -- alert | brief | reply | test | verify | inbound
    text            TEXT NOT NULL,
    subject         TEXT,
    status          TEXT NOT NULL,                -- queued | sending | sent | delivered | failed | received | ignored
    error           TEXT,
    alert_id        TEXT,
    decision_id     TEXT,
    idempotency_key TEXT UNIQUE,
    provider_id     TEXT,
    attempts        INTEGER NOT NULL DEFAULT 0,
    next_attempt_at TEXT,
    created_at      TEXT NOT NULL,
    updated_at      TEXT NOT NULL
);
CREATE INDEX messages_queue ON messages(status, next_attempt_at);
CREATE INDEX messages_created ON messages(created_at);
CREATE INDEX messages_address ON messages(address, direction, created_at);
CREATE UNIQUE INDEX messages_provider ON messages(channel, provider_id) WHERE provider_id IS NOT NULL;
