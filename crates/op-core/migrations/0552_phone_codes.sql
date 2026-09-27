-- op-alerts: the pending one-time code proving a person's phone. The hash
-- covers the phone too, so a code sent to an old number never verifies a new
-- one. code_hash 'relay' means the hosted relay sent the code and checks it.
CREATE TABLE phone_codes (
    user_id   TEXT PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    code_hash TEXT NOT NULL,
    attempts  INTEGER NOT NULL DEFAULT 0,
    sent_at   TEXT NOT NULL
);
