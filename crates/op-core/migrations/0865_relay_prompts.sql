-- op-alerts (hosted relay): relayed texts that asked the person to answer a
-- decision (the farm server says so when it posts the text). A bare Y or N
-- to the relay's shared number goes to the one farm that asked; when more
-- than one farm asked, the host asks for the decision's code instead. Rows
-- older than two days are pruned. PII: never exposed to the SQL console or
-- export.
CREATE TABLE relay_prompts (
    message_id TEXT PRIMARY KEY,                  -- this server's messages.id (ntf_…)
    key_id     TEXT NOT NULL REFERENCES brain_hosted_keys(id) ON DELETE CASCADE,
    address    TEXT NOT NULL,                     -- E.164 phone it went to
    at         TEXT NOT NULL
);
CREATE INDEX relay_prompts_address ON relay_prompts(address, at);
