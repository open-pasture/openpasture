-- op-analytics (H): how far the welfare task has read. `cues` holds the
-- highest cue id whose firmware 0.1 episodes have been rebuilt.
CREATE TABLE welfare_marks (
    source     TEXT PRIMARY KEY,
    max_id     INTEGER NOT NULL,
    updated_at TEXT NOT NULL
);
