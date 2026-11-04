-- Collar welfare shut-off state and queued re-arm commands (op-architect record L).
-- Additive only: welfare fields on collars, a command queue, an audit log, and
-- welfare event rows from reports.

ALTER TABLE collars ADD COLUMN welfare_state TEXT;          -- enforcement: on | off
ALTER TABLE collars ADD COLUMN shutoff_at TEXT;             -- last reported welfare.shutoff_at
ALTER TABLE collars ADD COLUMN welfare_rearm_seq INTEGER;   -- last reported welfare.rearm_seq
ALTER TABLE collars ADD COLUMN rearm_seq INTEGER NOT NULL DEFAULT 0; -- highest issued by this server

CREATE TABLE collar_rearms (
    id TEXT PRIMARY KEY,                    -- command_id (rrm_…)
    collar_id TEXT NOT NULL,
    herd_id TEXT NOT NULL,
    rearm_seq INTEGER NOT NULL,
    reason TEXT NOT NULL,                   -- free text (server / audit only)
    reason_code TEXT NOT NULL,              -- wire enum
    shutoff_at TEXT NOT NULL,               -- must match collar's reported shut-off
    issued_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    status TEXT NOT NULL,                   -- pending | acked | expired | rejected | superseded
    ack_status TEXT,                        -- applied | rejected (from collar ack)
    ack_code TEXT,                          -- stale | expired | unsupported | not_now | …
    ack_at TEXT,
    actor TEXT NOT NULL,                    -- JSON: who + kind agent|human + token_id?
    created_at TEXT NOT NULL,
    refresh_of TEXT,                        -- prior command_id when this is the one-shot stale refresh
    UNIQUE (collar_id, rearm_seq)
);
CREATE INDEX collar_rearms_collar_status ON collar_rearms (collar_id, status);
CREATE INDEX collar_rearms_expires ON collar_rearms (status, expires_at);

CREATE TABLE collar_rearm_audit (
    id TEXT PRIMARY KEY,
    collar_id TEXT NOT NULL,
    rearm_id TEXT,                          -- collar_rearms.id when known
    at TEXT NOT NULL,
    who TEXT NOT NULL,                       -- JSON Actor + kind + token_id?
    reason TEXT NOT NULL,
    reason_code TEXT NOT NULL,
    shutoff_at TEXT,
    event TEXT NOT NULL,                    -- issued | acked | expired | rejected | superseded
    ack_status TEXT,
    ack_code TEXT
);
CREATE INDEX collar_rearm_audit_collar ON collar_rearm_audit (collar_id, at);

CREATE TABLE collar_welfare_events (
    id TEXT PRIMARY KEY,
    collar_id TEXT NOT NULL,
    herd_id TEXT NOT NULL,
    kind INTEGER NOT NULL,                  -- 1..4 (provisional; see registry)
    at TEXT NOT NULL,
    t INTEGER NOT NULL,
    payload TEXT,                           -- JSON remainder of the event
    UNIQUE (collar_id, t, kind)
);
CREATE INDEX collar_welfare_events_collar ON collar_welfare_events (collar_id, t);
