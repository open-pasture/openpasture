-- P: tracks for a herd seek each collar's first fix per time bucket; with the
-- herd first, collars that were in another herd cost nothing.
CREATE INDEX IF NOT EXISTS fixes_herd_collar_t ON fixes(herd_id, collar_id, t);
