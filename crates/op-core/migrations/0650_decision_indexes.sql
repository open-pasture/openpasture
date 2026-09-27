-- P: the decision scheduler looks up due decisions by status and apply time,
-- and the engine a herd's open decision by status.
CREATE INDEX IF NOT EXISTS decisions_status_apply ON decisions(status, apply_at);
CREATE INDEX IF NOT EXISTS decisions_herd_status ON decisions(herd_id, status);
