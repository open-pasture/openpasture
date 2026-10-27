//! The background job supervisor.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use op_core::Ctx;
use op_core::tasks::{TaskStatus, backoff};

#[test]
fn backoff_doubles_up_to_a_minute() {
    let secs: Vec<u64> = (1..=9).map(|n| backoff(n).as_secs()).collect();
    assert_eq!(secs, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
}

#[tokio::test]
async fn a_job_that_panics_is_restarted_and_its_error_kept() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let runs = Arc::new(AtomicU32::new(0));
    let r = runs.clone();
    ctx.supervise("test.flaky", Some(Duration::from_millis(20)), move |ctx| {
        let n = r.fetch_add(1, Ordering::SeqCst);
        async move {
            if n == 0 {
                panic!("index out of bounds");
            }
            loop {
                ctx.beat("test.flaky");
                tokio::select! {
                    _ = ctx.on_shutdown() => break,
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
            }
        }
    });
    assert_eq!(runs.load(Ordering::SeqCst), 1, "the first run starts before supervise returns");

    let job = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let job = ctx.tasks().list().into_iter().find(|t| t.name == "test.flaky").unwrap();
            if job.last_beat_at.is_some() {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("restarted and beating");
    assert_eq!(job.status, TaskStatus::Running);
    assert_eq!(job.restarts, 1);
    assert!(job.last_error.as_deref().is_some_and(|e| e.contains("index out of bounds")), "{job:?}");
    assert_eq!(runs.load(Ordering::SeqCst), 2);

    ctx.shutdown();
    assert!(ctx.tasks().join(Duration::from_secs(5)).await, "jobs stop on shutdown");
    let job = ctx.tasks().list().into_iter().find(|t| t.name == "test.flaky").unwrap();
    assert_eq!(job.status, TaskStatus::Stopped);
}

#[tokio::test]
async fn a_job_that_returns_early_is_restarted_but_not_after_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let runs = Arc::new(AtomicU32::new(0));
    let r = runs.clone();
    ctx.supervise("test.quits", None, move |_ctx| {
        r.fetch_add(1, Ordering::SeqCst);
        async {}
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while runs.load(Ordering::SeqCst) < 2 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("started again");
    let job = ctx.tasks().list().into_iter().find(|t| t.name == "test.quits").unwrap();
    assert_eq!(job.last_error.as_deref(), Some("stopped on its own"));
    ctx.shutdown();
    assert!(ctx.tasks().join(Duration::from_secs(5)).await);
    let after = runs.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert_eq!(runs.load(Ordering::SeqCst), after);
}

/// A job that ignores shutdown and keeps working, counting its ticks.
fn stubborn(ctx: &Ctx, name: &'static str) -> Arc<AtomicU32> {
    let ticks = Arc::new(AtomicU32::new(0));
    let t = ticks.clone();
    ctx.supervise(name, None, move |_ctx| {
        let t = t.clone();
        async move {
            loop {
                t.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        }
    });
    ticks
}

#[tokio::test]
async fn a_short_join_aborts_the_job_itself_not_just_its_supervisor() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let ticks = stubborn(&ctx, "test.stubborn");
    tokio::time::sleep(Duration::from_millis(20)).await;
    ctx.shutdown();
    assert!(!ctx.tasks().join(Duration::from_millis(20)).await, "it didn't stop by itself");
    let at_return = ticks.load(Ordering::SeqCst);
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(ticks.load(Ordering::SeqCst), at_return, "the job kept running after join returned (detached)");
}

#[tokio::test]
async fn a_critical_job_failing_again_and_again_is_down_since_its_first_failure() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    // Named as a real critical job; this Ctx runs nothing else.
    assert!(op_core::tasks::CRITICAL.contains(&"ingest.moves"));
    ctx.supervise("ingest.moves", None, |_ctx| async { panic!("boom") });
    ctx.supervise("test.fine", None, |ctx| async move { ctx.on_shutdown().await });
    let job = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let job = ctx.tasks().list().into_iter().find(|t| t.name == "ingest.moves").unwrap();
            if job.restarts >= 2 {
                return job;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("failed twice (backoff 1 s)");
    assert!(job.critical);
    let since = job.down_since.expect("down after a second failure without a healthy run");
    let now = op_core::time::now();
    assert!(!job.critical_down(now), "not for long yet");
    assert!(job.critical_down(since + chrono::Duration::from_std(op_core::tasks::CRITICAL_DOWN_AFTER).unwrap()));
    let fine = ctx.tasks().list().into_iter().find(|t| t.name == "test.fine").unwrap();
    assert!(!fine.critical && fine.down_since.is_none());
    ctx.shutdown();
    ctx.tasks().join(Duration::from_secs(5)).await;
}

#[tokio::test]
async fn one_failure_is_not_down() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let runs = Arc::new(AtomicU32::new(0));
    let r = runs.clone();
    ctx.supervise("ingest.escapes", Some(Duration::from_secs(5)), move |ctx| {
        let n = r.fetch_add(1, Ordering::SeqCst);
        async move {
            if n == 0 {
                panic!("once");
            }
            loop {
                ctx.beat("ingest.escapes");
                tokio::select! {
                    _ = ctx.on_shutdown() => break,
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {}
                }
            }
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while ctx.tasks().list().into_iter().find(|t| t.name == "ingest.escapes").unwrap().beats == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("restarted and beating");
    let job = ctx.tasks().list().into_iter().find(|t| t.name == "ingest.escapes").unwrap();
    assert_eq!((job.restarts, job.down_since), (1, None));
    ctx.shutdown();
    ctx.tasks().join(Duration::from_secs(5)).await;
}
