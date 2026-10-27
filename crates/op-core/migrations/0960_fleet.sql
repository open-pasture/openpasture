-- op-fleet: units from the moment a station registers them, their factory
-- tests, provisioning and claim, firmware images and rollouts, and the herd
-- network (anchors, nodes, link quality). Times are RFC 3339 UTC text, as
-- everywhere else.

-- One piece of hardware, by serial (protocol v2 identity.md). `device_kid`
-- is the v2 device id: the first 8 bytes of SHA-256 of the device key, hex.
-- `factory_record` is the maker-signed COSE_Sign1 (hex), as received or as
-- this server signed it with its maker key.
CREATE TABLE fleet_units (
    serial         TEXT PRIMARY KEY,
    device_class   TEXT NOT NULL,                 -- node | anchor | station | direct | any class a builder names
    hw_rev         TEXT NOT NULL,
    fw_version     TEXT NOT NULL,                 -- as built; the collar's reports take over
    device_pubkey  TEXT NOT NULL,                 -- base64 Ed25519, generated on the unit
    device_kid     TEXT NOT NULL,
    radios         TEXT NOT NULL DEFAULT '{}',    -- JSON {lora_eui?, imei?, iccid?, ble_addr?}
    image_keys     TEXT NOT NULL DEFAULT '[]',    -- JSON kids its bootloader trusts
    caps           TEXT NOT NULL DEFAULT '[]',    -- JSON capabilities fixed by the hardware
    state          TEXT NOT NULL,                 -- built | tested | provisioned | claimed | in_service | retired | rma
    origin         TEXT NOT NULL DEFAULT 'station', -- station | card
    station        TEXT,
    claim_code     TEXT,                          -- 16 Crockford base32 characters, normalized
    factory_record TEXT,                          -- hex COSE_Sign1
    maker          TEXT,
    maker_kid      TEXT,
    maker_verified INTEGER NOT NULL DEFAULT 0,
    made_at        TEXT,
    collar_id      TEXT,                          -- once claimed
    provisioned_at TEXT,
    claimed_at     TEXT,
    created_at     TEXT NOT NULL,
    updated_at     TEXT NOT NULL
);
CREATE UNIQUE INDEX fleet_units_kid ON fleet_units(device_kid);
CREATE UNIQUE INDEX fleet_units_claim ON fleet_units(claim_code) WHERE claim_code IS NOT NULL;
CREATE UNIQUE INDEX fleet_units_collar ON fleet_units(collar_id) WHERE collar_id IS NOT NULL;
CREATE INDEX fleet_units_state ON fleet_units(state, created_at);

-- Every device key a serial has had; `to_at` is set when a key was replaced.
CREATE TABLE fleet_unit_keys (
    serial        TEXT NOT NULL,
    device_pubkey TEXT NOT NULL,
    device_kid    TEXT NOT NULL,
    from_at       TEXT NOT NULL,
    to_at         TEXT,
    PRIMARY KEY (serial, device_kid)
) WITHOUT ROWID;

-- What happened to a unit, in order: registered, key_replaced, tested,
-- provisioned, claimed, in_service, state, firmware.
CREATE TABLE fleet_unit_events (
    id     INTEGER PRIMARY KEY AUTOINCREMENT,
    serial TEXT NOT NULL,
    at     TEXT NOT NULL,
    kind   TEXT NOT NULL,
    detail TEXT NOT NULL DEFAULT '{}',            -- JSON
    by     TEXT                                   -- who: a person, a station or the server
);
CREATE INDEX fleet_unit_events_serial ON fleet_unit_events(serial, id);

-- Test plans by class, versioned: editing adds a version, runs keep theirs.
CREATE TABLE fleet_test_plans (
    id           TEXT NOT NULL,                   -- e.g. node-fct
    version      INTEGER NOT NULL,
    device_class TEXT NOT NULL,
    steps        TEXT NOT NULL,                   -- JSON [{name, kind, limit_lo?, limit_hi?, unit?, required, note?}]
    shipped      INTEGER NOT NULL DEFAULT 0,
    created_at   TEXT NOT NULL,
    created_by   TEXT,
    PRIMARY KEY (id, version)
) WITHOUT ROWID;
CREATE INDEX fleet_test_plans_class ON fleet_test_plans(device_class, version);

-- One pass of a unit through a fixture, judged by the server against its plan.
CREATE TABLE fleet_test_runs (
    id           TEXT PRIMARY KEY,                -- tst_…
    serial       TEXT NOT NULL,
    plan_id      TEXT NOT NULL,
    plan_version INTEGER NOT NULL,
    station      TEXT,
    fixture_id   TEXT,
    started_at   TEXT NOT NULL,
    finished_at  TEXT NOT NULL,
    received_at  TEXT NOT NULL,
    result       TEXT NOT NULL,                   -- pass | fail
    failed       TEXT NOT NULL DEFAULT '[]',      -- JSON step names
    steps        TEXT NOT NULL                    -- JSON judged steps
);
CREATE INDEX fleet_test_runs_serial ON fleet_test_runs(serial, finished_at);
CREATE INDEX fleet_test_runs_plan ON fleet_test_runs(plan_id, finished_at);
CREATE INDEX fleet_test_runs_station ON fleet_test_runs(station, finished_at);

-- Factory station PCs. A station token can only register units, fetch plans,
-- post tests, provision and draw labels. Only its hash is kept.
CREATE TABLE fleet_stations (
    id           TEXT PRIMARY KEY,                -- stn_…
    name         TEXT NOT NULL,
    token_hash   TEXT NOT NULL UNIQUE,
    created_at   TEXT NOT NULL,
    created_by   TEXT,
    last_used_at TEXT,
    revoked_at   TEXT
);

-- Keys this server trusts besides the OpenCollar release keys built into
-- it: image keys (firmware signed by a DIY builder or owner) and maker keys
-- (factory records from another builder).
CREATE TABLE fleet_signing_keys (
    kid        TEXT PRIMARY KEY,
    purpose    TEXT NOT NULL,                     -- image | maker
    name       TEXT NOT NULL,
    public_key TEXT NOT NULL,
    created_at TEXT NOT NULL,
    created_by TEXT,
    revoked_at TEXT
);

-- Firmware keys a verified release manifest told us to stop trusting.
CREATE TABLE fleet_revoked_kids (
    kid         TEXT PRIMARY KEY,
    at          TEXT NOT NULL,
    firmware_id TEXT
);

-- Release manifests (protocol v2 type 65, a COSE_Sign1 by a release or
-- image key). The image bytes, when uploaded, live in
-- <data>/firmware/<sha256>.bin.
CREATE TABLE fleet_firmware (
    id               TEXT PRIMARY KEY,            -- fwi_…
    version          TEXT NOT NULL,
    classes          TEXT NOT NULL,               -- JSON device class names
    hw_revs          TEXT NOT NULL,               -- JSON; exact board revisions
    sha256           TEXT NOT NULL,               -- the manifest's digest of the image
    size             INTEGER NOT NULL,
    signer_kid       TEXT NOT NULL,
    security_counter INTEGER NOT NULL,
    min_battery      INTEGER,                     -- percent
    revoke           TEXT NOT NULL DEFAULT '[]',  -- JSON kids
    channel          TEXT NOT NULL DEFAULT 'stable', -- stable | beta
    notes            TEXT,
    manifest         TEXT NOT NULL,               -- hex COSE_Sign1 as signed
    has_image        INTEGER NOT NULL DEFAULT 0,
    created_at       TEXT NOT NULL,
    created_by       TEXT,
    withdrawn_at     TEXT
);
CREATE UNIQUE INDEX fleet_firmware_image ON fleet_firmware(sha256, version);

-- A release going out in stages (protocol v2 server.md §8): one collar of
-- a first herd, then that herd, then every herd.
CREATE TABLE fleet_rollouts (
    id            TEXT PRIMARY KEY,               -- rol_…
    firmware_id   TEXT NOT NULL,
    state         TEXT NOT NULL,                  -- active | paused | done | cancelled
    paused_reason TEXT,
    waves         TEXT NOT NULL,                  -- JSON [{kind, after_h, started_at?, collar_id?, herd_id?}]
    wave          INTEGER NOT NULL DEFAULT 0,
    created_at    TEXT NOT NULL,
    created_by    TEXT,
    updated_at    TEXT NOT NULL
);
CREATE INDEX fleet_rollouts_state ON fleet_rollouts(state, created_at);

-- Each collar's place in a rollout: what it runs, what it should run, and why
-- it is waiting. `applied_at`: the collar said it wrote the image.
CREATE TABLE fleet_rollout_units (
    rollout_id TEXT NOT NULL,
    collar_id  TEXT NOT NULL,
    serial     TEXT,
    wave       INTEGER NOT NULL,
    state      TEXT NOT NULL,                     -- waiting | held | unsupported | ready | offered | confirmed | failed | rolled_back
    reason     TEXT,
    current    TEXT,
    offered_at TEXT,
    applied_at TEXT,
    updated_at TEXT NOT NULL,
    PRIMARY KEY (rollout_id, collar_id)
) WITHOUT ROWID;
CREATE INDEX fleet_rollout_units_collar ON fleet_rollout_units(collar_id);

-- The herd network: each collar's place in it (role, last heard, via which
-- anchor, how well), kept current from protocol v2 reports and beacons.
CREATE TABLE fleet_links (
    collar_id      TEXT PRIMARY KEY,
    herd_id        TEXT,
    role           TEXT,                          -- anchor | node | direct, as last reported
    heard_at       TEXT,                          -- server time
    via            TEXT,                          -- anchor collar id or fixed anchor id
    rssi_dbm       REAL,
    snr_db         REAL,
    beacon_at      TEXT,                          -- anchors: the last beacon they sent
    active         INTEGER NOT NULL DEFAULT 0,    -- anchors: the herd's link now
    lte_rsrp_dbm   REAL,
    updated_at     TEXT NOT NULL
);
CREATE INDEX fleet_links_herd ON fleet_links(herd_id);

-- Anchors on posts or barns: the same electronics as an anchor collar, fixed.
CREATE TABLE fleet_fixed_anchors (
    id         TEXT PRIMARY KEY,                  -- anc_…
    name       TEXT NOT NULL,
    lon        REAL NOT NULL,
    lat        REAL NOT NULL,
    serial     TEXT,
    herd_ids   TEXT NOT NULL DEFAULT '[]',        -- JSON
    created_at TEXT NOT NULL
);
