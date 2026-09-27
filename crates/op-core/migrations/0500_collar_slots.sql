-- op-ingest (protocol v1): the boundaries each collar holds, as its reports
-- list them (`applied` = the one in effect, `received` = staged), kept
-- between reports from its acks, plus the versions it rejected (`rejected`
-- with the reject code). A report with `slots` replaces the applied and
-- received rows; rejected rows stay until the collar holds a higher version.
CREATE TABLE collar_slots (
    collar_id    TEXT NOT NULL,
    version      INTEGER NOT NULL,
    status       TEXT NOT NULL,                  -- applied | received | rejected
    effective_at TEXT,
    reported_at  TEXT NOT NULL,                  -- server time of the report or ack
    code         TEXT,                           -- reject code, for rejected rows
    PRIMARY KEY (collar_id, version)
) WITHOUT ROWID;
