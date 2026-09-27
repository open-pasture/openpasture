-- Saved strip layouts for a paddock (C): how the strips were cut (params) and
-- the strips themselves, so the same arrangement can be used again.
CREATE TABLE layouts (
    id          TEXT PRIMARY KEY,                 -- lay_…
    paddock_id  TEXT NOT NULL REFERENCES paddocks(id) ON DELETE CASCADE,
    name        TEXT NOT NULL,
    params      TEXT NOT NULL,                    -- JSON: { orientation_deg, width_m?, count?, days?, head?, warn_m? }
    strips      TEXT NOT NULL,                    -- JSON: [Polygon], in advance order
    fitted_to   TEXT NOT NULL,                    -- key of the paddock geometry the strips were cut from
    created_by  TEXT NOT NULL,                    -- JSON Actor
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL
);
CREATE INDEX layouts_paddock ON layouts(paddock_id, created_at);
