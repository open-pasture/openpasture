-- op-core: each collar's latest boundary ack (highest version, then its
-- latest status), kept current by the ack handler so readers never scan
-- `acks`. Backfilled once from the acks so far.
CREATE TABLE collar_boundary_state (
    collar_id  TEXT PRIMARY KEY,
    herd_id    TEXT,
    version    INTEGER NOT NULL,
    status     TEXT NOT NULL,                     -- received | applied | rejected
    code       TEXT,                              -- reject code (protocol v1)
    command_id TEXT NOT NULL,
    at         TEXT NOT NULL                      -- collar time of the ack
);
INSERT INTO collar_boundary_state (collar_id, herd_id, version, status, code, command_id, at)
SELECT collar_id, herd_id, version, status, code, command_id, at FROM (
    SELECT a.*, ROW_NUMBER() OVER (PARTITION BY a.collar_id ORDER BY a.version DESC, a.id DESC) AS rn FROM acks a
) WHERE rn = 1;
