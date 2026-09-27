-- op-analytics (G): a person checked how a collar sits on its animal.
CREATE TABLE collar_fit_checks (
    id         TEXT PRIMARY KEY,              -- fit_…
    collar_id  TEXT NOT NULL,
    checked_at TEXT NOT NULL,
    "by"       TEXT,                          -- Actor JSON
    notes      TEXT
);
CREATE INDEX collar_fit_checks_collar ON collar_fit_checks(collar_id, checked_at);
