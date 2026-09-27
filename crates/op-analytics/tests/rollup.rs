//! Rolling days to Parquet: a day streams through in short write locks while
//! reports keep landing, an interrupted run merges without duplicates, health
//! rolls up and its battery history still shows, and pasture history is as
//! of the end of the range asked for.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use op_analytics::range::TimeRange;
use op_core::*;
use op_geo::{LonLat, Projection};
use serde_json::Value;
use tower::ServiceExt;

const ORIGIN: LonLat = [-93.62, 42.03];
const DAY_MS: i64 = 86_400_000;
const MIN: i64 = 60_000;

fn at(east: f64, north: f64) -> LonLat {
    Projection::new(ORIGIN).offset(east, north)
}

fn midnight(t: DateTime<Utc>) -> i64 {
    t.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc().timestamp_millis()
}

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
}

/// Paddocks A (0-100 m east) and B (100-200 m east), herd_1 of 10 cattle.
async fn app() -> App {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let now = time::now();
    let s = ctx.store();
    s.insert_farm(&Farm { id: "farm_1".into(), name: "Test".into(), timezone: "UTC".into(), center: ORIGIN, created_at: now }).await.unwrap();
    for (id, e0) in [("pad_a", 0.0), ("pad_b", 100.0)] {
        let geometry = Polygon::from_ring(vec![at(e0, 0.0), at(e0 + 100.0, 0.0), at(e0 + 100.0, 100.0), at(e0, 100.0)]);
        let p = Paddock {
            id: id.into(),
            name: id.into(),
            area_ha: geometry.area_ha(),
            geometry,
            status: PaddockStatus::Resting,
            notes: None,
            grazed_until: None,
            created_at: now,
            props: Default::default(),
        };
        s.insert_paddock(&p).await.unwrap();
    }
    let herd = Herd {
        id: "herd_1".into(),
        name: "Cows".into(),
        species: Species::Cattle,
        count: 10,
        paddock_id: None,
        autonomy: Autonomy::Propose,
        timer_minutes: 60,
        created_at: now,
    };
    s.insert_herd(&herd).await.unwrap();
    sqlx::query("INSERT INTO collars (id, name, herd_id, created_at) VALUES ('col_1', 'c1', 'herd_1', ?)")
        .bind(time::to_db(&(now - chrono::Duration::days(30))))
        .execute(ctx.db())
        .await
        .unwrap();
    let router = op_analytics::router().with_state(ctx.clone());
    App { _dir: dir, ctx, router }
}

impl App {
    async fn fixes(&self, collars: usize, from: i64, every_ms: i64, n: i64, p: LonLat) {
        sqlx::query(
            "WITH RECURSIVE c(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM c WHERE i < ?2 - 1),
                           s(j) AS (SELECT 0 UNION ALL SELECT j + 1 FROM s WHERE j < ?4 - 1)
             INSERT INTO fixes (collar_id, herd_id, animal_id, at, t, lon, lat, accuracy_m, sats)
             SELECT printf('col_%d', i + 1), 'herd_1', printf('ani_%d', i + 1), 'x', ?1 + j * ?3 + i, ?5, ?6, 3.0, 9
             FROM s CROSS JOIN c",
        )
        .bind(from)
        .bind(collars as i64)
        .bind(every_ms)
        .bind(n)
        .bind(p[0])
        .bind(p[1])
        .execute(self.ctx.db())
        .await
        .unwrap();
    }

    async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(self.ctx.db()).await.unwrap()
    }

    async fn get(&self, path: &str) -> Value {
        let res = self.router.clone().oneshot(Request::builder().uri(path).body(Body::empty()).unwrap()).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK, "{path}");
        serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap()
    }
}

fn parquet_rows(path: &std::path::Path) -> (i64, Vec<i64>) {
    use parquet::file::reader::{FileReader, SerializedFileReader};
    let r = SerializedFileReader::new(std::fs::File::open(path).unwrap()).unwrap();
    let m = r.metadata();
    (m.file_metadata().num_rows(), m.row_groups().iter().map(|g| g.num_rows()).collect())
}

fn q(ms: i64) -> String {
    time::to_db(&time::from_unix_ms(ms)).replace(':', "%3A")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_day_streams_to_parquet_in_short_write_locks_while_reports_land() {
    let app = app().await;
    let now = time::now();
    let day0 = midnight(now) - 6 * DAY_MS;
    // 250 collars, a fix every 5 s for 40 minutes: more rows than one chunk.
    app.fixes(250, day0 + 3_600_000, 5000, 480, at(50.0, 50.0)).await;
    let rows = 250 * 480;
    assert!(rows > op_analytics::rollup::CHUNK_ROWS as i64);
    let day = TimeRange::new(time::from_unix_ms(day0), time::from_unix_ms(day0 + DAY_MS));
    let hot = op_analytics::routes::track_points(&app.ctx, day, None, Some("herd_1"), 300).await.unwrap();

    let done = Arc::new(AtomicBool::new(false));
    let worst = Arc::new(AtomicU64::new(0));
    let reports = {
        let (ctx, done, worst) = (app.ctx.clone(), done.clone(), worst.clone());
        tokio::spawn(async move {
            let mut n = 0;
            while !done.load(Ordering::Relaxed) {
                let started = Instant::now();
                let mut tx = op_core::store::begin_immediate(ctx.db()).await.unwrap();
                sqlx::query("INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m) VALUES ('col_1', 'herd_1', 'x', ?, 0, 0, 3)")
                    .bind(time::now().timestamp_millis())
                    .execute(&mut *tx)
                    .await
                    .unwrap();
                tx.commit().await.unwrap();
                worst.fetch_max(started.elapsed().as_millis() as u64, Ordering::Relaxed);
                n += 1;
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            n
        })
    };
    let rolled = op_analytics::rollup::rollup(&app.ctx, 3, now).await.unwrap();
    done.store(true, Ordering::Relaxed);
    let landed = reports.await.unwrap();
    let worst = worst.load(Ordering::Relaxed);
    assert_eq!(rolled.iter().find(|r| r.table == "fixes").unwrap().rows as i64, rows);
    assert!(landed > 5, "{landed} reports landed while it ran");
    assert!(worst < 250, "a report waited {worst} ms");

    let (n, groups) = parquet_rows(&op_analytics::telemetry::day_path(app.ctx.data_dir(), "fixes", time::from_unix_ms(day0).date_naive()));
    assert_eq!(n, rows);
    assert!(groups.len() >= 2 && groups.iter().all(|g| *g as usize <= op_analytics::rollup::CHUNK_ROWS), "{groups:?}");
    assert_eq!(app.count(&format!("SELECT COUNT(*) FROM fixes WHERE t < {}", day0 + DAY_MS)).await, 0);
    // Dwell was summed on the stream: 250 collars in A, 479 gaps of 5 s each
    // plus the last fix's 30 minutes.
    let (collars, dwell): (i64, f64) = sqlx::query_as("SELECT COUNT(DISTINCT collar_id), SUM(dwell_s) FROM analytics_paddock_days WHERE paddock_id = 'pad_a'")
        .fetch_one(app.ctx.db())
        .await
        .unwrap();
    assert_eq!(collars, 250);
    assert!((dwell / 250.0 - (479.0 * 5.0 + 1800.0)).abs() < 1.0, "{dwell}");

    let cold = op_analytics::routes::track_points(&app.ctx, day, None, Some("herd_1"), 300).await.unwrap();
    assert_eq!(cold, hot, "the same tracks from Parquet as from SQLite");
    assert_eq!(cold.len(), 250);
}

#[tokio::test]
async fn an_interrupted_rollup_merges_again_without_duplicates() {
    let app = app().await;
    let now = time::now();
    let day0 = midnight(now) - 6 * DAY_MS;
    app.fixes(3, day0, 10 * MIN, 144, at(50.0, 50.0)).await;
    let before: Vec<(i64, String, i64)> = sqlx::query_as("SELECT id, collar_id, t FROM fixes ORDER BY id").fetch_all(app.ctx.db()).await.unwrap();
    op_analytics::rollup::rollup(&app.ctx, 3, now).await.unwrap();
    let file = op_analytics::telemetry::day_path(app.ctx.data_dir(), "fixes", time::from_unix_ms(day0).date_naive());
    assert_eq!(parquet_rows(&file).0, 432);

    // A run that wrote the file but stopped before its deletes: the rows are in both.
    for (id, collar, t) in &before {
        sqlx::query("INSERT INTO fixes (id, collar_id, herd_id, at, t, lon, lat, accuracy_m, sats) VALUES (?, ?, 'herd_1', 'x', ?, ?, ?, 3.0, 9)")
            .bind(id)
            .bind(collar)
            .bind(t)
            .bind(at(50.0, 50.0)[0])
            .bind(at(50.0, 50.0)[1])
            .execute(app.ctx.db())
            .await
            .unwrap();
    }
    // And a late fix for that day.
    app.fixes(1, day0 + 5 * MIN, MIN, 1, at(150.0, 50.0)).await;
    let rolled = op_analytics::rollup::rollup(&app.ctx, 3, now).await.unwrap();
    assert_eq!(rolled.len(), 1, "{rolled:?}");
    assert_eq!(parquet_rows(&file).0, 433, "one copy of each row, plus the late fix");
    assert_eq!(app.count("SELECT COUNT(*) FROM fixes").await, 0);
    let (_, res) = (0, op_analytics::sql::run(&app.ctx, "SELECT count(*) AS n, count(DISTINCT id) AS ids FROM fixes", 10).await.unwrap());
    assert_eq!(res.rows[0], vec![Value::from(433), Value::from(433)]);
}

#[tokio::test]
async fn health_rolls_up_and_its_battery_history_still_shows() {
    let app = app().await;
    let now = time::now();
    let day0 = midnight(now) - 6 * DAY_MS;
    for i in 0..48i64 {
        sqlx::query("INSERT INTO health (collar_id, herd_id, at, t, battery, fixes, cues) VALUES ('col_1', 'herd_1', 'x', ?, ?, 1, 0)")
            .bind(day0 + i * 30 * MIN)
            .bind(0.9 - i as f64 * 0.01)
            .execute(app.ctx.db())
            .await
            .unwrap();
    }
    let path = format!("/api/analytics/health?collar_id=col_1&from={}&to={}&bucket=1h", q(day0), q(day0 + DAY_MS));
    let before = app.get(&path).await;
    let battery = |v: &Value| v[0]["points"].as_array().unwrap().iter().map(|p| p["battery"].clone()).collect::<Vec<_>>();
    assert_eq!(battery(&before)[0], Value::from(0.895), "the mean of 0.9 and 0.89");
    assert!(battery(&before).iter().all(|b| b.is_number()));

    let rolled = op_analytics::rollup::rollup(&app.ctx, 3, now).await.unwrap();
    assert_eq!(rolled, vec![op_analytics::rollup::RolledDay { table: "health", date: time::from_unix_ms(day0).date_naive().to_string(), rows: 48 }]);
    assert_eq!(app.count("SELECT COUNT(*) FROM health").await, 0);
    assert_eq!(parquet_rows(&op_analytics::telemetry::day_path(app.ctx.data_dir(), "health", time::from_unix_ms(day0).date_naive())).0, 48);
    let after = app.get(&path).await;
    assert_eq!(battery(&after), battery(&before), "the same battery history from Parquet");
    assert_eq!(after[0]["summary"]["battery"], before[0]["summary"]["battery"]);
    // Health is a table the SQL console reads too, cold days included.
    let res = op_analytics::sql::run(&app.ctx, "SELECT count(*) FROM health", 10).await.unwrap();
    assert_eq!(res.rows[0][0], Value::from(48));
}

#[tokio::test]
async fn pasture_is_as_of_the_end_of_the_range() {
    let app = app().await;
    let now = time::now();
    let today = midnight(now);
    // Ten days ago a whole day in A (rolled), two days ago a whole day in B (still in SQLite).
    app.fixes(1, today - 10 * DAY_MS, 10 * MIN, 144, at(50.0, 50.0)).await;
    op_analytics::rollup::rollup(&app.ctx, 3, now).await.unwrap();
    app.fixes(1, today - 2 * DAY_MS, 10 * MIN, 144, at(150.0, 50.0)).await;

    let pad = |v: &Value, id: &str| v.as_array().unwrap().iter().find(|p| p["paddock_id"] == id).unwrap().clone();
    let now_view = app.get("/api/analytics/pasture?herd_id=herd_1").await;
    assert_eq!(pad(&now_view, "pad_a")["grazing_days"], 1);
    assert_eq!(pad(&now_view, "pad_b")["grazing_days"], 1);
    assert!(pad(&now_view, "pad_b")["rest_days"].as_f64().unwrap() < 2.0);

    // As of a week ago: B's day hadn't happened, A had rested since its day.
    let (from, to) = (today - 12 * DAY_MS, today - 7 * DAY_MS);
    let then = app.get(&format!("/api/analytics/pasture?herd_id=herd_1&from={}&to={}", q(from), q(to))).await;
    let (a, b) = (pad(&then, "pad_a"), pad(&then, "pad_b"));
    assert_eq!(a["grazing_days"], 1);
    assert_eq!(b["grazing_days"], 0);
    assert_eq!(b["last_grazed"], Value::Null, "nothing after the range counts");
    assert_eq!(b["rest_days"], Value::Null);
    let rest = a["rest_days"].as_f64().unwrap();
    assert!((2.0..=3.1).contains(&rest), "rest measured to the end of the range: {rest}");

    // A range ending before B's day, starting after A's: A still shows when it was last grazed.
    let (from, to) = (today - 6 * DAY_MS, today - 3 * DAY_MS);
    let gap = app.get(&format!("/api/analytics/pasture?herd_id=herd_1&from={}&to={}", q(from), q(to))).await;
    assert_eq!(pad(&gap, "pad_a")["grazing_days"], Value::Null, "no tracked day in the range");
    assert!(pad(&gap, "pad_a")["last_grazed"].is_string());
    assert_eq!(pad(&gap, "pad_b")["last_grazed"], Value::Null);
}

#[tokio::test]
async fn hot_dwell_summed_in_sqlite_matches_the_row_by_row_sum() {
    use op_analytics::metrics::{Dwell, PaddockIndex};
    use op_analytics::telemetry::{Scope, Source, each_fix, for_each, scan};

    let app = app().await;
    let now = time::now();
    let day = midnight(now) - DAY_MS;
    // Three collars, two herds, gaps short and long, across midnight, a fix stored with
    // no paddock (in B by its point), one with a paddock since deleted, one arriving late.
    let mut seed = 11u64;
    let mut rnd = |n: u64| {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (seed >> 33) % n
    };
    let mut rows = Vec::new();
    for (c, herd) in [("col_1", "herd_1"), ("col_2", "herd_1"), ("col_3", "herd_2")] {
        let mut t = day - 3 * 3_600_000;
        for i in 0..400 {
            t += [5_000, 60_000, 45 * MIN][rnd(10) as usize % 3 * (rnd(7) == 0) as usize] as i64 + rnd(3000) as i64;
            let (east, pad): (f64, Option<&str>) = match i % 7 {
                0 => (150.0, None),
                1 => (50.0, Some("pad_gone")),
                2 | 3 => (150.0, Some("pad_b")),
                _ => (50.0, Some("pad_a")),
            };
            rows.push((c, herd, t, at(east, 50.0), pad));
        }
    }
    let late = rows.remove(100);
    rows.push(late);
    for (c, herd, t, p, pad) in &rows {
        sqlx::query("INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m, paddock_id) VALUES (?, ?, 'x', ?, ?, ?, 3, ?)")
            .bind(c)
            .bind(herd)
            .bind(t)
            .bind(p[0])
            .bind(p[1])
            .bind(pad)
            .execute(app.ctx.db())
            .await
            .unwrap();
    }
    let index = PaddockIndex::new(&app.ctx.store().list_paddocks().await.unwrap());
    let (from, end) = (day - 2 * DAY_MS, now.timestamp_millis());
    for herd in [None, Some("herd_1")] {
        let mut dwell = Dwell::default();
        let range = TimeRange::new(time::from_unix_ms(from), time::from_unix_ms(end));
        let scope = Scope { collar_ids: None, herd_id: herd.map(str::to_owned) };
        for_each(scan(&app.ctx, "fixes", range, scope, Source::Hot), |b| {
            each_fix(b, |f| dwell.push(f.collar_id, f.herd_id, index.resolve(f.paddock_id, [f.lon, f.lat]), f.t));
        })
        .await
        .unwrap();
        let want = dwell.finish(end);
        let got = op_analytics::routes::hot_dwell(&app.ctx, from, end, herd, &index).await.unwrap();
        assert!(want.len() >= 8 && want.keys().any(|k| k.3.is_empty() || k.3 == "pad_b"), "{want:?}");
        assert_eq!(got, want, "herd {herd:?}");
    }
}
