//! One Iowa day at full scale: 250 collars fixing every 5 s is 4.32 M fixes,
//! plus a health row a minute per collar and some cues. The day's tracks
//! answer in under 3 s from SQLite and from Parquet, and rolling the day to
//! Parquet never holds the write lock long enough to stall a collar report
//! (250 ms) and keeps memory growth under 200 MB. Alone in its own test
//! binary, so the process's memory is this test's.
//!
//! It takes 2-4 minutes in a debug build, so it runs on request:
//! `cargo test -p op-analytics --test scale -- --ignored --nocapture`.
//! `tests/rollup.rs` checks the same behaviour at a smaller scale on every run.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chrono::Duration as Days;
use op_analytics::range::TimeRange;
use op_core::*;

const COLLARS: i64 = 250;
const FIXES_PER_COLLAR: i64 = 17_280; // every 5 s
const HEALTH_PER_COLLAR: i64 = 1_440; // every minute
const DAY_MS: i64 = 86_400_000;

/// Resident memory of this process, in kB (macOS and Linux `ps`).
fn rss_kb() -> u64 {
    let out = std::process::Command::new("ps").args(["-o", "rss=", "-p", &std::process::id().to_string()]).output().unwrap();
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap_or(0)
}

/// Indexes of `table` as the migrations made them, dropped for a bulk load
/// and made again after it (much faster than growing them row by row).
async fn without_indexes(ctx: &Ctx, table: &str) -> Vec<String> {
    let ddl: Vec<(String, String)> = sqlx::query_as("SELECT name, sql FROM sqlite_master WHERE type = 'index' AND tbl_name = ? AND sql IS NOT NULL")
        .bind(table)
        .fetch_all(ctx.db())
        .await
        .unwrap();
    for (name, _) in &ddl {
        sqlx::query(&format!("DROP INDEX {name}")).execute(ctx.db()).await.unwrap();
    }
    ddl.into_iter().map(|(_, sql)| sql).collect()
}

async fn fill_day(ctx: &Ctx, day0: i64) {
    let mut ddl = without_indexes(ctx, "fixes").await;
    ddl.extend(without_indexes(ctx, "health").await);
    // Rows in time order, the way 250 collars report them.
    sqlx::query(
        "WITH RECURSIVE c(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM c WHERE i < ?2 - 1),
                       s(j) AS (SELECT 0 UNION ALL SELECT j + 1 FROM s WHERE j < ?3 - 1)
         INSERT INTO fixes (collar_id, herd_id, animal_id, at, t, lon, lat, accuracy_m, sats, cn0, state, paddock_id)
         SELECT printf('col_%03d', i), 'herd_1', printf('ani_%03d', i),
                'x', ?1 + j * 5000 + i * 17,
                -93.6230 + (i % 16) * 0.0002 + (j % 50) * 0.000001, 42.0310 + (i / 16) * 0.0002, 3.5, 9, 38.0, 'inside', 'pad_1'
         FROM s CROSS JOIN c",
    )
    .bind(day0)
    .bind(COLLARS)
    .bind(FIXES_PER_COLLAR)
    .execute(ctx.db())
    .await
    .unwrap();
    sqlx::query(
        "WITH RECURSIVE c(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM c WHERE i < ?2 - 1),
                       s(j) AS (SELECT 0 UNION ALL SELECT j + 1 FROM s WHERE j < ?3 - 1)
         INSERT INTO health (collar_id, herd_id, at, t, battery, sats, cn0, fixes, cues, rsrp_dbm)
         SELECT printf('col_%03d', i), 'herd_1', 'x',
                ?1 + j * 60000 + i * 101, 0.9 - j * 0.0001, 9, 38.0, 12, 0, -97.0
         FROM s CROSS JOIN c",
    )
    .bind(day0)
    .bind(COLLARS)
    .bind(HEALTH_PER_COLLAR)
    .execute(ctx.db())
    .await
    .unwrap();
    for sql in ddl {
        sqlx::query(&sql).execute(ctx.db()).await.unwrap();
    }
    for i in 0..COLLARS {
        sqlx::query("INSERT INTO cues (collar_id, herd_id, animal_id, at, t, level, margin_m, kind) VALUES (?, 'herd_1', ?, 'x', ?, 1, 2.0, 'warn')")
            .bind(format!("col_{i:03}"))
            .bind(format!("ani_{i:03}"))
            .bind(day0 + 3_600_000 + i * 1000)
            .execute(ctx.db())
            .await
            .unwrap();
    }
}

async fn count(ctx: &Ctx, sql: &str, day0: i64) -> i64 {
    sqlx::query_scalar(sql).bind(day0).bind(day0 + DAY_MS).fetch_one(ctx.db()).await.unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a full 4.3 M-fix day takes minutes in debug; run with --ignored"]
async fn a_day_of_250_collars_rolls_to_parquet_in_short_locks_and_its_tracks_stay_fast() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let now = time::now();
    let day0 = (now - Days::days(6)).date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis();
    let t = Instant::now();
    fill_day(&ctx, day0).await;
    eprintln!("filled a day: {:?}", t.elapsed());
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM fixes WHERE t >= ? AND t < ?", day0).await, COLLARS * FIXES_PER_COLLAR);

    let day = TimeRange::new(time::from_unix_ms(day0), time::from_unix_ms(day0 + DAY_MS));
    let t = Instant::now();
    let hot = op_analytics::routes::track_points(&ctx, day, None, Some("herd_1"), 300).await.unwrap();
    let hot_s = t.elapsed();
    eprintln!("tracks from SQLite: {hot_s:?}");
    assert_eq!(hot.len(), COLLARS as usize);
    assert!(hot.iter().all(|t| (300..=302).contains(&t.points.len())), "{}", hot[0].points.len());
    assert!(hot_s < Duration::from_secs(3), "tracks from SQLite took {hot_s:?}");
    // For the notes: pasture and health read every fix of the hot days.
    let t = Instant::now();
    op_analytics::routes::paddock_pasture(&ctx, TimeRange::new(time::from_unix_ms(day0 - 80 * DAY_MS), now), Some("herd_1")).await.unwrap();
    eprintln!("pasture over a hot day: {:?}", t.elapsed());

    // Roll the day while a collar report lands every 20 ms, timing each.
    let done = Arc::new(AtomicBool::new(false));
    let worst_ms = Arc::new(AtomicU64::new(0));
    let reports = {
        let (ctx, done, worst_ms) = (ctx.clone(), done.clone(), worst_ms.clone());
        tokio::spawn(async move {
            let mut n = 0u64;
            while !done.load(Ordering::Relaxed) {
                let started = Instant::now();
                let mut tx = op_core::store::begin_immediate(ctx.db()).await.unwrap();
                sqlx::query("INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m) VALUES ('col_000', 'herd_1', 'x', ?, -93.62, 42.03, 3.0)")
                    .bind(time::now().timestamp_millis())
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                worst_ms.fetch_max(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                n += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            n
        })
    };
    let peak_kb = Arc::new(AtomicU64::new(0));
    let sampler = {
        let (done, peak_kb) = (done.clone(), peak_kb.clone());
        std::thread::spawn(move || {
            while !done.load(Ordering::Relaxed) {
                peak_kb.fetch_max(rss_kb(), Ordering::Relaxed);
                std::thread::sleep(Duration::from_millis(50));
            }
        })
    };
    let before_kb = rss_kb();
    let t = Instant::now();
    let rolled = op_analytics::rollup::rollup(&ctx, 3, now).await.unwrap();
    let roll_s = t.elapsed();
    done.store(true, Ordering::Relaxed);
    let reported = reports.await.unwrap();
    sampler.join().unwrap();
    let (worst, grew_mb) = (worst_ms.load(Ordering::Relaxed), peak_kb.load(Ordering::Relaxed).saturating_sub(before_kb) / 1024);
    eprintln!("rollup: {roll_s:?}, {reported} reports during it, slowest {worst} ms, memory grew {grew_mb} MB, {rolled:?}");
    assert_eq!(rolled.iter().find(|r| r.table == "fixes").map(|r| r.rows), Some((COLLARS * FIXES_PER_COLLAR) as usize));
    assert_eq!(rolled.iter().find(|r| r.table == "health").map(|r| r.rows), Some((COLLARS * HEALTH_PER_COLLAR) as usize));
    assert!(reported > 20, "reports kept landing: {reported}");
    assert!(worst < 250, "a report waited {worst} ms for the write lock");
    assert!(grew_mb < 200, "memory grew {grew_mb} MB");

    // SQLite no longer holds the day; Parquet does, health included.
    for table in ["fixes", "cues", "health"] {
        assert_eq!(count(&ctx, &format!("SELECT COUNT(*) FROM {table} WHERE t >= ? AND t < ?"), day0).await, 0, "{table}");
    }
    let date = time::from_unix_ms(day0).date_naive();
    let health_file = op_analytics::telemetry::day_path(ctx.data_dir(), "health", date);
    let reader = parquet::file::reader::SerializedFileReader::new(std::fs::File::open(&health_file).unwrap()).unwrap();
    assert_eq!(parquet::file::reader::FileReader::metadata(&reader).file_metadata().num_rows(), COLLARS * HEALTH_PER_COLLAR);
    let fixes_file = op_analytics::telemetry::day_path(ctx.data_dir(), "fixes", date);
    let reader = parquet::file::reader::SerializedFileReader::new(std::fs::File::open(&fixes_file).unwrap()).unwrap();
    let meta = parquet::file::reader::FileReader::metadata(&reader);
    assert_eq!(meta.file_metadata().num_rows(), COLLARS * FIXES_PER_COLLAR);
    assert!(meta.row_groups().iter().all(|rg| rg.num_rows() as usize <= op_analytics::rollup::CHUNK_ROWS), "a row group per chunk");

    // The same tracks from Parquet, just as fast.
    let t = Instant::now();
    let cold = op_analytics::routes::track_points(&ctx, day, None, Some("herd_1"), 300).await.unwrap();
    let cold_s = t.elapsed();
    eprintln!("tracks from Parquet: {cold_s:?}");
    assert_eq!(cold, hot);
    assert!(cold_s < Duration::from_secs(3), "tracks from Parquet took {cold_s:?}");
}
