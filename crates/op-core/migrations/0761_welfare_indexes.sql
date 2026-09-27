-- op-analytics (H): the welfare record reads one animal's cues and episodes,
-- whichever collar it wore. Episodes carry what learning status needs, so it
-- reads the index alone.
CREATE INDEX IF NOT EXISTS cues_animal_t ON cues(animal_id, t);
CREATE INDEX IF NOT EXISTS episodes_animal ON episodes(animal_id, start_t, end_t, outcome, derived);
