//! A write transaction abandoned half-way through starting (the request that
//! wanted it was dropped: a collar gave up on its report) never leaves a
//! pooled connection holding SQLite's write lock. Found by stream L's 250-
//! collar runs: one such connection stalled every writer for 50 s and then
//! answered every begin with "non-zero transaction depth".

use std::time::Duration;

use op_core::store::begin_immediate;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_begin_given_up_half_way_leaves_no_transaction_behind() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = op_core::Ctx::open(dir.path()).await.unwrap();
    let pool = ctx.db().clone();
    sqlx::query("CREATE TABLE l_t (n INTEGER)").execute(&pool).await.unwrap();
    for i in 0..400u64 {
        // Give up after a few microseconds: some of these land between the
        // BEGIN and the end of starting the transaction.
        let _ = tokio::time::timeout(Duration::from_micros(i % 40), begin_immediate(&pool)).await;
        // Whatever happened, the next writer gets in at once and can write.
        let tx = tokio::time::timeout(Duration::from_secs(2), begin_immediate(&pool)).await;
        let mut tx = tx.unwrap_or_else(|_| panic!("round {i}: a writer waited 2 s for the lock")).unwrap_or_else(|e| panic!("round {i}: {e}"));
        sqlx::query("INSERT INTO l_t (n) VALUES (?)").bind(i as i64).execute(&mut *tx).await.unwrap();
        tx.commit().await.unwrap();
    }
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM l_t").fetch_one(&pool).await.unwrap();
    assert_eq!(n, 400);
}
