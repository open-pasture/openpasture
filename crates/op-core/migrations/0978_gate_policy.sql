-- Safety gate, second pass (docs/internal/review-2026-10-26-astra/02-safety-auth.md).

-- The farm map's safety policy revision. Every change to a map feature (no-go
-- zones, roads, the farm boundary, hazards, water, exclusions) or to the
-- gate's settings bumps it in the same transaction, so a boundary checked
-- against revision N can be stored only while the map is still at N.
CREATE TABLE gate_policy (
    id       INTEGER PRIMARY KEY CHECK (id = 1),
    revision INTEGER NOT NULL
);
INSERT INTO gate_policy (id, revision) VALUES (1, 1);

CREATE TRIGGER gate_policy_feature_insert AFTER INSERT ON features
BEGIN
    UPDATE gate_policy SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER gate_policy_feature_update AFTER UPDATE ON features
BEGIN
    UPDATE gate_policy SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER gate_policy_feature_delete AFTER DELETE ON features
BEGIN
    UPDATE gate_policy SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER gate_policy_settings_insert AFTER INSERT ON settings WHEN NEW.key = 'gate'
BEGIN
    UPDATE gate_policy SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER gate_policy_settings_update AFTER UPDATE ON settings WHEN NEW.key = 'gate' OR OLD.key = 'gate'
BEGIN
    UPDATE gate_policy SET revision = revision + 1 WHERE id = 1;
END;
CREATE TRIGGER gate_policy_settings_delete AFTER DELETE ON settings WHEN OLD.key = 'gate'
BEGIN
    UPDATE gate_policy SET revision = revision + 1 WHERE id = 1;
END;

-- What each gate row was: a check before a send (`send`), a preview of a
-- person's approval (`preview`), a copy for one collar (`copy`), a command
-- refused at download (`delivery`), a staged boundary checked again
-- (`recheck`) or as it took effect (`activation`). The revision it was
-- judged at, the exact shape's hash, the credential, and the boundary it
-- became (set in the transaction that stored it).
ALTER TABLE gate_log ADD COLUMN stage TEXT NOT NULL DEFAULT 'send';
ALTER TABLE gate_log ADD COLUMN revision INTEGER;
ALTER TABLE gate_log ADD COLUMN geometry_hash TEXT;
ALTER TABLE gate_log ADD COLUMN credential TEXT;
ALTER TABLE gate_log ADD COLUMN boundary_id TEXT;
ALTER TABLE gate_log ADD COLUMN version INTEGER;
CREATE INDEX gate_log_boundary ON gate_log(boundary_id);

-- The gate row that let each boundary through, and the revision it was stored at.
ALTER TABLE boundaries ADD COLUMN gate_id TEXT;
ALTER TABLE boundaries ADD COLUMN policy_revision INTEGER;

-- Texted approval codes tried, per phone: a try is reserved here before the
-- code is looked at (at most five an hour), and given back when it was right.
CREATE TABLE sms_code_attempts (
    id      INTEGER PRIMARY KEY AUTOINCREMENT,
    address TEXT NOT NULL,
    at      TEXT NOT NULL
);
CREATE INDEX sms_code_attempts_address_at ON sms_code_attempts(address, at);
