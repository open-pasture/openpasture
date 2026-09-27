-- op-alerts (A-engine): alerts. One row per alert; a key that clears and
-- comes back later opens a new row, so at most one row per key is not
-- resolved. Rule kinds, keys and the lifecycle are in docs/API.md "Alerts".
-- The routing columns (notify … renotified_at) are the engine's own
-- bookkeeping for grouping, quiet hours, re-notification and escalation.
CREATE TABLE alerts (
    id            TEXT PRIMARY KEY,               -- alr_…
    kind          TEXT NOT NULL,                  -- rule kind: escaped | outside | silent | …
    key           TEXT NOT NULL,                  -- <kind>:<subject id>; rollups <kind>:herd:<herd id>
    severity      TEXT NOT NULL,                  -- info | warning | critical
    status        TEXT NOT NULL,                  -- open | acked | resolved
    herd_id       TEXT,
    title         TEXT NOT NULL,
    body          TEXT,
    at_lon        REAL,
    at_lat        REAL,
    targets       TEXT NOT NULL DEFAULT '[]',     -- JSON [[kind, id], …]
    data          TEXT NOT NULL DEFAULT '{}',     -- JSON
    opened_at     TEXT NOT NULL,
    updated_at    TEXT NOT NULL,
    acked_at      TEXT,
    acked_by      TEXT,                           -- Actor JSON
    resolved_at   TEXT,
    resolved_by   TEXT,                           -- Actor JSON; NULL with resolved_at = cleared by itself
    rolled_into   TEXT,                           -- the herd rollup it joined
    seen_at       TEXT NOT NULL,                  -- last evaluation that found the key
    notify        INTEGER NOT NULL DEFAULT 0,     -- the rule notified when it opened
    batch_at      TEXT,                           -- first send due (grouping window)
    routed_at     TEXT,                           -- first send done for everyone it matched
    tier          INTEGER NOT NULL DEFAULT 0,     -- escalations so far
    escalated_at  TEXT,
    renotified    INTEGER NOT NULL DEFAULT 0,     -- re-notifications so far
    renotified_at TEXT
);
CREATE UNIQUE INDEX alerts_open_key ON alerts(key) WHERE status != 'resolved';
CREATE INDEX alerts_status ON alerts(status, opened_at);
CREATE INDEX alerts_herd ON alerts(herd_id, opened_at);
CREATE INDEX alerts_key ON alerts(key, resolved_at);
