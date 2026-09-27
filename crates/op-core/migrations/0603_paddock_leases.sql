-- op-reports: who owns a rented paddock and what the grazing costs.
-- rate_per: acre_season | head_day | au_day | aum | pair_month. For
-- acre_season, rate_amount is per hectare (the API is SI; the UI shows it per
-- acre on imperial farms). season_from / season_to are farm-local dates.
-- No foreign key: a lease record outlives a deleted paddock.
CREATE TABLE paddock_leases (
    paddock_id  TEXT PRIMARY KEY,
    landowner   TEXT NOT NULL,
    rate_per    TEXT NOT NULL,
    rate_amount REAL NOT NULL,
    currency    TEXT NOT NULL,
    season_from TEXT,
    season_to   TEXT,
    notes       TEXT,
    updated_at  TEXT NOT NULL
);
