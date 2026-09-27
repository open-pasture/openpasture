-- op-core: people on the farm. A person needs no sign-in: people who only
-- text are users with a phone and no token. PII: never exposed to the SQL
-- console or export.
CREATE TABLE users (
    id                TEXT PRIMARY KEY,              -- usr_…
    name              TEXT NOT NULL,
    role              TEXT NOT NULL,                 -- owner | manager | hand | viewer
    phone             TEXT,                          -- E.164, e.g. +15155550123
    phone_verified_at TEXT,                          -- set by a one-time code; cleared when phone changes
    email             TEXT,
    created_at        TEXT NOT NULL,
    disabled_at       TEXT
);
CREATE UNIQUE INDEX users_phone ON users(phone) WHERE phone IS NOT NULL;
CREATE UNIQUE INDEX users_email ON users(lower(email)) WHERE email IS NOT NULL;
