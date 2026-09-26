-- Core record and telemetry tables. Times are RFC 3339 UTC text
-- (YYYY-MM-DDTHH:MM:SS.sssZ, sorts as text). Telemetry rows also carry
-- `t`, unix milliseconds, for range queries. Geometry is GeoJSON text.

CREATE TABLE farm (
    id          TEXT PRIMARY KEY,
    name        TEXT NOT NULL,
    timezone    TEXT NOT NULL,
    center_lon  REAL NOT NULL,
    center_lat  REAL NOT NULL,
    created_at  TEXT NOT NULL
);

CREATE TABLE paddocks (
    id           TEXT PRIMARY KEY,
    name         TEXT NOT NULL,
    geometry     TEXT NOT NULL,
    area_ha      REAL NOT NULL,
    status       TEXT NOT NULL DEFAULT 'resting',
    notes        TEXT,
    grazed_until TEXT,
    created_at   TEXT NOT NULL
);

CREATE TABLE herds (
    id            TEXT PRIMARY KEY,
    name          TEXT NOT NULL,
    species       TEXT NOT NULL,
    count         INTEGER NOT NULL DEFAULT 0,
    paddock_id    TEXT REFERENCES paddocks(id) ON DELETE SET NULL,
    autonomy      TEXT NOT NULL DEFAULT 'propose',
    timer_minutes INTEGER NOT NULL DEFAULT 60,
    created_at    TEXT NOT NULL
);

CREATE TABLE collars (
    id               TEXT PRIMARY KEY,
    name             TEXT NOT NULL,
    kind             TEXT NOT NULL,              -- sim | device
    herd_id          TEXT NOT NULL REFERENCES herds(id) ON DELETE CASCADE,
    animal_id        TEXT,
    key_hash         TEXT,                        -- sha256 hex of the collar key, devices only
    last_seen        TEXT,
    battery          REAL,
    boundary_version INTEGER,
    state            TEXT NOT NULL DEFAULT 'unknown',
    last_fix         TEXT,                        -- Fix JSON
    created_at       TEXT NOT NULL
);
CREATE INDEX collars_herd ON collars(herd_id);
CREATE UNIQUE INDEX collars_key_hash ON collars(key_hash) WHERE key_hash IS NOT NULL;

CREATE TABLE animals (
    id         TEXT PRIMARY KEY,
    tag        TEXT NOT NULL,
    name       TEXT,
    herd_id    TEXT NOT NULL REFERENCES herds(id) ON DELETE CASCADE,
    collar_id  TEXT REFERENCES collars(id) ON DELETE SET NULL,
    created_at TEXT NOT NULL
);
CREATE INDEX animals_herd ON animals(herd_id);
CREATE UNIQUE INDEX animals_collar ON animals(collar_id) WHERE collar_id IS NOT NULL;

CREATE TABLE decisions (
    id            TEXT PRIMARY KEY,
    herd_id       TEXT NOT NULL,
    source        TEXT NOT NULL,                  -- brain | farmer | heuristic
    brain         TEXT,
    model         TEXT,
    status        TEXT NOT NULL,
    action        TEXT,                           -- STAY | MOVE | NEEDS_INFO
    to_paddock_id TEXT,
    geometry      TEXT,
    reasoning     TEXT,
    confidence    REAL,
    need          TEXT,
    inputs        TEXT NOT NULL DEFAULT '{}',
    apply_at      TEXT,
    boundary_id   TEXT,
    error         TEXT,
    created_at    TEXT NOT NULL,
    responded_at  TEXT,
    outcome       TEXT                            -- JSON
);
CREATE INDEX decisions_herd ON decisions(herd_id, created_at);

-- Versions only go up, per herd.
CREATE TABLE boundaries (
    id           TEXT PRIMARY KEY,
    herd_id      TEXT NOT NULL,
    version      INTEGER NOT NULL,
    geometry     TEXT NOT NULL,
    warn_m       REAL NOT NULL,
    hysteresis_m REAL NOT NULL,
    effective_at TEXT,
    decision_id  TEXT NOT NULL,
    created_at   TEXT NOT NULL,
    UNIQUE (herd_id, version)
);

CREATE TABLE acks (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    collar_id   TEXT NOT NULL,
    herd_id     TEXT,
    command_id  TEXT NOT NULL,
    version     INTEGER NOT NULL,
    status      TEXT NOT NULL,                    -- received | applied | rejected
    reason      TEXT,
    at          TEXT NOT NULL,                    -- collar time
    received_at TEXT NOT NULL                     -- server time
);
CREATE INDEX acks_collar ON acks(collar_id, version);
CREATE INDEX acks_herd ON acks(herd_id, version);

CREATE TABLE fixes (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    collar_id        TEXT NOT NULL,
    herd_id          TEXT,
    animal_id        TEXT,
    at               TEXT NOT NULL,
    t                INTEGER NOT NULL,
    lon              REAL NOT NULL,
    lat              REAL NOT NULL,
    accuracy_m       REAL NOT NULL,
    sats             INTEGER NOT NULL DEFAULT 0,
    cn0              REAL,
    ttf_s            REAL,
    boundary_version INTEGER,
    state            TEXT,                        -- geofence state at this fix
    margin_m         REAL,
    paddock_id       TEXT                         -- paddock containing the fix, if known
);
CREATE INDEX fixes_collar_t ON fixes(collar_id, t);
CREATE INDEX fixes_herd_t ON fixes(herd_id, t);

CREATE TABLE cues (
    id               INTEGER PRIMARY KEY AUTOINCREMENT,
    collar_id        TEXT NOT NULL,
    herd_id          TEXT,
    animal_id        TEXT,
    at               TEXT NOT NULL,
    t                INTEGER NOT NULL,
    level            INTEGER NOT NULL,
    margin_m         REAL NOT NULL,
    lon              REAL,
    lat              REAL,
    boundary_version INTEGER
);
CREATE INDEX cues_collar_t ON cues(collar_id, t);
CREATE INDEX cues_herd_t ON cues(herd_id, t);

-- Key/value settings. `settings` holds the app Settings JSON; crates may add
-- their own keys, prefixed with the crate name (e.g. `ingest.tunnel`).
CREATE TABLE settings (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- Append-only activity log (kit docs/domain.md, FarmActivityEvent).
CREATE TABLE events (
    id          TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,                    -- e.g. paddock.created, decision.applied
    source      TEXT NOT NULL,                    -- farmer | brain | collar | system
    occurred_at TEXT NOT NULL,
    recorded_at TEXT NOT NULL,
    title       TEXT NOT NULL,
    body        TEXT,
    payload     TEXT NOT NULL DEFAULT '{}'
);
CREATE INDEX events_occurred ON events(occurred_at);

CREATE TABLE event_targets (
    event_id    TEXT NOT NULL REFERENCES events(id) ON DELETE CASCADE,
    target_type TEXT NOT NULL,                    -- farm | paddock | herd | animal | collar | decision
    target_id   TEXT NOT NULL,
    PRIMARY KEY (event_id, target_type, target_id)
);
CREATE INDEX event_targets_target ON event_targets(target_type, target_id);
