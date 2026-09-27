-- op-alerts (A3): the inbound loops' own bookkeeping, one row per loop.
-- `poll:sms` / `poll:whatsapp`: checking Twilio for texts to the farm's
-- number (`cursor` = the number checked, `since` = when checking began for
-- it; older texts are never acted on). `relay:inbox`: the hosted relay's
-- inbox (`cursor` = the relay's cursor). `brief`: the morning brief
-- (`cursor` = the farm date last sent). `list:<user id>`: the numbered list
-- last texted to that person (`cursor` = JSON). `error` is the last failure
-- in words, cleared by the next success.
CREATE TABLE texting_state (
    key     TEXT PRIMARY KEY,
    cursor  TEXT,
    since   TEXT,
    ran_at  TEXT,
    ok_at   TEXT,
    error   TEXT
);
