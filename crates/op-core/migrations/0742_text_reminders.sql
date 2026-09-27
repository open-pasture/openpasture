-- op-alerts (A3): "later" texted back to a decision prompt: ask that person
-- again at `due_at`, on the channel and number the "later" came from, if the
-- decision is still waiting then. The decision's own timer is unchanged.
CREATE TABLE text_reminders (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id     TEXT NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    decision_id TEXT NOT NULL REFERENCES decisions(id) ON DELETE CASCADE,
    channel     TEXT NOT NULL,
    address     TEXT NOT NULL,
    due_at      TEXT NOT NULL,
    done_at     TEXT,
    created_at  TEXT NOT NULL
);
CREATE INDEX text_reminders_due ON text_reminders(done_at, due_at);
