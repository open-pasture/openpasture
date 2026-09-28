//! The WAL is checkpointed in the background, not by the commit that happens
//! to cross SQLite's threshold (at 250 collars that was a collar's report,
//! waiting on the disk's sync): with the checkpointer running the WAL is
//! reused from its start and stays small; without it, commits leave the WAL
//! to grow up to the 64 MB backstop.

use std::time::Duration;

async fn wal_after_writes(checkpointer: bool) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let ctx = op_core::Ctx::open(dir.path()).await.unwrap();
    if checkpointer {
        op_core::store::spawn_checkpointer(&ctx, Duration::from_millis(20));
    }
    sqlx::query("CREATE TABLE l_blob (b BLOB)").execute(ctx.db()).await.unwrap();
    // 200 small commits of 40 KB: about 2,000 pages of WAL in all.
    let blob = vec![7u8; 40 * 1024];
    for _ in 0..200 {
        let mut tx = op_core::store::begin_immediate(ctx.db()).await.unwrap();
        sqlx::query("INSERT INTO l_blob (b) VALUES (?)").bind(&blob).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
        tokio::time::sleep(Duration::from_millis(3)).await;
    }
    let size = std::fs::metadata(dir.path().join("openpasture.db-wal")).map(|m| m.len()).unwrap_or(0);
    ctx.shutdown();
    size
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_wal_is_checkpointed_in_the_background_and_stays_small() {
    let without = wal_after_writes(false).await;
    let with = wal_after_writes(true).await;
    assert!(without > 7 * 1024 * 1024, "commits alone leave it to grow: {without} bytes");
    assert!(with < 3 * 1024 * 1024, "the checkpointer keeps it small: {with} bytes");
}
