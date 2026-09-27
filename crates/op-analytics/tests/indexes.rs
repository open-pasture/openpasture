//! The hot paths use the indexes meant for them (`EXPLAIN QUERY PLAN` on the
//! exact statements the code runs): the decision scheduler, the rollup's
//! finds, reads and deletes, tracks' seeks and battery history.

use op_core::Ctx;
use sqlx::Row;

async fn plan(ctx: &Ctx, sql: &str) -> String {
    let rows = sqlx::query(&format!("EXPLAIN QUERY PLAN {sql}")).fetch_all(ctx.db()).await.unwrap();
    rows.iter().map(|r| r.get::<String, _>("detail")).collect::<Vec<_>>().join(" | ")
}

async fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    (dir, ctx)
}

fn uses(plan: &str, index: &str) -> bool {
    plan.contains(&format!("USING INDEX {index} ")) || plan.contains(&format!("USING COVERING INDEX {index} ")) || plan.ends_with(index)
}

#[tokio::test]
async fn decisions_are_found_by_status_and_by_herd_and_status() {
    let (_d, ctx) = ctx().await;
    // op-engine's due-decision scan and the herd's open proposal.
    let p = plan(&ctx, "SELECT * FROM decisions WHERE status = ? ORDER BY created_at").await;
    assert!(uses(&p, "decisions_status_apply"), "{p}");
    let p = plan(&ctx, "UPDATE decisions SET apply_at = ? WHERE herd_id = ? AND status = 'proposed' AND action = 'MOVE'").await;
    assert!(uses(&p, "decisions_herd_status"), "{p}");
}

#[tokio::test]
async fn the_rollup_finds_reads_and_deletes_a_day_by_index() {
    let (_d, ctx) = ctx().await;
    for table in ["fixes", "cues", "health"] {
        let p = plan(&ctx, &format!("SELECT MIN(t) FROM {table} WHERE t < ?")).await;
        assert!(uses(&p, &format!("{table}_t")), "{table}: {p}");
        let schema = op_analytics::schema::table_schema(ctx.db(), table).await.unwrap();
        let p = plan(&ctx, &op_analytics::rollup::read_sql(&schema)).await;
        assert!(uses(&p, &format!("{table}_collar_t")) && !p.contains("TEMP B-TREE"), "{table}: {p}");
        let p = plan(&ctx, &op_analytics::rollup::delete_sql(table)).await;
        assert!(uses(&p, &format!("{table}_t")), "{table}: {p}");
        let p = plan(&ctx, &format!("SELECT collar_id FROM {table} WHERE collar_id > ? ORDER BY collar_id LIMIT 1")).await;
        assert!(uses(&p, &format!("{table}_collar_t")), "{table}: {p}");
    }
}

#[tokio::test]
async fn tracks_seek_each_bucket_by_index() {
    let (_d, ctx) = ctx().await;
    let p = plan(&ctx, op_analytics::routes::HERD_NEXT_COLLAR_SQL).await;
    assert!(uses(&p, "fixes_herd_collar_t"), "{p}");
    for (herd, index) in [(true, "fixes_herd_collar_t"), (false, "fixes_collar_t")] {
        for sql in op_analytics::routes::hot_track_sql(herd) {
            let p = plan(&ctx, &sql).await;
            assert!(uses(&p, index) && !p.contains("TEMP B-TREE"), "herd {herd}: {p}");
            assert!(!p.contains("SCAN fixes"), "herd {herd}: {p}");
        }
    }
}

#[tokio::test]
async fn battery_history_reads_by_collar_and_time() {
    let (_d, ctx) = ctx().await;
    let p = plan(&ctx, &op_analytics::routes::battery_sql(3)).await;
    assert!(uses(&p, "health_collar_t") || uses(&p, "health_t"), "{p}");
    assert!(!p.contains("SCAN health"), "{p}");
    let p = plan(&ctx, op_analytics::routes::BATTERY_BEFORE_SQL).await;
    assert!(uses(&p, "health_collar_t"), "{p}");
}

#[tokio::test]
async fn pasture_sums_each_collars_dwell_by_index() {
    let (_d, ctx) = ctx().await;
    for (herd, index) in [(true, "fixes_herd_collar_t"), (false, "fixes_collar_t")] {
        let p = plan(&ctx, &op_analytics::routes::hot_dwell_sql(herd)).await;
        assert!(uses(&p, index), "herd {herd}: {p}");
        assert!(!p.contains("SCAN fixes"), "herd {herd}: {p}");
    }
}
