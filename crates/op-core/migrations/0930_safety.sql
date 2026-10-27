-- Safety: the boundary gate's log, the farm's own rules for the farm agent,
-- and owner sign-in without trusting headers.

-- Every boundary the gate looked at on its way to the collars, allowed or
-- refused: who sent it, the hard findings and what a person confirmed.
CREATE TABLE gate_log (
    id          TEXT PRIMARY KEY,                -- gat_…
    at          TEXT NOT NULL,
    herd_id     TEXT NOT NULL,
    collar_id   TEXT,                            -- an escape pen's collar
    sender      TEXT NOT NULL,                   -- farmer | api | mcp | brain | timer | auto | schedule | sweep | escape | system
    actor       TEXT,                            -- Actor JSON
    decision_id TEXT,
    allowed     INTEGER NOT NULL,
    codes       TEXT NOT NULL DEFAULT '[]',      -- hard finding codes
    confirmed   TEXT NOT NULL DEFAULT '[]',      -- codes a person confirmed
    findings    TEXT NOT NULL DEFAULT '[]',      -- the hard findings, with geometry
    geometry    TEXT,                            -- the shape as it would go to the collars
    reason      TEXT
);
CREATE INDEX gate_log_herd_at ON gate_log(herd_id, at);
CREATE INDEX gate_log_decision ON gate_log(decision_id);

-- Plain-language rules the farmer gives the farm agent. Removed rules stay
-- for the record.
CREATE TABLE farm_rules (
    id         TEXT PRIMARY KEY,                 -- rul_…
    text       TEXT NOT NULL,
    feature_id TEXT,                             -- the no-go zone or exclusion it became
    created_by TEXT NOT NULL,                    -- Actor JSON
    created_at TEXT NOT NULL,
    updated_by TEXT,
    updated_at TEXT NOT NULL,
    removed_by TEXT,
    removed_at TEXT
);

-- Owner sign-ins from a one-time code (the desktop app's launch, the URL
-- `openpasture serve` prints). Only hashes are kept.
CREATE TABLE owner_sessions (
    id         TEXT PRIMARY KEY,                 -- ses_…
    token_hash TEXT NOT NULL UNIQUE,
    label      TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    last_used  TEXT,
    revoked_at TEXT
);

CREATE TABLE signin_codes (
    code_hash  TEXT PRIMARY KEY,
    label      TEXT NOT NULL DEFAULT '',
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    used_at    TEXT
);
