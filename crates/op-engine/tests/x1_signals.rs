//! Rest days and herd position at 250 collars without walking the hot fixes:
//! the newest fix per (herd, paddock) is kept by a trigger as fixes land, so
//! "last grazed" is one row per paddock and matches a full scan; fixes that
//! landed outside every paddock count for a paddock drawn over them later; a
//! window larger than the sample is read by index seeks per collar and bucket.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use http_body_util::BodyExt;
use op_core::{Ctx, Herd, Identity, Paddock, Via, time};
use op_engine::signals::{self, BUCKETS_SQL, COLLAR_IN_WINDOW_SQL, COUNT_UP_TO_SQL, MAX_FIX_ROWS, NEXT_COLLAR_SQL};
use serde_json::{Value, json};
use sqlx::Row;
use tower::ServiceExt;

struct T {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    app: Router,
    herd: Herd,
}

fn square(lon: f64, lat: f64) -> Value {
    json!({ "type": "Polygon", "coordinates": [[[lon, lat], [lon + 0.005, lat], [lon + 0.005, lat + 0.0036], [lon, lat + 0.0036], [lon, lat]]] })
}

async fn ok(app: &Router, method: &str, path: &str, body: Value) -> Value {
    let req = Request::builder().method(method).uri(path).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
    let res = app.clone().oneshot(req).await.unwrap();
    let status = res.status();
    let v: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap_or(Value::Null);
    assert!(status.is_success() || status == StatusCode::NO_CONTENT, "{method} {path}: {status} {v}");
    v
}

/// P1, P2 and P3 near Ames, the herd Cows in P1.
async fn setup() -> T {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let app = Router::new().merge(op_core::router()).merge(op_ingest::router()).merge(op_engine::router()).with_state(ctx.clone());
    let app = op_core::with_identity(app, Identity::owner(Via::Local));
    ok(&app, "POST", "/api/farm", json!({ "name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03] })).await;
    let p1 = ok(&app, "POST", "/api/paddocks", json!({ "name": "P1", "geometry": square(-93.625, 42.03) })).await;
    ok(&app, "POST", "/api/paddocks", json!({ "name": "P2", "geometry": square(-93.62, 42.03) })).await;
    ok(&app, "POST", "/api/paddocks", json!({ "name": "P3", "geometry": square(-93.625, 42.0336) })).await;
    let herd = ok(&app, "POST", "/api/herds", json!({ "name": "Cows", "species": "cattle", "count": 250, "paddock_id": p1["id"] })).await;
    T { _dir: dir, ctx, app, herd: serde_json::from_value(herd).unwrap() }
}

impl T {
    async fn ok(&self, method: &str, path: &str, body: Value) -> Value {
        ok(&self.app, method, path, body).await
    }

    async fn paddocks(&self) -> Vec<Paddock> {
        self.ctx.store().list_paddocks().await.unwrap()
    }

    async fn id_of(&self, name: &str) -> String {
        self.paddocks().await.into_iter().find(|p| p.name == name).unwrap().id
    }

    /// A fix as ingest stores it (paddock found then, or none).
    async fn fix(&self, collar: &str, at: DateTime<Utc>, point: [f64; 2], paddock: Option<&str>) {
        sqlx::query(
            "INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m, sats, state, paddock_id) VALUES (?, ?, ?, ?, ?, ?, 3.0, 9, 'inside', ?)",
        )
        .bind(collar)
        .bind(&self.herd.id)
        .bind(time::to_db(&at))
        .bind(time::unix_ms(&at))
        .bind(point[0])
        .bind(point[1])
        .bind(paddock)
        .execute(self.ctx.db())
        .await
        .unwrap();
    }

    async fn last_grazed(&self, now: DateTime<Utc>) -> std::collections::BTreeMap<String, Option<DateTime<Utc>>> {
        signals::last_grazed(&self.ctx, Some(&self.herd), &self.paddocks().await, None, &[], now).await.unwrap()
    }
}

const IN_P1: [f64; 2] = [-93.6225, 42.0318];
const IN_P2: [f64; 2] = [-93.6175, 42.0318];
const IN_P3: [f64; 2] = [-93.6225, 42.0354];
/// East of P2: in no paddock (yet).
const LANE: [f64; 2] = [-93.6125, 42.0318];

fn ms(t: DateTime<Utc>) -> DateTime<Utc> {
    time::from_unix_ms(time::unix_ms(&t))
}

#[tokio::test]
async fn rest_days_read_the_newest_fix_per_paddock_and_match_a_full_scan() {
    let t = setup().await;
    let (p1, p2, p3) = (t.id_of("P1").await, t.id_of("P2").await, t.id_of("P3").await);
    let now = ms(time::now());
    t.fix("col_a", now - Duration::hours(5), IN_P1, Some(&p1)).await;
    t.fix("col_a", now - Duration::hours(4), IN_P1, Some(&p1)).await;
    t.fix("col_b", now - Duration::hours(3), IN_P2, Some(&p2)).await;
    // A late fix, older than what P1 and P2 have: nothing moves back.
    t.fix("col_a", now - Duration::hours(6), IN_P2, Some(&p2)).await;
    // In the lane, outside every paddock.
    t.fix("col_b", now - Duration::hours(1), LANE, None).await;
    // Far older than the lookback: not grazing.
    t.fix("col_c", now - Duration::days(200), IN_P3, Some(&p3)).await;

    let kept: Vec<(String, i64)> = sqlx::query_as("SELECT paddock_id, last_t FROM fix_paddock_last WHERE herd_id = ? ORDER BY paddock_id")
        .bind(&t.herd.id)
        .fetch_all(t.ctx.db())
        .await
        .unwrap();
    let mut want = vec![
        (String::new(), time::unix_ms(&(now - Duration::hours(1)))),
        (p1.clone(), time::unix_ms(&(now - Duration::hours(4)))),
        (p2.clone(), time::unix_ms(&(now - Duration::hours(3)))),
        (p3.clone(), time::unix_ms(&(now - Duration::days(200)))),
    ];
    want.sort();
    assert_eq!(kept, want);

    let last = t.last_grazed(now).await;
    assert_eq!(last[&p1], Some(now - Duration::hours(4)));
    assert_eq!(last[&p2], Some(now - Duration::hours(3)));
    assert_eq!(last[&p3], None, "200 days back is outside the lookback");

    // The same as reading every fix.
    let scan = sqlx::query("SELECT paddock_id, MAX(t) FROM fixes WHERE herd_id = ? AND paddock_id IS NOT NULL AND t >= ? GROUP BY paddock_id")
        .bind(&t.herd.id)
        .bind(time::unix_ms(&(now - Duration::days(120))))
        .fetch_all(t.ctx.db())
        .await
        .unwrap();
    for r in scan {
        assert_eq!(last[&r.get::<String, _>(0)], Some(time::from_unix_ms(r.get::<i64, _>(1))));
    }

    // A collar clock running ahead: grazing now, not in the future.
    t.fix("col_c", now + Duration::seconds(3), IN_P3, Some(&p3)).await;
    assert_eq!(t.last_grazed(now).await[&p3], Some(now));
}

#[tokio::test]
async fn fixes_outside_every_paddock_count_for_a_paddock_drawn_over_them_later() {
    let t = setup().await;
    let now = ms(time::now());
    t.fix("col_a", now - Duration::hours(3), LANE, None).await;
    t.fix("col_a", now - Duration::hours(2), LANE, None).await;
    // Drawn over the lane afterwards.
    t.ok("POST", "/api/paddocks", json!({ "name": "Lane", "geometry": square(-93.615, 42.03) })).await;
    let lane = t.id_of("Lane").await;
    assert_eq!(t.last_grazed(ms(time::now())).await[&lane], Some(now - Duration::hours(2)));
    // A fix landing in the lane from now on is stored with it.
    let later = ms(time::now()) + Duration::milliseconds(5);
    t.fix("col_a", later, LANE, Some(&lane)).await;
    assert_eq!(t.last_grazed(later + Duration::seconds(1)).await[&lane], Some(later));
}

#[tokio::test]
async fn a_big_window_is_sampled_per_collar_and_bucket() {
    let t = setup().await;
    let (p1, p2) = (t.id_of("P1").await, t.id_of("P2").await);
    let now = ms(time::now());
    let from = now - Duration::hours(24);
    // 30 collars, a fix every 72 s for a day: 36,000 fixes. Half in P1, half in P2.
    sqlx::query(
        "WITH RECURSIVE c(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM c WHERE i < 29),
                        s(k) AS (SELECT 0 UNION ALL SELECT k + 1 FROM s WHERE k < 1199)
         INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m, sats, state, paddock_id)
         SELECT printf('col_%02d', c.i), ?, 'x', ? + s.k * 72000, CASE WHEN c.i < 15 THEN ?3 ELSE ?4 END, 42.0318, 3.0, 9, 'inside',
                CASE WHEN c.i < 15 THEN ?5 ELSE ?6 END
         FROM c, s",
    )
    .bind(&t.herd.id)
    .bind(time::unix_ms(&from))
    .bind(IN_P1[0])
    .bind(IN_P2[0])
    .bind(&p1)
    .bind(&p2)
    .execute(t.ctx.db())
    .await
    .unwrap();
    let paddocks = t.paddocks().await;

    let got = signals::herd_fixes(&t.ctx, &t.herd.id, from, now, &paddocks).await.unwrap();
    assert!(got.len() as i64 <= MAX_FIX_ROWS && got.len() > 15_000, "{} samples", got.len());
    assert!(got.windows(2).all(|w| w[0].t <= w[1].t), "in time order");
    let per: std::collections::BTreeMap<&str, (usize, i64, i64)> = got.iter().fold(Default::default(), |mut m, f| {
        let e = m.entry(f.collar_id.as_str()).or_insert((0, i64::MAX, i64::MIN));
        e.0 += 1;
        e.1 = e.1.min(f.t);
        e.2 = e.2.max(f.t);
        m
    });
    assert_eq!(per.len(), 30, "every collar");
    let day = 24 * 3_600_000;
    for (c, (n, lo, hi)) in &per {
        assert!(*n >= 600, "{c}: {n}");
        assert!(lo - time::unix_ms(&from) < day / 500 && time::unix_ms(&now) - hi < day / 300, "{c} spans the day: {lo}..{hi}");
    }
    let counts = signals::fix_counts(&got);
    assert_eq!(counts[&p1], counts[&p2], "an even sample: {counts:?}");

    // A window under the cap is every fix, as it always was.
    let hour = signals::herd_fixes(&t.ctx, &t.herd.id, now - Duration::hours(1), now, &paddocks).await.unwrap();
    let all: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fixes WHERE herd_id = ? AND t >= ? AND t < ?")
        .bind(&t.herd.id)
        .bind(time::unix_ms(&(now - Duration::hours(1))))
        .bind(time::unix_ms(&now))
        .fetch_one(t.ctx.db())
        .await
        .unwrap();
    assert_eq!(hour.len() as i64, all);

    // Where the herd is, from the same sample: half the fixes in either paddock is enough.
    let (current, source, _, n) = op_engine::context::locate(&t.ctx, &t.herd, &paddocks).await.unwrap();
    assert!(n as i64 <= MAX_FIX_ROWS && n > 15_000, "{n}");
    assert!(source == "collar" && (current == Some(p1.clone()) || current == Some(p2.clone())), "{current:?} {source}");
}

async fn plan(ctx: &Ctx, sql: &str) -> String {
    let rows = sqlx::query(&format!("EXPLAIN QUERY PLAN {sql}")).fetch_all(ctx.db()).await.unwrap();
    rows.iter().map(|r| r.get::<String, _>("detail")).collect::<Vec<_>>().join(" | ")
}

#[tokio::test]
async fn signals_read_fixes_by_index_seeks_only() {
    let t = setup().await;
    for sql in [NEXT_COLLAR_SQL, COLLAR_IN_WINDOW_SQL, BUCKETS_SQL] {
        let p = plan(&t.ctx, sql).await;
        assert!(p.contains("fixes_herd_collar_t") && !p.contains("SCAN fixes") && !p.contains("TEMP B-TREE"), "{sql}: {p}");
    }
    let p = plan(&t.ctx, COUNT_UP_TO_SQL).await;
    assert!(p.contains("fixes_herd_t") && !p.contains("SCAN fixes"), "{p}");
    let p = plan(&t.ctx, "SELECT MIN(t) FROM fixes WHERE herd_id = ?").await;
    assert!(p.contains("fixes_herd_t") && !p.contains("SCAN fixes"), "{p}");
    let p = plan(&t.ctx, "SELECT paddock_id, last_t FROM fix_paddock_last WHERE herd_id = ?").await;
    assert!(p.contains("PRIMARY KEY") && !p.contains("SCAN"), "{p}");
}
