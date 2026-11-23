-- External sign-in identities (Clerk today). The external subject, not a
-- display name or email, is the durable identity.
CREATE TABLE user_external_ids (
    issuer      TEXT NOT NULL,
    subject     TEXT NOT NULL,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at  TEXT NOT NULL,
    PRIMARY KEY (issuer, subject)
);
CREATE INDEX user_external_ids_user ON user_external_ids(user_id);
