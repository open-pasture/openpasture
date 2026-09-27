//! How many texted questions run: each one is a brain run (a model call or a
//! Claude CLI process), so one person has at most one in flight and
//! [`PER_HOUR`] an hour, and at most [`AT_ONCE`] run on the farm at a time
//! (the rest wait their turn). A question over either limit isn't asked; the
//! first one over it is told why, the rest are logged `ignored`, so a burst
//! of texts never becomes a burst of replies.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use chrono::{DateTime, Duration, Utc};
use op_core::Ctx;
use tokio::sync::Semaphore;

/// Questions one person may ask in an hour.
pub const PER_HOUR: usize = 20;
/// Questions answered at once on one farm.
pub const AT_ONCE: usize = 2;

pub const BUSY: &str = "One question at a time. I'll answer the one before first.";
pub fn capped_text() -> String {
    format!("That's {PER_HOUR} questions this hour. Ask again later.")
}

#[derive(Default)]
struct Person {
    /// A question of theirs is being answered; `true` once told to wait.
    busy: Option<bool>,
    /// When their recent questions were taken.
    asked: VecDeque<DateTime<Utc>>,
    /// Told about the hourly limit, until this time.
    capped_until: Option<DateTime<Utc>>,
}

#[derive(Default)]
struct Farm {
    people: HashMap<String, Person>,
    runs: Option<Arc<Semaphore>>,
}

/// Per data dir (one farm per data dir; tests run many at once).
static FARMS: LazyLock<Mutex<HashMap<PathBuf, Farm>>> = LazyLock::new(Default::default);

fn with<T>(ctx: &Ctx, f: impl FnOnce(&mut Farm) -> T) -> T {
    let mut all = FARMS.lock().unwrap_or_else(|e| e.into_inner());
    f(all.entry(ctx.data_dir().to_path_buf()).or_default())
}

/// What becomes of a question.
pub enum Admit {
    /// Ask it; drop the turn when the answer is queued.
    Ask(Turn),
    /// Over a limit: reply this (the first time), else log it ignored.
    Refuse { tell: Option<String>, why: &'static str },
}

/// A person's question in flight, and the farm's run slots.
pub struct Turn {
    ctx: Ctx,
    user_id: String,
    runs: Arc<Semaphore>,
}

impl Turn {
    /// Wait for a free run slot, held until the returned permit drops.
    pub async fn run_slot(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.runs.clone().acquire_owned().await.ok()
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        with(&self.ctx, |f| {
            if let Some(p) = f.people.get_mut(&self.user_id) {
                p.busy = None;
            }
        });
    }
}

/// Take `user_id`'s question at `now`, or say why not.
pub fn admit(ctx: &Ctx, user_id: &str, now: DateTime<Utc>) -> Admit {
    with(ctx, |f| {
        let runs = f.runs.get_or_insert_with(|| Arc::new(Semaphore::new(AT_ONCE))).clone();
        let p = f.people.entry(user_id.to_owned()).or_default();
        while p.asked.front().is_some_and(|t| now - *t >= Duration::hours(1)) {
            p.asked.pop_front();
        }
        if let Some(told) = &mut p.busy {
            let tell = (!*told).then(|| BUSY.to_owned());
            *told = true;
            return Admit::Refuse { tell, why: "A question of theirs is being answered." };
        }
        if p.asked.len() >= PER_HOUR {
            let tell = p.capped_until.is_none_or(|t| now >= t).then(capped_text);
            if tell.is_some() {
                p.capped_until = p.asked.front().map(|t| *t + Duration::hours(1));
            }
            return Admit::Refuse { tell, why: "Too many questions this hour." };
        }
        p.asked.push_back(now);
        p.busy = Some(false);
        Admit::Ask(Turn { ctx: ctx.clone(), user_id: user_id.to_owned(), runs })
    })
}
