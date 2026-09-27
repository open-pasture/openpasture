-- op-alerts (hosted relay): texts and emails sent for each hosted key per UTC
-- day, for the daily cap.
CREATE TABLE notify_usage (
    key_id TEXT NOT NULL,
    day    TEXT NOT NULL,                        -- YYYY-MM-DD, UTC
    count  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (key_id, day)
) WITHOUT ROWID;
