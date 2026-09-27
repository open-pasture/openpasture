-- op-alerts (A3, hosted relay): when each key last polled its inbox, for the
-- dead-man. A key quiet for longer than `notify.hosting.deadman_after_min`
-- texts its dead-man recipients once per outage: `deadman_for` is the
-- `polled_at` of the outage they were texted about, so the next poll starts
-- a new one.
CREATE TABLE relay_polls (
    key_id      TEXT PRIMARY KEY REFERENCES brain_hosted_keys(id) ON DELETE CASCADE,
    polled_at   TEXT NOT NULL,
    deadman_for TEXT
);
