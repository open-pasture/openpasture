-- op-ingest (FX-FENCE): when a collar's current config last went out in a
-- report reply. A collar holding it has checked the server's signature and
-- knows its herd, so its `wrong_herd` and `bad_sig` refusals from before
-- then are offered again once (a herd change reaching the collar after a
-- download of the new herd's boundary, a reprovisioned collar).
ALTER TABLE collar_config ADD COLUMN sent_at TEXT;
