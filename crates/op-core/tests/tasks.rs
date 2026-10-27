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
