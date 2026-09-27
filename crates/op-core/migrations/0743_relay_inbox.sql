-- op-alerts (A3, hosted relay): texts to the relay's number, waiting for the
-- farm server whose key last texted that number. A farm long-polls
-- `GET /v1/notify/inbox?since=<cursor>`; rows up to the cursor it sends back
-- are delivered and deleted. PII: never exposed to the SQL console or export.
CREATE TABLE relay_inbox (
    seq      INTEGER PRIMARY KEY AUTOINCREMENT,
    id       TEXT NOT NULL UNIQUE,                 -- rin_…
    key_id   TEXT NOT NULL REFERENCES brain_hosted_keys(id) ON DELETE CASCADE,
    channel  TEXT NOT NULL,                        -- sms | whatsapp
    address  TEXT NOT NULL,                        -- E.164 phone it came from
    text     TEXT NOT NULL,
    at       TEXT NOT NULL
);
CREATE INDEX relay_inbox_key ON relay_inbox(key_id, seq);
