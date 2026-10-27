//! Supervised background jobs. A job that panics or returns before shutdown
//! is logged, recorded and started again after a backoff, so one bad
//! iteration can't quietly end the alert engine or the move driver for the
//! life of the process. Jobs report a heartbeat each time round their loop;
//! `/healthz` and `/api/server` show each job's state.
//!
//! ```ignore
//! ctx.supervise("ingest.moves", Some(Duration::from_secs(5)), |ctx| {
//!     let mut rx = ctx.subscribe(); // runs now, before `supervise` returns
//!     async move {
//!         loop {
//!             tokio::select! {
//!                 _ = ctx.on_shutdown() => break,
//!                 _ = tick.tick() => { ctx.beat("ingest.moves"); … }
//!             }
//!         }
//!     }
//! });
//! ```

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Mutex;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::Serialize;
use tokio::task::{AbortHandle, JoinHandle};

use crate::Ctx;
use crate::time::now;

/// First wait before a restart; doubles up to [`MAX_BACKOFF`].
pub const MIN_BACKOFF: Duration = Duration::from_secs(1);
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A run this long resets the backoff.
const HEALTHY_RUN: Duration = Duration::from_secs(300);

/// A job is stale when it hasn't beaten for this many of its intervals (and
/// at least [`STALE_FLOOR`]).
const STALE_INTERVALS: u32 = 3;
const STALE_FLOOR: Duration = Duration::from_secs(60);

/// Jobs the farm depends on to keep animals safe and the farmer told: moves,
/// escapes, schedules and activations driving fences, the alert engine and
/// the alert senders. One that stays down past [`CRITICAL_DOWN_AFTER`] makes
/// `/healthz` answer 503, so Docker and monitors see it.
pub const CRITICAL: &[&str] =
    &["alerts.engine", "engine.scheduler", "ingest.activations", "ingest.escapes", "ingest.moves", "ingest.schedules", "notify.workers"];
/// How long a critical job may fail (restart again and again, or stop
/// beating) before the server reports itself down.
pub const CRITICAL_DOWN_AFTER: Duration = Duration::from_secs(300);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Running, and beating on time if it beats.
    Running,
    /// Running but no heartbeat for several intervals: stuck or very slow.
    Stale,
    /// Panicked or stopped on its own; waiting to start again.
    Restarting,
    /// Stopped because the server is shutting down.
    Stopped,
}

/// One job as `/api/server` shows it.
#[derive(Debug, Clone, Serialize)]
pub struct TaskInfo {
    pub name: &'static str,
    pub status: TaskStatus,
    pub started_at: DateTime<Utc>,
    /// Expected heartbeat interval in seconds, if the job beats.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub every_s: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_beat_at: Option<DateTime<Utc>>,
    pub restarts: u32,
    /// Heartbeats since the server started.
    pub beats: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error_at: Option<DateTime<Utc>>,
    /// One of [`CRITICAL`].
    pub critical: bool,
    /// Failing since then: it has failed twice or more without a healthy run
    /// of five minutes in between, or it has stopped beating (stale).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub down_since: Option<DateTime<Utc>>,
}

impl TaskInfo {
    /// A critical job failing for longer than [`CRITICAL_DOWN_AFTER`].
    pub fn critical_down(&self, at: DateTime<Utc>) -> bool {
        self.critical && self.down_since.is_some_and(|since| (at - since).to_std().is_ok_and(|d| d >= CRITICAL_DOWN_AFTER))
    }
}

struct Entry {
    every: Option<Duration>,
    started_at: DateTime<Utc>,
    last_beat_at: Option<DateTime<Utc>>,
    restarts: u32,
    beats: u64,
    restarting: bool,
    stopped: bool,
    last_error: Option<String>,
    last_error_at: Option<DateTime<Utc>>,
    /// Failures in a row (no healthy run between) and when the first was.
    failures: u32,
    failing_since: Option<DateTime<Utc>>,
    /// The running copy of the job, aborted with it.
    child: Option<AbortHandle>,
}

impl Entry {
    fn new(every: Option<Duration>) -> Self {
        Entry {
            every,
            started_at: now(),
            last_beat_at: None,
            restarts: 0,
            beats: 0,
            restarting: false,
            stopped: false,
            last_error: None,
            last_error_at: None,
            failures: 0,
            failing_since: None,
            child: None,
        }
    }
}

/// The job table, one per [`Ctx`].
#[derive(Default)]
pub struct Tasks {
    jobs: Mutex<BTreeMap<&'static str, Entry>>,
    handles: Mutex<Vec<JoinHandle<()>>>,
}

impl Tasks {
    fn with<R>(&self, name: &str, f: impl FnOnce(&mut Entry) -> R) -> Option<R> {
        self.jobs.lock().unwrap_or_else(|e| e.into_inner()).get_mut(name).map(f)
    }

    pub fn beat(&self, name: &str) {
        self.with(name, |e| {
            e.last_beat_at = Some(now());
            e.beats += 1;
        });
    }

    /// Every job, by name.
    pub fn list(&self) -> Vec<TaskInfo> {
        let at = now();
        self.jobs
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|(name, e)| TaskInfo {
                name,
                status: status_of(e, at),
                started_at: e.started_at,
                every_s: e.every.map(|d| d.as_secs()),
                last_beat_at: e.last_beat_at,
                restarts: e.restarts,
                beats: e.beats,
                last_error: e.last_error.clone(),
                last_error_at: e.last_error_at,
                critical: CRITICAL.contains(name),
                down_since: down_since(e, at),
            })
            .collect()
    }

    /// Wait up to `limit` for every job to stop (after [`Ctx::shutdown`]),
    /// then abort what is left, the jobs themselves as well as their
    /// supervisors, and wait (up to [`ABORT_WAIT`]) for them to go. True
    /// when all stopped in time.
    pub async fn join(&self, limit: Duration) -> bool {
        let handles: Vec<JoinHandle<()>> = std::mem::take(&mut *self.handles.lock().unwrap_or_else(|e| e.into_inner()));
        let aborts: Vec<_> = handles.iter().map(|h| h.abort_handle()).collect();
        let mut all = futures::future::join_all(handles);
        if tokio::time::timeout(limit, &mut all).await.is_ok() {
            return true;
        }
        let children: Vec<AbortHandle> = self.jobs.lock().unwrap_or_else(|e| e.into_inner()).values().filter_map(|e| e.child.clone()).collect();
        for a in aborts.iter().chain(&children) {
            a.abort();
        }
        // An aborted task stops at its next await; one stuck in blocking code
        // can't be stopped, so don't wait for it forever.
        let gone = async {
            all.await;
            while !children.iter().all(AbortHandle::is_finished) {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        };
        if tokio::time::timeout(ABORT_WAIT, gone).await.is_err() {
            tracing::warn!("a background job did not stop when aborted");
        }
        false
    }
}

/// How long [`Tasks::join`] waits for aborted jobs to go.
pub const ABORT_WAIT: Duration = Duration::from_secs(1);

/// Aborts the job when its supervisor ends, whether it returns or is aborted.
struct AbortOnDrop(AbortHandle);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

fn stale_limit(every: Duration) -> Duration {
    (every * STALE_INTERVALS).max(STALE_FLOOR)
}

fn down_since(e: &Entry, at: DateTime<Utc>) -> Option<DateTime<Utc>> {
    if e.stopped {
        return None;
    }
    // Failing again and again: down from the first failure, until a run lasts.
    let recovered = !e.restarting && (at - e.started_at).to_std().is_ok_and(|d| d >= HEALTHY_RUN);
    if e.failures >= 2 && !recovered {
        return e.failing_since;
    }
    if !e.restarting
        && let Some(every) = e.every
    {
        let since = e.last_beat_at.unwrap_or(e.started_at);
        if (at - since).to_std().is_ok_and(|d| d > stale_limit(every)) {
            return Some(since);
        }
    }
    None
}

fn status_of(e: &Entry, at: DateTime<Utc>) -> TaskStatus {
    if e.stopped {
        return TaskStatus::Stopped;
    }
    if e.restarting {
        return TaskStatus::Restarting;
    }
    if let Some(every) = e.every {
        let since = e.last_beat_at.unwrap_or(e.started_at);
        if (at - since).to_std().is_ok_and(|d| d > stale_limit(every)) {
            return TaskStatus::Stale;
        }
    }
    TaskStatus::Running
}

fn panic_message(e: tokio::task::JoinError) -> String {
    if !e.is_panic() {
        return e.to_string();
    }
    let p = e.into_panic();
    if let Some(s) = p.downcast_ref::<&str>() {
        format!("panicked: {s}")
    } else if let Some(s) = p.downcast_ref::<String>() {
        format!("panicked: {s}")
    } else {
        "panicked".into()
    }
}

/// The wait after `failures` failed runs in a row.
pub fn backoff(failures: u32) -> Duration {
    MIN_BACKOFF.saturating_mul(1u32 << failures.saturating_sub(1).min(16)).min(MAX_BACKOFF)
}

impl Ctx {
    /// Run a background job under supervision until shutdown. `run` is
    /// called now for the first run (so anything it does before its future,
    /// like subscribing to events, is done when this returns) and again for
    /// each restart. `every` is how often the job calls [`Ctx::beat`]; `None`
    /// for a job that doesn't.
    pub fn supervise<F, Fut>(&self, name: &'static str, every: Option<Duration>, run: F)
    where
        F: Fn(Ctx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let tasks = self.tasks();
        tasks.jobs.lock().unwrap_or_else(|e| e.into_inner()).insert(name, Entry::new(every));
        let first = run(self.clone());
        let ctx = self.clone();
        let handle = tokio::spawn(async move {
            let mut next = Some(first);
            loop {
                let fut = match next.take() {
                    Some(f) => f,
                    None => run(ctx.clone()),
                };
                let began = std::time::Instant::now();
                let mut child = tokio::spawn(fut);
                let _abort = AbortOnDrop(child.abort_handle());
                ctx.tasks().with(name, |e| e.child = Some(child.abort_handle()));
                let outcome = tokio::select! {
                    r = &mut child => Some(r),
                    _ = ctx.on_shutdown() => None,
                };
                let outcome = match outcome {
                    Some(r) => r,
                    None => {
                        // Let the job finish its own shutdown branch.
                        let _ = tokio::time::timeout(Duration::from_secs(10), &mut child).await;
                        child.abort();
                        break;
                    }
                };
                if ctx.is_shutting_down() {
                    break;
                }
                let why = match outcome {
                    Ok(()) => "stopped on its own".to_string(),
                    Err(e) => panic_message(e),
                };
                let healthy_run = began.elapsed() >= HEALTHY_RUN;
                let failures = ctx
                    .tasks()
                    .with(name, |e| {
                        if healthy_run || e.failures == 0 {
                            e.failures = 0;
                            e.failing_since = Some(now());
                        }
                        e.failures += 1;
                        e.failures
                    })
                    .unwrap_or(1);
                let wait = backoff(failures);
                tracing::error!(task = name, restart_in_s = wait.as_secs(), critical = CRITICAL.contains(&name), "background task {why}");
                ctx.tasks().with(name, |e| {
                    e.restarts += 1;
                    e.restarting = true;
                    e.last_error = Some(why);
                    e.last_error_at = Some(now());
                });
                tokio::select! {
                    _ = tokio::time::sleep(wait) => {}
                    _ = ctx.on_shutdown() => break,
                }
                ctx.tasks().with(name, |e| {
                    e.restarting = false;
                    e.started_at = now();
                    e.last_beat_at = None;
                });
            }
            ctx.tasks().with(name, |e| e.stopped = true);
        });
        tasks.handles.lock().unwrap_or_else(|e| e.into_inner()).push(handle);
    }

    /// Record a heartbeat for a supervised job.
    pub fn beat(&self, name: &str) {
        self.tasks().beat(name);
    }
}
