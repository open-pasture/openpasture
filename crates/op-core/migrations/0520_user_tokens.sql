-- op-core (J): a person's own sign-in tokens (`opu_…`), one per browser that
-- accepted a sign-in link. Only the sha256 of each token is kept; the token is
-- shown once. Revoked tokens stay for the record. PII: never exposed to the
-- SQL console or export.
CREATE TABLE user_tokens (
    id         TEXT PRIMARY KEY,                                   -- tok_…
    user_id    TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    label      TEXT NOT NULL DEFAULT '',
    token_hash TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL,
    last_used  TEXT,                                               -- written at most once a minute
    revoked_at TEXT
);
CREATE INDEX user_tokens_user ON user_tokens(user_id);
