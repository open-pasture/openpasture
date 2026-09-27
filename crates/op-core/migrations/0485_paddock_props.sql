-- op-core: extra paddock facts as a JSON object, e.g. FSA numbers under
-- fsa_farm, fsa_tract, fsa_field.
ALTER TABLE paddocks ADD COLUMN props TEXT NOT NULL DEFAULT '{}';
