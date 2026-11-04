-- Wrong claim attempts and temporary lockouts (op-fleet provision). Kept in
-- the store so a restart does not clear a lockout.
CREATE TABLE claim_lockouts (
    scope TEXT NOT NULL,                 -- caller | serial | target | global
    key TEXT NOT NULL,
    fails TEXT NOT NULL DEFAULT '[]',    -- JSON array of unix-second timestamps in the window
    lock_until TEXT,                    -- server time; NULL when not locked
    lock_level INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (scope, key)
);

-- 24-hour undo for a claim or same-farm herd move (no approval required).
CREATE TABLE collar_undos (
    id TEXT PRIMARY KEY,
    collar_id TEXT NOT NULL,
    kind TEXT NOT NULL,                 -- claim | move
    serial TEXT,                        -- claim: unit serial to return to provisioned
    from_herd_id TEXT,                  -- move: herd before the change
    to_herd_id TEXT NOT NULL,           -- herd after the change (claim destination)
    from_animal_id TEXT,
    to_animal_id TEXT,
    from_boundary_version INTEGER,
    from_state TEXT,
    from_outside_since TEXT,
    by_user TEXT,
    created_at TEXT NOT NULL,
    expires_at TEXT NOT NULL,
    undone_at TEXT
);
CREATE INDEX collar_undos_collar ON collar_undos(collar_id, expires_at);
