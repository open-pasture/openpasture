-- op-core (J): sign-in links. `{base_url}/#/join/<code>` with a 128-bit code,
-- valid for 7 days, accepted once for an `opu_` token. Every link belongs to a
-- person; name, role, phone and email are what they were when it was made.
-- Only the sha256 of each code is kept. PII: never exposed to the SQL console
-- or export.
CREATE TABLE invites (
    id          TEXT PRIMARY KEY,                                  -- inv_…
    code_hash   TEXT NOT NULL UNIQUE,
    name        TEXT NOT NULL,
    role        TEXT NOT NULL,                                     -- owner | manager | hand | viewer
    phone       TEXT,
    email       TEXT,
    created_by  TEXT,                                              -- Actor JSON
    created_at  TEXT NOT NULL,
    expires_at  TEXT NOT NULL,
    accepted_at TEXT,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE
);
CREATE INDEX invites_user ON invites(user_id);
