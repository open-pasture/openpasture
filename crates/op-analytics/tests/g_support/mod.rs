//! Fixtures for the coverage and fleet tests (field-ready G): a farm with
//! two herds and collars, telemetry written straight into SQLite the way
//! op-ingest stores it, day files written the way a rollup writes them, and
//! HTTP calls through op-analytics' router as the local owner.

#![allow(dead_code)]

use std::sync::{Arc, Mutex, OnceLock};

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use datafusion::arrow::array::{ArrayRef, Float64Array, Int64Array, StringArray};
use datafusion::arrow::datatypes::{DataType, Field, Schema};
use datafusion::arrow::record_batch::RecordBatch;
use http_body_util::BodyExt;
use op_core::*;
use op_geo::{LonLat, Projection};
use serde_json::Value;
use tower::ServiceExt;

/// The farm's centre: the grid origin, so `at(e, n)` lands in cell
/// `(floor(e / 10), floor(n / 10))`.
pub const ORIGIN: LonLat = [-93.62, 42.03];
pub const SEC: i64 = 1000;
pub const MIN: i64 = 60_000;
pub const HOUR: i64 = 3_600_000;
pub const DAY: i64 = 86_400_000;

pub fn at(east: f64, north: f64) -> LonLat {
    Projection::new(ORIGIN).offset(east, north)
}

/// A point in the middle of cell `(cx, cy)`.
pub fn cell(cx: i64, cy: i64) -> LonLat {
    at(cx as f64 * 10.0 + 5.0, cy as f64 * 10.0 + 5.0)
}

pub fn midnight(t: DateTime<Utc>) -> DateTime<Utc> {
    t.date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc()
}

pub fn date(ms: i64) -> NaiveDate {
    time::from_unix_ms(ms).date_naive()
}

pub fn day_str(ms: i64) -> String {
    date(ms).format("%Y-%m-%d").to_string()
}

pub struct App {
    _dir: tempfile::TempDir,
    pub ctx: Ctx,
    router: axum::Router,
}

impl App {
    /// Farm; herd_1 "Cows" with collars col_1..col_n on animals tagged 101..;
    /// herd_2 "Heifers" with none.
    pub async fn new(collars: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let now = time::now();
        let s = ctx.store();
        s.insert_farm(&Farm { id: "farm_1".into(), name: "Test".into(), timezone: "America/Chicago".into(), center: ORIGIN, created_at: now }).await.unwrap();
        for (id, name) in [("herd_1", "Cows"), ("herd_2", "Heifers")] {
            s.insert_herd(&Herd {
                id: id.into(),
                name: name.into(),
                species: Species::Cattle,
                count: 10,
                paddock_id: None,
                autonomy: Autonomy::Propose,
                timer_minutes: 60,
                created_at: now - Duration::days(60),
            })
            .await
            .unwrap();
        }
        for i in 1..=collars {
            let c = format!("col_{i}");
            sqlx::query("INSERT INTO collars (id, name, herd_id, created_at) VALUES (?, ?, 'herd_1', ?)")
                .bind(&c)
                .bind(format!("C-{i:04}"))
                .bind(time::to_db(&(now - Duration::days(40))))
                .execute(ctx.db())
                .await
                .unwrap();
            s.insert_animal(&Animal {
                id: format!("ani_{i}"),
                tag: format!("{}", 100 + i),
                herd_id: "herd_1".into(),
                collar_id: Some(c),
                ..Default::default()
            })
            .await
            .unwrap();
        }
        let router = op_core::with_identity(op_analytics::router(), Identity::owner(Via::Local)).with_state(ctx.clone());
        Self { _dir: dir, ctx, router }
    }

    /// Fixes `(collar, t, point, accuracy)` in herd_1, in one transaction.
    pub async fn fixes(&self, rows: &[(&str, i64, LonLat, f64)]) {
        self.fixes_in("herd_1", rows).await
    }

    pub async fn fixes_in(&self, herd: &str, rows: &[(&str, i64, LonLat, f64)]) {
        let mut tx = self.ctx.db().begin().await.unwrap();
        for chunk in rows.chunks(500) {
            let sql = format!(
                "INSERT INTO fixes (collar_id, herd_id, animal_id, at, t, lon, lat, accuracy_m, sats) VALUES {}",
                vec!["(?, ?, ?, ?, ?, ?, ?, ?, 9)"; chunk.len()].join(", ")
            );
            let mut q = sqlx::query(&sql);
            for (c, t, p, acc) in chunk {
                q = q.bind(*c).bind(herd).bind(c.replace("col", "ani")).bind(time::to_db(&time::from_unix_ms(*t))).bind(*t).bind(p[0]).bind(p[1]).bind(*acc);
            }
            q.execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
    }

    /// One fix every `every` ms from `from` (inclusive) to `to` (exclusive) at `p`.
    pub async fn steady(&self, collar: &str, from: i64, to: i64, every: i64, p: LonLat, acc: f64) {
        let rows: Vec<(&str, i64, LonLat, f64)> = (0..).map(|i| from + i * every).take_while(|t| *t < to).map(|t| (collar, t, p, acc)).collect();
        self.fixes(&rows).await;
    }

    /// Health rows `(collar, t, battery)`, one per report.
    pub async fn health(&self, rows: &[(&str, i64, Option<f64>)]) {
        let mut tx = self.ctx.db().begin().await.unwrap();
        for chunk in rows.chunks(500) {
            let sql = format!(
                "INSERT INTO health (collar_id, herd_id, at, t, battery, fixes, cues) VALUES {}",
                vec!["(?, 'herd_1', ?, ?, ?, 1, 0)"; chunk.len()].join(", ")
            );
            let mut q = sqlx::query(&sql);
            for (c, t, b) in chunk {
                q = q.bind(*c).bind(time::to_db(&time::from_unix_ms(*t))).bind(*t).bind(*b);
            }
            q.execute(&mut *tx).await.unwrap();
        }
        tx.commit().await.unwrap();
    }

    pub async fn count(&self, sql: &str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(self.ctx.db()).await.unwrap()
    }

    pub async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        send(self.router.clone(), method, path, body).await
    }

    /// The same call made by someone else.
    pub async fn call_as(&self, who: Identity, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        send(op_core::with_identity(op_analytics::router(), who).with_state(self.ctx.clone()), method, path, body).await
    }

    pub async fn get(&self, path: &str) -> Value {
        let (status, v) = self.call("GET", path, None).await;
        assert_eq!(status, StatusCode::OK, "{path}: {v}");
        v
    }

    /// Write `rows` of `table` for the day of `day_ms` to its day file the
    /// way the rollup does, leaving them in SQLite too (a rollup that has
    /// written its file but not yet deleted the rows).
    pub async fn copy_day_to_parquet(&self, table: &str, day_ms: i64) {
        let d = date(day_ms);
        let (from, to) = (day_ms - day_ms.rem_euclid(DAY), day_ms - day_ms.rem_euclid(DAY) + DAY);
        let path = op_analytics::telemetry::day_path(self.ctx.data_dir(), table, d);
        let batch = match table {
            "fixes" => {
                let schema = op_analytics::schema::table_schema(self.ctx.db(), "fixes").await.unwrap();
                let mut b = op_analytics::schema::BatchBuilder::new(&schema);
                let rows = sqlx::query(&format!("SELECT {} FROM fixes WHERE t >= ? AND t < ? ORDER BY collar_id, t", schema.select_list()))
                    .bind(from)
                    .bind(to)
                    .fetch_all(self.ctx.db())
                    .await
                    .unwrap();
                for r in &rows {
                    b.push(r);
                }
                b.finish().unwrap()
            }
            "health" => {
                let rows: Vec<(i64, String, String, i64, Option<f64>)> =
                    sqlx::query_as("SELECT id, collar_id, at, t, battery FROM health WHERE t >= ? AND t < ? ORDER BY collar_id, t")
                        .bind(from)
                        .bind(to)
                        .fetch_all(self.ctx.db())
                        .await
                        .unwrap();
                health_batch(&rows)
            }
            _ => unreachable!(),
        };
        let schema = batch.schema();
        op_analytics::telemetry::write_parquet(&path, &schema, &[batch]).unwrap();
    }

    /// Move a day of `health` to its day file and delete it from SQLite, the
    /// way a rollup of `health` would.
    pub async fn roll_health_day(&self, day_ms: i64) {
        self.copy_day_to_parquet("health", day_ms).await;
        let from = day_ms - day_ms.rem_euclid(DAY);
        sqlx::query("DELETE FROM health WHERE t >= ? AND t < ?").bind(from).bind(from + DAY).execute(self.ctx.db()).await.unwrap();
    }
}

async fn send(router: axum::Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = router.oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn health_batch(rows: &[(i64, String, String, i64, Option<f64>)]) -> RecordBatch {
    let schema = Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("collar_id", DataType::Utf8, true),
        Field::new("herd_id", DataType::Utf8, true),
        Field::new("at", DataType::Utf8, true),
        Field::new("t", DataType::Int64, true),
        Field::new("battery", DataType::Float64, true),
    ]));
    let cols: Vec<ArrayRef> = vec![
        Arc::new(Int64Array::from(rows.iter().map(|r| r.0).collect::<Vec<_>>())),
        Arc::new(StringArray::from(rows.iter().map(|r| r.1.clone()).collect::<Vec<_>>())),
        Arc::new(StringArray::from(vec!["herd_1"; rows.len()])),
        Arc::new(StringArray::from(rows.iter().map(|r| r.2.clone()).collect::<Vec<_>>())),
        Arc::new(Int64Array::from(rows.iter().map(|r| r.3).collect::<Vec<_>>())),
        Arc::new(Float64Array::from(rows.iter().map(|r| r.4).collect::<Vec<_>>())),
    ];
    RecordBatch::try_new(schema, cols).unwrap()
}

// ---------------------------------------------------------------- query counter

/// SQL statements sqlx ran, as its query log reports them (target
/// `sqlx::query`), for every connection in this test process.
static STATEMENTS: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

struct Counter;

struct Fields(String);

impl tracing::field::Visit for Fields {
    fn record_str(&mut self, _: &tracing::field::Field, value: &str) {
        self.0.push(' ');
        self.0.push_str(value);
    }
    fn record_debug(&mut self, f: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        if matches!(f.name(), "summary" | "db.statement" | "message") {
            self.0.push(' ');
            self.0.push_str(&format!("{value:?}"));
        }
    }
}

impl tracing::Subscriber for Counter {
    fn enabled(&self, m: &tracing::Metadata<'_>) -> bool {
        m.target() == "sqlx::query"
    }
    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, e: &tracing::Event<'_>) {
        let mut f = Fields(String::new());
        e.record(&mut f);
        STATEMENTS.get_or_init(Default::default).lock().unwrap().push(f.0);
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

/// Start counting statements (once per test process).
pub fn count_queries() {
    static ONCE: OnceLock<()> = OnceLock::new();
    ONCE.get_or_init(|| {
        tracing::subscriber::set_global_default(Counter).expect("one subscriber per test process");
    });
}

/// Statements logged since the last call.
pub fn take_statements() -> Vec<String> {
    std::mem::take(&mut *STATEMENTS.get_or_init(Default::default).lock().unwrap())
}

/// Whether a statement names `table` as a word.
pub fn names_table(sql: &str, table: &str) -> bool {
    sql.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_')).any(|w| w.eq_ignore_ascii_case(table))
}

/// Tests that count queries, and every other test in the same binary, run
/// one at a time so no other test's statements land in the count.
pub fn serial() -> &'static tokio::sync::Mutex<()> {
    static LOCK: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| tokio::sync::Mutex::new(()))
}
