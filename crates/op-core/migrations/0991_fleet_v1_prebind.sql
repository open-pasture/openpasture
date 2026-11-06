-- Opt-in cordless field-claim (prebind_v1): mint v1 collar id + bearer at
-- USB provision so a Rook can report before the farmer scans its QR card.
-- claim_code on the card is mirrored in v1_card_code so undo can restore it.
-- v1_herd_id is the herd placed in provision line "h"; Rook rejects
-- BoundaryCommand.herd_id mismatches until a signed config updates it, so
-- the first claim must use this herd (same-farm transfer moves it later).
ALTER TABLE fleet_units ADD COLUMN v1_collar_id TEXT;
ALTER TABLE fleet_units ADD COLUMN v1_key_hash TEXT;
ALTER TABLE fleet_units ADD COLUMN v1_card_code TEXT;
ALTER TABLE fleet_units ADD COLUMN v1_herd_id TEXT;
CREATE UNIQUE INDEX fleet_units_v1_collar ON fleet_units(v1_collar_id) WHERE v1_collar_id IS NOT NULL;
CREATE UNIQUE INDEX fleet_units_v1_key ON fleet_units(v1_key_hash) WHERE v1_key_hash IS NOT NULL;
