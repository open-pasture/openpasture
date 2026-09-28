//! HTTP handlers for tracks, health, behaviour, heatmap, pasture and SQL.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use futures::{StreamExt, TryStreamExt};
use op_core::domain::DbEnum;
use op_core::{ApiError, ApiJson, ApiResult, Ctx};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

use crate::metrics::{Cadence, Dwell, DwellAgg, DwellKey, Grid, Hist, MAX_DWELL_GAP_MS, PaddockIndex, Walk, au_per_head, round, slope};
use crate::range::{self, TimeRange, bucket_ms};
use crate::schema::table_schema;
use crate::sql;
use crate::telemetry::{Order, Scope, Source, each_cue, each_fix, for_each, scan_cold, scan_in};

const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Default, Deserialize)]
pub struct Params {
    pub collar_id: Option<String>,
    pub herd_id: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub bucket: Option<String>,
    pub max_points: Option<String>,
    pub cell_m: Option<String>,
    pub normalize: Option<String>,
}

impl Params {
    fn range(&self, default_span: Duration) -> Result<TimeRange, ApiError> {
        TimeRange::parse(self.from.as_deref(), self.to.as_deref(), default_span, op_core::time::now())
    }
    fn collar(&self) -> Option<&str> {
        self.collar_id.as_deref().filter(|s| !s.is_empty())
    }
    fn herd(&self) -> Option<&str> {
        self.herd_id.as_deref().filter(|s| !s.is_empty())
    }
}

fn parse_num<T: std::str::FromStr>(v: Option<&str>, name: &str) -> Result<Option<T>, ApiError> {
    match v.filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => s.parse().map(Some).map_err(|_| ApiError::bad_request(format!("`{name}` must be a number."))),
    }
}

fn unix_s(ms: i64) -> f64 {
    ms as f64 / 1000.0
}

fn rfc3339(ms: i64) -> String {
    op_core::time::to_db(&op_core::time::from_unix_ms(ms))
}

/// Collars in scope, with what health needs.
#[derive(Debug, Clone)]
pub struct CollarInfo {
    pub id: String,
    pub name: String,
    pub herd_id: String,
    pub animal_id: Option<String>,
    pub battery: Option<f64>,
    pub boundary_version: Option<i64>,
    pub created_ms: Option<i64>,
    /// Configured seconds between fixes, if the collar row has one.
    pub cadence_s: Option<f64>,
}

pub async fn collars(ctx: &Ctx, herd_id: Option<&str>, collar_id: Option<&str>) -> anyhow::Result<Vec<CollarInfo>> {
    let has_cadence = table_schema(ctx.db(), "collars").await?.has("fix_interval_s");
    let cadence = if has_cadence { "fix_interval_s" } else { "NULL" };
    let mut sql = format!("SELECT id, name, herd_id, animal_id, battery, boundary_version, created_at, {cadence} AS cadence FROM collars WHERE 1=1");
    if collar_id.is_some() {
        sql.push_str(" AND id = ?");
    }
    if herd_id.is_some() {
        sql.push_str(" AND herd_id = ?");
    }
    sql.push_str(" ORDER BY id");
    let mut q = sqlx::query(&sql);
    if let Some(c) = collar_id {
        q = q.bind(c);
    }
    if let Some(h) = herd_id {
        q = q.bind(h);
    }
    let mut out = Vec::new();
    for r in q.fetch_all(ctx.db()).await? {
        let created: Option<String> = r.try_get("created_at")?;
        out.push(CollarInfo {
            id: r.try_get("id")?,
            name: r.try_get("name")?,
            herd_id: r.try_get("herd_id")?,
            animal_id: r.try_get("animal_id")?,
            battery: r.try_get_unchecked::<Option<f64>, _>("battery").ok().flatten(),
            boundary_version: r.try_get_unchecked::<Option<i64>, _>("boundary_version").ok().flatten(),
            created_ms: created.and_then(|s| op_core::time::from_db(&s).ok()).map(|t| t.timestamp_millis()),
            cadence_s: r.try_get_unchecked::<Option<f64>, _>("cadence").ok().flatten().filter(|c| *c > 0.0),
        });
    }
    Ok(out)
}

// ---------------------------------------------------------------- tracks

#[derive(Debug, Serialize, PartialEq)]
pub struct Track {
    pub collar_id: String,
    pub points: Vec<[f64; 3]>,
}

/// One collar's track as it is built: the first fix in each time bucket and
/// the latest fix, from however many sources in whatever order.
#[derive(Default)]
struct TrackAcc {
    first: BTreeMap<i64, (i64, f64, f64)>,
    last: Option<(i64, f64, f64)>,
    /// The bucket of the previous push and its first time, so sorted input
    /// touches the map once per bucket.
    run: Option<(i64, i64)>,
}

impl TrackAcc {
    fn push(&mut self, bucket: i64, t: i64, lon: f64, lat: f64) {
        if self.last.is_none_or(|(lt, _, _)| t > lt) {
            self.last = Some((t, lon, lat));
        }
        if let Some((b, bt)) = self.run {
            if b == bucket && t >= bt {
                return;
            }
        }
        let e = self.first.entry(bucket).or_insert((t, lon, lat));
        if t < e.0 {
            *e = (t, lon, lat);
        }
        self.run = Some((bucket, e.0));
    }

    fn merge(&mut self, other: TrackAcc) {
        for (b, (t, lon, lat)) in other.first {
            self.run = None;
            self.push(b, t, lon, lat);
        }
        if let Some((t, lon, lat)) = other.last {
            if self.last.is_none_or(|(lt, _, _)| t > lt) {
                self.last = Some((t, lon, lat));
            }
        }
    }

    fn into_track(self, collar_id: String) -> Track {
        let pt = |(t, lon, lat): (i64, f64, f64)| [round(lon, 7), round(lat, 7), unix_s(t)];
        let mut points: Vec<[f64; 3]> = self.first.into_values().map(pt).collect();
        if let Some(last) = self.last {
            if points.last().is_none_or(|p| p[2] != unix_s(last.0)) {
                points.push(pt(last));
            }
        }
        Track { collar_id, points }
    }
}

pub async fn tracks(State(ctx): State<Ctx>, Query(p): Query<Params>) -> ApiResult<Json<Vec<Track>>> {
    let range = p.range(Duration::hours(24))?;
    let max_points: usize = parse_num(p.max_points.as_deref(), "max_points")?.unwrap_or(2000).clamp(2, 50_000);
    Ok(Json(track_points(&ctx, range, p.collar(), p.herd(), max_points).await?))
}

/// Tracks downsampled by time: the first fix in each of `max_points` equal
/// time buckets, plus each collar's latest fix. Hot days seek each bucket's
/// first fix by index; Parquet days read only the five columns a track needs.
pub async fn track_points(ctx: &Ctx, range: TimeRange, collar: Option<&str>, herd: Option<&str>, max_points: usize) -> anyhow::Result<Vec<Track>> {
    let width = (range.span_ms() / max_points as i64).max(1);
    let mut accs: BTreeMap<String, TrackAcc> = BTreeMap::new();
    let (first, last) = (range::date_of(range.from_ms()), range::date_of(range.to_ms() - 1));
    for (date, path) in crate::telemetry::list_days(ctx.data_dir(), "fixes") {
        if date < first || date > last {
            continue;
        }
        let (c, h) = (collar.map(str::to_owned), herd.map(str::to_owned));
        let day = tokio::task::spawn_blocking(move || cold_track_day(&path, range, width, c.as_deref(), h.as_deref())).await??;
        for (id, acc) in day {
            accs.entry(id).or_default().merge(acc);
        }
    }
    hot_tracks(ctx, range, collar, herd, width, &mut accs).await?;
    Ok(accs.into_iter().filter(|(_, a)| a.last.is_some()).map(|(id, a)| a.into_track(id)).collect())
}

/// One Parquet day: row groups outside the range (or not holding the collar)
/// are skipped by their statistics, and only the track columns are decoded.
fn cold_track_day(path: &std::path::Path, range: TimeRange, width: i64, collar: Option<&str>, herd: Option<&str>) -> anyhow::Result<HashMap<String, TrackAcc>> {
    use datafusion::arrow::array::Array;
    use parquet::arrow::ProjectionMask;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
    use parquet::file::statistics::Statistics;

    let (from, to) = (range.from_ms(), range.to_ms());
    let builder = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(path)?)?;
    let descr = builder.parquet_schema();
    let col = |name: &str| descr.columns().iter().position(|c| c.name() == name);
    let (t_idx, collar_idx) = (col("t"), col("collar_id"));
    let groups: Vec<usize> = builder
        .metadata()
        .row_groups()
        .iter()
        .enumerate()
        .filter(|(_, rg)| {
            let t_ok = match t_idx.and_then(|i| rg.column(i).statistics()) {
                Some(Statistics::Int64(s)) => s.min_opt().is_none_or(|lo| *lo < to) && s.max_opt().is_none_or(|hi| *hi >= from),
                _ => true,
            };
            let collar_ok = match (collar, collar_idx.and_then(|i| rg.column(i).statistics())) {
                (Some(c), Some(Statistics::ByteArray(s))) => {
                    s.min_opt().is_none_or(|lo| lo.data() <= c.as_bytes()) && s.max_opt().is_none_or(|hi| hi.data() >= c.as_bytes())
                }
                _ => true,
            };
            t_ok && collar_ok
        })
        .map(|(i, _)| i)
        .collect();
    let mask = ProjectionMask::columns(descr, ["collar_id", "herd_id", "t", "lon", "lat"]);
    let reader = builder.with_projection(mask).with_row_groups(groups).with_batch_size(crate::telemetry::BATCH_ROWS).build()?;
    let mut index: HashMap<String, usize> = HashMap::new();
    let mut accs: Vec<(String, TrackAcc)> = Vec::new();
    for b in reader {
        let b = b?;
        let c = crate::schema::Cols::new(&b);
        let (Some(ids), Some(ts), Some(lons), Some(lats)) = (c.str("collar_id"), c.i64("t"), c.f64("lon"), c.f64("lat")) else { continue };
        let herds = c.str("herd_id");
        let mut cur: Option<usize> = None;
        for i in 0..b.num_rows() {
            if ids.is_null(i) || ts.is_null(i) || lons.is_null(i) || lats.is_null(i) {
                continue;
            }
            let t = ts.value(i);
            if t < from || t >= to {
                continue;
            }
            let id = ids.value(i);
            if collar.is_some_and(|c| c != id) || herd.is_some_and(|h| herds.is_none_or(|a| a.is_null(i) || a.value(i) != h)) {
                continue;
            }
            let slot = match cur.filter(|s| accs[*s].0 == id) {
                Some(s) => s,
                None => {
                    let s = *index.entry(id.to_owned()).or_insert_with(|| {
                        accs.push((id.to_owned(), TrackAcc::default()));
                        accs.len() - 1
                    });
                    cur = Some(s);
                    s
                }
            };
            accs[slot].1.push((t - from) / width, t, lons.value(i), lats.value(i));
        }
    }
    Ok(accs.into_iter().collect())
}

/// The next collar after `?2` with fixes in herd `?1` (by `fixes_herd_collar_t`).
pub const HERD_NEXT_COLLAR_SQL: &str = "SELECT collar_id FROM fixes WHERE herd_id = ? AND collar_id > ? ORDER BY collar_id LIMIT 1";

/// A collar's first fix, last fix and first fix per bucket in `[?3, ?4)`
/// (`?1` collar, `?2` herd or NULL, buckets `?5..=?6` of `?7` ms from `?3`).
/// Each is an index seek: `fixes_herd_collar_t` for a herd, else `fixes_collar_t`.
pub fn hot_track_sql(herd: bool) -> [String; 3] {
    let herd_filter = if herd { "herd_id = ?2 AND" } else { "?2 IS NULL AND" };
    let edge =
        |dir: &str| format!("SELECT t, lon, lat FROM fixes WHERE {herd_filter} collar_id = ?1 AND t >= ?3 AND t < ?4 ORDER BY t {dir}, id {dir} LIMIT 1");
    [
        edge("ASC"),
        edge("DESC"),
        format!(
            "WITH RECURSIVE b(k) AS (SELECT ?5 UNION ALL SELECT k + 1 FROM b WHERE k < ?6)
             SELECT f.t, f.lon, f.lat FROM b JOIN fixes f ON f.id = (
                 SELECT id FROM fixes WHERE {herd_filter} collar_id = ?1 AND t >= max(?3, ?3 + k * ?7) AND t < min(?4, ?3 + (k + 1) * ?7)
                 ORDER BY t, id LIMIT 1)"
        ),
    ]
}

/// Hot fixes: each collar's first fix per bucket by one index seek per
/// bucket (never a scan of the day), and its latest.
async fn hot_tracks(
    ctx: &Ctx,
    range: TimeRange,
    collar: Option<&str>,
    herd: Option<&str>,
    width: i64,
    accs: &mut BTreeMap<String, TrackAcc>,
) -> anyhow::Result<()> {
    let (from, to) = (range.from_ms(), range.to_ms());
    let [first_sql, last_sql, buckets_sql] = hot_track_sql(herd.is_some());
    let mut after = String::new();
    loop {
        let next = match (collar, herd) {
            (Some(c), _) => (after.is_empty()).then(|| c.to_owned()),
            (None, Some(h)) => sqlx::query_scalar(HERD_NEXT_COLLAR_SQL).bind(h).bind(&after).fetch_optional(ctx.db()).await?,
            (None, None) => crate::rollup::next_collar(ctx, "fixes", &after).await?,
        };
        let Some(id) = next else { break };
        let edge_row = |sql: String| {
            let id = id.clone();
            async move { sqlx::query_as::<_, (i64, f64, f64)>(&sql).bind(id).bind(herd).bind(from).bind(to).fetch_optional(ctx.db()).await }
        };
        if let (Some(first), Some(last)) = (edge_row(first_sql.clone()).await?, edge_row(last_sql.clone()).await?) {
            let acc = accs.entry(id.clone()).or_default();
            let rows: Vec<(i64, f64, f64)> = sqlx::query_as(&buckets_sql)
                .bind(&id)
                .bind(herd)
                .bind(from)
                .bind(to)
                .bind((first.0 - from) / width)
                .bind((last.0 - from) / width)
                .bind(width)
                .fetch_all(ctx.db())
                .await?;
            for (t, lon, lat) in rows.into_iter().chain([first, last]) {
                acc.push((t - from) / width, t, lon, lat);
            }
        }
        after = id;
    }
    Ok(())
}

// ---------------------------------------------------------------- health

#[derive(Default)]
struct HealthAcc {
    fixes: i64,
    /// Accuracy values and how often each came (@L: every fix counts, and
    /// memory stays bounded however long the range).
    acc: Hist,
    sats: (f64, i64),
    cn0: (f64, i64),
    ttf: (f64, i64),
    cues: i64,
}

// @L
/// The hot fixes of health's collars from `?` to `?`, summed in SQLite per
/// collar, bucket (`(t - first) / bucket`, bound first) and accuracy: a day
/// of 250 collars comes back as some thousands of sums, not 4.3 M rows.
/// (Plain `?` only: sqlx numbers them apart from `?NNN` ones.)
pub fn health_hot_sql(collars: usize, rolled: usize) -> String {
    format!(
        "SELECT collar_id, (t - ?) / ? AS b, ROUND(accuracy_m, 2) AS a, COUNT(*) AS n,
                TOTAL(sats), COUNT(sats), TOTAL(cn0), COUNT(cn0), TOTAL(ttf_s), COUNT(ttf_s)
         FROM fixes WHERE t >= ? AND t < ? AND +collar_id IN ({}){}
         GROUP BY collar_id, b, a",
        vec!["?"; collars].join(","),
        crate::telemetry::not_rolled_sql(rolled)
    )
}

/// Seconds between one collar's hot fixes (whole, at least 1), counted:
/// [`Cadence`]'s gaps over [`CADENCE_FIXES`] of its fixes, the first
/// `CADENCE_FIXES / CADENCE_CHUNKS` of each of [`CADENCE_CHUNKS`] equal
/// stretches of the range, so a change of cadence partway through counts
/// (`?1` the collar, `?2` fixes a stretch, then each stretch's from and to),
/// read from the `(collar_id, t)` index alone. The typical gap is its
/// cadence; a day of them would read 17,280 entries a collar. Also gives
/// how many fixes were read, to weigh the gaps against the cold days'.
pub fn health_gaps_sql() -> String {
    let chunk = |i: usize| {
        format!(
            "SELECT t - pt AS d FROM (SELECT t, LAG(t) OVER (ORDER BY t) AS pt FROM (SELECT t FROM fixes WHERE collar_id = ?1 AND t >= ?{} AND t < ?{} ORDER BY t LIMIT ?2))",
            3 + 2 * i,
            4 + 2 * i
        )
    };
    let all: Vec<String> = (0..CADENCE_CHUNKS).map(chunk).collect();
    format!("SELECT MAX(1, CAST(ROUND(d / 1000.0) AS INTEGER)) AS g, COUNT(*) FROM ({}) WHERE d > 0 GROUP BY g", all.join(" UNION ALL "))
}
pub const CADENCE_FIXES: i64 = 2_000;
pub const CADENCE_CHUNKS: usize = 8;

fn mean((sum, n): (f64, i64)) -> Option<f64> {
    (n > 0).then(|| sum / n as f64)
}

#[derive(Serialize)]
pub struct HealthPoint {
    pub t: String,
    pub fixes: i64,
    pub fix_rate: Option<f64>,
    pub acc_p50: Option<f64>,
    pub acc_p95: Option<f64>,
    pub sats: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cn0: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ttf_s: Option<f64>,
    pub battery: Option<f64>,
    pub cues: i64,
}

#[derive(Serialize)]
pub struct CollarHealth {
    pub collar_id: String,
    pub name: String,
    pub herd_id: String,
    pub animal_id: Option<String>,
    pub bucket_s: i64,
    /// Seconds between fixes: configured, else the median seen in the range.
    pub cadence_s: Option<f64>,
    pub summary: HealthPoint,
    pub ack: Option<Value>,
    pub points: Vec<HealthPoint>,
}

/// Longest a battery reading is carried forward into later buckets.
const BATTERY_CARRY_MS: i64 = 6 * 3_600_000;

pub async fn health(State(ctx): State<Ctx>, Query(p): Query<Params>) -> ApiResult<Json<Vec<CollarHealth>>> {
    let mut range = p.range(Duration::hours(24))?;
    let list = collars(&ctx, p.herd(), p.collar()).await?;
    if let (Some(c), true) = (p.collar(), list.is_empty()) {
        return Err(ApiError::not_found(format!("No collar {c}.")));
    }
    // With no bucket asked for, bucket the span the collars can have data
    // for: a herd linked an hour ago gets minutes, not a day of empty tens.
    if p.bucket.as_deref().is_none_or(|b| b.trim().is_empty()) {
        range = data_span(range, &list, op_core::time::now());
    }
    let bucket = bucket_ms(p.bucket.as_deref(), &range, 150)?;
    Ok(Json(collar_health(&ctx, range, bucket, list).await?))
}

/// The range clipped to start when the first of these collars was linked and
/// to end no later than now. At least ten minutes, so a new herd still gets
/// a few buckets.
fn data_span(range: TimeRange, list: &[CollarInfo], now: DateTime<Utc>) -> TimeRange {
    let to = range.to.min(now).max(range.from);
    let first = list.iter().map(|c| c.created_ms).collect::<Option<Vec<_>>>().and_then(|v| v.into_iter().min());
    let from = match first.map(op_core::time::from_unix_ms) {
        Some(f) if f > range.from => f.min(to - Duration::minutes(10)).max(range.from),
        _ => range.from,
    };
    TimeRange::new(from, to)
}

pub async fn collar_health(ctx: &Ctx, range: TimeRange, bucket: i64, list: Vec<CollarInfo>) -> anyhow::Result<Vec<CollarHealth>> {
    let ids: Vec<String> = list.iter().map(|c| c.id.clone()).collect();
    let (from, to) = (range.from_ms(), range.to_ms());
    let first = from.div_euclid(bucket) * bucket;
    let n_buckets = ((to - first + bucket - 1) / bucket).max(1) as usize;
    let idx = |t: i64| ((t - first) / bucket) as usize;

    let mut per: HashMap<String, (Vec<HealthAcc>, HealthAcc, Cadence)> =
        ids.iter().map(|id| (id.clone(), ((0..n_buckets).map(|_| HealthAcc::default()).collect(), HealthAcc::default(), Cadence::default()))).collect();
    let scope = Scope { collar_ids: Some(ids.clone()), herd_id: None };
    // Parquet days row by row; per collar in time order is all it needs.
    let (cold, read) = scan_cold(ctx, "fixes", range, scope.clone(), Order::Time);
    for_each(cold, |b| {
        each_fix(b, |f| {
            let Some((buckets, total, cadence)) = per.get_mut(f.collar_id) else { return };
            cadence.push(f.t);
            for a in [&mut buckets[idx(f.t).min(n_buckets - 1)], total] {
                a.fixes += 1;
                if let Some(v) = f.accuracy_m {
                    a.acc.add(v, 1);
                }
                if let Some(v) = f.sats {
                    a.sats.0 += v as f64;
                    a.sats.1 += 1;
                }
                if let Some(v) = f.cn0 {
                    a.cn0.0 += v;
                    a.cn0.1 += 1;
                }
                if let Some(v) = f.ttf_s {
                    a.ttf.0 += v;
                    a.ttf.1 += 1;
                }
            }
        })
    })
    .await?;
    // @L: the hot fixes summed in SQLite, leaving out the rows of the files just read.
    let rolled = read.await.unwrap_or_default();
    if !ids.is_empty() {
        let rolled: Vec<_> = rolled.into_iter().filter(|r| r.from < to && r.to > from).collect();
        let sql = health_hot_sql(ids.len(), rolled.len());
        let mut q = sqlx::query(&sql).bind(first).bind(bucket).bind(from).bind(to);
        for id in &ids {
            q = q.bind(id);
        }
        for r in &rolled {
            q = q.bind(r.from).bind(r.to).bind(r.max_id);
        }
        let mut hot_n: HashMap<String, i64> = HashMap::new();
        for row in q.fetch_all(ctx.db()).await? {
            let collar: String = row.try_get(0)?;
            let Some((buckets, total, _)) = per.get_mut(&collar) else { continue };
            let b = (row.try_get::<i64, _>(1)?.max(0) as usize).min(n_buckets - 1);
            let n: i64 = row.try_get(3)?;
            *hot_n.entry(collar.clone()).or_default() += n;
            let acc: Option<f64> = row.try_get(2)?;
            let sums = |i: usize| -> anyhow::Result<(f64, i64)> { Ok((row.try_get::<Option<f64>, _>(i)?.unwrap_or(0.0), row.try_get::<i64, _>(i + 1)?)) };
            let (sats, cn0, ttf) = (sums(4)?, sums(6)?, sums(8)?);
            for a in [&mut buckets[b], total] {
                a.fixes += n;
                if let Some(v) = acc {
                    a.acc.add(v, n as u64);
                }
                for (dst, src) in [(&mut a.sats, sats), (&mut a.cn0, cn0), (&mut a.ttf, ttf)] {
                    dst.0 += src.0;
                    dst.1 += src.1;
                }
            }
        }
        // Cadence only matters when the collar's isn't configured. The hot
        // gaps are a sample spread over the hot part, each counted for as
        // many of its hot fixes as it stands for, beside every cold gap.
        let hot_from = rolled.iter().map(|r| r.to).max().unwrap_or(from).clamp(from, to);
        let want: Vec<String> =
            list.iter().filter(|c| c.cadence_s.is_none()).map(|c| c.id.clone()).filter(|id| hot_n.get(id).is_some_and(|n| *n > 1)).collect();
        let sql = health_gaps_sql();
        let chunks = CADENCE_CHUNKS as i64;
        let step = ((to - hot_from).max(1) + chunks - 1) / chunks;
        let reads = want.into_iter().map(|collar| {
            let (ctx, sql) = (ctx.clone(), sql.clone());
            async move {
                let mut q = sqlx::query_as::<_, (i64, i64)>(&sql).bind(collar.clone()).bind(CADENCE_FIXES / chunks);
                for i in 0..chunks {
                    q = q.bind(hot_from + i * step).bind((hot_from + (i + 1) * step).min(to));
                }
                let rows = q.fetch_all(ctx.db()).await?;
                anyhow::Ok((collar, rows))
            }
        });
        let gaps: Vec<(String, Vec<(i64, i64)>)> = futures::stream::iter(reads).buffer_unordered(PARALLEL_READS).try_collect().await?;
        for (collar, rows) in gaps {
            let sampled: i64 = rows.iter().map(|r| r.1).sum();
            let (Some((_, _, cadence)), Some(n)) = (per.get_mut(&collar), hot_n.get(&collar)) else { continue };
            let weight = (*n - 1).max(1) as f64 / sampled.max(1) as f64;
            for (g, k) in rows {
                cadence.add_gaps(g, (k as f64 * weight).round().max(1.0) as u64);
            }
        }
    }
    for_each(scan_in(ctx, "cues", range, scope, Source::All, Order::Time), |b| {
        each_cue(b, |c| {
            if let Some((buckets, total, _)) = per.get_mut(c.collar_id) {
                buckets[idx(c.t).min(n_buckets - 1)].cues += 1;
                total.cues += 1;
            }
        })
    })
    .await?;

    let battery = battery_series(ctx, &ids, first, bucket, n_buckets, from, to).await?;
    let acks = latest_acks(ctx, &ids).await?;
    let now = op_core::time::now().timestamp_millis();

    let mut out = Vec::with_capacity(list.len());
    for c in list {
        let Some((buckets, mut total, cadence)) = per.remove(&c.id) else { continue };
        let cadence_s = c.cadence_s.or_else(|| cadence.median_s());
        let series = battery.get(&c.id).cloned().unwrap_or_else(|| Battery::new(n_buckets));
        let point = |a: &mut HealthAcc, start: i64, end: i64, battery: Option<f64>| -> HealthPoint {
            let lo = start.max(from).max(c.created_ms.unwrap_or(i64::MIN));
            let hi = end.min(to).min(now);
            let fix_rate = match cadence_s {
                Some(cs) if hi > lo => Some(round((a.fixes as f64 / ((hi - lo) as f64 / 1000.0 / cs)).min(1.0), 3)),
                _ => None,
            };
            HealthPoint {
                t: rfc3339(start),
                fixes: a.fixes,
                fix_rate,
                acc_p50: a.acc.percentile(0.5).map(|v| round(v, 2)),
                acc_p95: a.acc.percentile(0.95).map(|v| round(v, 2)),
                sats: mean(a.sats).map(|v| round(v, 1)),
                cn0: mean(a.cn0).map(|v| round(v, 1)),
                ttf_s: mean(a.ttf).map(|v| round(v, 1)),
                battery,
                cues: a.cues,
            }
        };
        let points: Vec<HealthPoint> = buckets
            .into_iter()
            .enumerate()
            .map(|(i, mut a)| {
                let start = first + i as i64 * bucket;
                point(&mut a, start, start + bucket, series.at(i, start))
            })
            .collect();
        let mut summary = point(&mut total, from, to, series.summary(from).or(c.battery));
        summary.t = rfc3339(from);
        out.push(CollarHealth {
            ack: acks.get(&c.id).cloned(),
            collar_id: c.id,
            name: c.name,
            herd_id: c.herd_id,
            animal_id: c.animal_id,
            bucket_s: bucket / 1000,
            cadence_s,
            summary,
            points,
        });
    }
    Ok(out)
}

/// Battery (0-1) of one collar over the health buckets: per bucket the sum,
/// count and last reading, plus the last reading before the range.
#[derive(Debug, Clone, Default)]
struct Battery {
    buckets: Vec<Option<BatteryBucket>>,
    before: Option<(i64, f64)>,
}

#[derive(Debug, Clone, Copy)]
struct BatteryBucket {
    sum: f64,
    n: i64,
    last: (i64, f64),
}

impl Battery {
    fn new(n_buckets: usize) -> Self {
        Self { buckets: vec![None; n_buckets], before: None }
    }

    fn add(&mut self, i: usize, sum: f64, n: i64, last: (i64, f64)) {
        let Some(slot) = self.buckets.get_mut(i) else { return };
        match slot {
            Some(b) => {
                b.sum += sum;
                b.n += n;
                if last.0 >= b.last.0 {
                    b.last = last;
                }
            }
            None => *slot = Some(BatteryBucket { sum, n, last }),
        }
    }

    /// The last reading before bucket `i`.
    fn carried(&self, i: usize) -> Option<(i64, f64)> {
        self.buckets[..i].iter().rev().find_map(|b| b.map(|b| b.last)).or(self.before)
    }

    /// Mean of bucket `i`'s readings, else the last earlier reading if it is
    /// recent enough.
    fn at(&self, i: usize, start: i64) -> Option<f64> {
        match self.buckets.get(i).copied().flatten() {
            Some(b) => Some(round(b.sum / b.n as f64, 3)),
            None => self.carried(i.min(self.buckets.len())).filter(|(t, _)| start - t <= BATTERY_CARRY_MS).map(|(_, v)| round(v, 3)),
        }
    }

    /// Mean over the whole range, else the reading carried into it.
    fn summary(&self, from: i64) -> Option<f64> {
        let (sum, n) = self.buckets.iter().flatten().fold((0.0, 0), |(s, n), b| (s + b.sum, n + b.n));
        if n > 0 {
            return Some(round(sum / n as f64, 3));
        }
        self.before.filter(|(t, _)| from - t <= BATTERY_CARRY_MS).map(|(_, v)| round(v, 3))
    }
}

/// Battery per collar over `[from, to)` in buckets of `bucket` ms starting at
/// `first`: SQLite groups its rows itself, Parquet days are read three
/// columns at a time, and the reading before `from` (up to
/// [`BATTERY_CARRY_MS`] earlier) is one index seek per collar.
async fn battery_series(ctx: &Ctx, ids: &[String], first: i64, bucket: i64, n_buckets: usize, from: i64, to: i64) -> anyhow::Result<HashMap<String, Battery>> {
    let mut out: HashMap<String, Battery> = ids.iter().map(|id| (id.clone(), Battery::new(n_buckets))).collect();
    if ids.is_empty() {
        return Ok(out);
    }
    let idx = |t: i64| (((t - first) / bucket).max(0) as usize).min(n_buckets - 1);
    let range = TimeRange::new(op_core::time::from_unix_ms(from - BATTERY_CARRY_MS), op_core::time::from_unix_ms(to));
    // Parquet days: the readings in range and the latest before it.
    let (d0, d1) = (range::date_of(range.from_ms()), range::date_of(to - 1));
    for (date, path) in crate::telemetry::list_days(ctx.data_dir(), "health") {
        if date < d0 || date > d1 {
            continue;
        }
        let wanted: std::collections::HashSet<String> = ids.iter().cloned().collect();
        let rows = tokio::task::spawn_blocking(move || cold_battery_day(&path, &wanted, from - BATTERY_CARRY_MS, to)).await??;
        for (id, t, v) in rows {
            let Some(b) = out.get_mut(&id) else { continue };
            if t < from {
                if b.before.is_none_or(|(bt, _)| t > bt) {
                    b.before = Some((t, v));
                }
            } else {
                b.add(idx(t), v, 1, (t, v));
            }
        }
    }
    let grouped = battery_sql(ids.len());
    let mut q = sqlx::query(&grouped).bind(first).bind(bucket).bind(from).bind(to);
    for id in ids {
        q = q.bind(id);
    }
    for r in q.fetch_all(ctx.db()).await? {
        let id: String = r.try_get("collar_id")?;
        let Some(b) = out.get_mut(&id) else { continue };
        let i = (r.try_get::<i64, _>("b")?.max(0) as usize).min(n_buckets - 1);
        b.add(i, r.try_get("s")?, r.try_get("n")?, (r.try_get("last_t")?, r.try_get("battery")?));
    }
    for id in ids {
        let row: Option<(i64, f64)> = sqlx::query_as(BATTERY_BEFORE_SQL).bind(id).bind(from).bind(from - BATTERY_CARRY_MS).fetch_optional(ctx.db()).await?;
        if let (Some((t, v)), Some(b)) = (row, out.get_mut(id)) {
            if b.before.is_none_or(|(bt, _)| t > bt) {
                b.before = Some((t, v));
            }
        }
    }
    Ok(out)
}

/// Battery readings of `n` collars grouped per bucket: `?1` first bucket
/// start, `?2` bucket ms, `[?3, ?4)`, then the collar ids. op-ingest's
/// `health` table is one row per report; the bare `battery` is the one at MAX(t).
pub fn battery_sql(n: usize) -> String {
    format!(
        "SELECT collar_id, (t - ?) / ? AS b, SUM(battery) AS s, COUNT(*) AS n, MAX(t) AS last_t, battery FROM health
         WHERE battery IS NOT NULL AND t >= ? AND t < ? AND collar_id IN ({}) GROUP BY collar_id, b",
        vec!["?"; n].join(",")
    )
}

/// A collar's last battery reading in `[?3, ?2)`.
pub const BATTERY_BEFORE_SQL: &str = "SELECT t, battery FROM health WHERE collar_id = ? AND battery IS NOT NULL AND t < ? AND t >= ? ORDER BY t DESC LIMIT 1";

/// `(collar, t, battery)` of one Parquet health day, only the wanted collars
/// in `[from, to)`.
fn cold_battery_day(path: &std::path::Path, wanted: &std::collections::HashSet<String>, from: i64, to: i64) -> anyhow::Result<Vec<(String, i64, f64)>> {
    use datafusion::arrow::array::Array;
    use parquet::arrow::ProjectionMask;
    use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;

    let builder = ParquetRecordBatchReaderBuilder::try_new(std::fs::File::open(path)?)?;
    let mask = ProjectionMask::columns(builder.parquet_schema(), ["collar_id", "t", "battery"]);
    let mut out = Vec::new();
    for b in builder.with_projection(mask).with_batch_size(crate::telemetry::BATCH_ROWS).build()? {
        let b = b?;
        let c = crate::schema::Cols::new(&b);
        let (Some(ids), Some(ts), Some(vs)) = (c.str("collar_id"), c.i64("t"), c.f64("battery")) else { continue };
        for i in 0..b.num_rows() {
            if ids.is_null(i) || ts.is_null(i) || vs.is_null(i) {
                continue;
            }
            let t = ts.value(i);
            if t >= from && t < to && wanted.contains(ids.value(i)) {
                out.push((ids.value(i).to_owned(), t, vs.value(i)));
            }
        }
    }
    Ok(out)
}

async fn latest_acks(ctx: &Ctx, ids: &[String]) -> anyhow::Result<HashMap<String, Value>> {
    let mut out = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let sql =
        format!("SELECT collar_id, version, status, reason, at, MAX(id) FROM acks WHERE collar_id IN ({}) GROUP BY collar_id", vec!["?"; ids.len()].join(","));
    let mut q = sqlx::query(&sql);
    for id in ids {
        q = q.bind(id);
    }
    for r in q.fetch_all(ctx.db()).await? {
        let v = json!({
            "version": r.try_get::<i64, _>("version")?,
            "status": r.try_get::<String, _>("status")?,
            "reason": r.try_get::<Option<String>, _>("reason")?,
            "at": r.try_get::<String, _>("at")?,
        });
        out.insert(r.try_get("collar_id")?, v);
    }
    Ok(out)
}

// ---------------------------------------------------------------- behaviour

#[derive(Debug, Serialize)]
pub struct Learning {
    /// Change in cues per day, per day (least squares).
    pub slope_per_day: f64,
    /// "falling" (learning the line), "rising", or "flat".
    pub trend: &'static str,
}

#[derive(Debug, Serialize)]
pub struct AnimalBehaviour {
    pub animal_id: Option<String>,
    pub collar_id: Option<String>,
    pub tag: Option<String>,
    pub name: Option<String>,
    pub fixes: i64,
    pub distance_km: Option<f64>,
    pub paddock_hours: BTreeMap<String, f64>,
    /// Hours at fixes outside every paddock.
    pub outside_hours: f64,
    /// UTC dates for `cues_per_day`.
    pub days: Vec<String>,
    pub cues_per_day: Vec<i64>,
    pub cues: i64,
    pub learning: Option<Learning>,
}

pub async fn behaviour(State(ctx): State<Ctx>, Query(p): Query<Params>) -> ApiResult<Json<Vec<AnimalBehaviour>>> {
    let range = p.range(Duration::hours(24))?;
    Ok(Json(animal_behaviour(&ctx, range, p.herd()).await?))
}

#[derive(Default)]
struct CollarBehaviour {
    animal_id: Option<String>,
    fixes: i64,
    walk: Walk,
    cues_by_day: HashMap<i64, i64>,
}

pub async fn animal_behaviour(ctx: &Ctx, range: TimeRange, herd: Option<&str>) -> anyhow::Result<Vec<AnimalBehaviour>> {
    let paddocks = PaddockIndex::new(&ctx.store().list_paddocks().await?);
    let scope = Scope { collar_ids: None, herd_id: herd.map(str::to_owned) };
    let mut per: BTreeMap<String, CollarBehaviour> = BTreeMap::new();
    let mut dwell = Dwell::default();
    // @L: days with a Parquet file are read from it row by row; the fixes
    // after them are summed in SQLite (see `hot_behaviour`).
    let (from, to) = (range.from_ms(), range.to_ms());
    let end = to.min(op_core::time::now().timestamp_millis());
    let cold = {
        let (dir, r) = (ctx.data_dir().to_path_buf(), range);
        tokio::task::spawn_blocking(move || crate::telemetry::rolled_days(&dir, "fixes", Some(&r))).await??
    };
    let hot_from = cold.iter().map(|r| r.to).max().unwrap_or(from).clamp(from, to);
    let cold_range = TimeRange::new(range.from, op_core::time::from_unix_ms(hot_from));
    // Those days' files, then what SQLite holds for them beyond the files (a
    // late fix stored after its day was rolled, until the next rollup).
    for_each(scan_in(ctx, "fixes", cold_range, scope.clone(), Source::All, Order::Time), |b| {
        each_fix(b, |f| {
            if !per.contains_key(f.collar_id) {
                per.insert(f.collar_id.to_owned(), CollarBehaviour::default());
            }
            let c = per.get_mut(f.collar_id).expect("inserted");
            c.fixes += 1;
            if let Some(a) = f.animal_id {
                if c.animal_id.as_deref() != Some(a) {
                    c.animal_id = Some(a.to_owned());
                }
            }
            c.walk.push(f.t, [f.lon, f.lat], f.accuracy_m);
            dwell.push(f.collar_id, None, paddocks.resolve(f.paddock_id, [f.lon, f.lat]), f.t);
        })
    })
    .await?;
    for_each(scan_in(ctx, "cues", range, scope, Source::All, Order::Time), |b| {
        each_cue(b, |q| {
            if !per.contains_key(q.collar_id) {
                per.insert(q.collar_id.to_owned(), CollarBehaviour::default());
            }
            let c = per.get_mut(q.collar_id).expect("inserted");
            if c.animal_id.is_none() {
                c.animal_id = q.animal_id.map(str::to_owned);
            }
            *c.cues_by_day.entry(q.t.div_euclid(DAY_MS)).or_default() += 1;
        })
    })
    .await?;
    let mut dwell = dwell.finish(hot_from.min(end));
    if hot_from < to {
        hot_behaviour(ctx, hot_from, to, herd, &mut per).await?;
        for (k, agg) in hot_dwell(ctx, hot_from, end, herd, &paddocks).await? {
            let e = dwell.entry((k.0, String::new(), k.2, k.3)).or_default();
            e.fixes += agg.fixes;
            e.dwell_ms += agg.dwell_ms;
            e.last_t = e.last_t.max(agg.last_t);
        }
    }

    // Animals by id and by collar, for collars whose telemetry lacks animal_id.
    let animals = ctx.store().list_animals(herd).await?;
    let mut by_collar: HashMap<String, String> = animals.iter().filter_map(|a| Some((a.collar_id.clone()?, a.id.clone()))).collect();
    for c in collars(ctx, herd, None).await? {
        if let Some(a) = c.animal_id {
            by_collar.entry(c.id).or_insert(a);
        }
    }
    let animal_meta: HashMap<&str, &op_core::Animal> = animals.iter().map(|a| (a.id.as_str(), a)).collect();

    let (d0, d1) = (range.from_ms().div_euclid(DAY_MS), (range.to_ms() - 1).div_euclid(DAY_MS));
    let days: Vec<String> = (d0..=d1).map(|d| rfc3339(d * DAY_MS)[..10].to_owned()).collect();

    struct Group {
        collar_id: String,
        fixes: i64,
        metres: f64,
        cues: HashMap<i64, i64>,
        paddock_ms: BTreeMap<String, i64>,
        outside_ms: i64,
    }
    let mut groups: BTreeMap<(Option<String>, String), Group> = BTreeMap::new();
    let key_of = |collar: &str, c: &CollarBehaviour| -> (Option<String>, String) {
        match c.animal_id.clone().or_else(|| by_collar.get(collar).cloned()) {
            Some(a) => (Some(a), String::new()),
            None => (None, collar.to_owned()),
        }
    };
    for (collar, c) in &per {
        let g = groups.entry(key_of(collar, c)).or_insert_with(|| Group {
            collar_id: collar.clone(),
            fixes: 0,
            metres: 0.0,
            cues: HashMap::new(),
            paddock_ms: BTreeMap::new(),
            outside_ms: 0,
        });
        if c.fixes > 0 {
            g.collar_id = collar.clone();
        }
        g.fixes += c.fixes;
        g.metres += c.walk.metres;
        for (d, n) in &c.cues_by_day {
            *g.cues.entry(*d).or_default() += n;
        }
    }
    for ((_, _, collar, paddock), agg) in dwell {
        let Some(c) = per.get(&collar) else { continue };
        let Some(g) = groups.get_mut(&key_of(&collar, c)) else { continue };
        if paddock.is_empty() {
            g.outside_ms += agg.dwell_ms;
        } else {
            *g.paddock_ms.entry(paddock).or_default() += agg.dwell_ms;
        }
    }

    Ok(groups
        .into_iter()
        .map(|((animal_id, _), g)| {
            let cues_per_day: Vec<i64> = (d0..=d1).map(|d| g.cues.get(&d).copied().unwrap_or(0)).collect();
            let meta = animal_id.as_deref().and_then(|a| animal_meta.get(a));
            let learning = (g.fixes > 0 && cues_per_day.len() >= 3).then(|| {
                let y: Vec<f64> = cues_per_day.iter().map(|v| *v as f64).collect();
                let s = slope(&y).unwrap_or(0.0);
                let m = y.iter().sum::<f64>() / y.len() as f64;
                let trend = if m == 0.0 || (s / m).abs() < 0.05 {
                    "flat"
                } else if s < 0.0 {
                    "falling"
                } else {
                    "rising"
                };
                Learning { slope_per_day: round(s, 3), trend }
            });
            AnimalBehaviour {
                tag: meta.map(|a| a.tag.clone()),
                name: meta.and_then(|a| a.name.clone()),
                animal_id,
                collar_id: Some(g.collar_id),
                fixes: g.fixes,
                distance_km: (g.fixes > 0).then(|| round(g.metres / 1000.0, 3)),
                paddock_hours: g.paddock_ms.into_iter().map(|(k, v)| (k, round(v as f64 / 3_600_000.0, 2))).collect(),
                outside_hours: round(g.outside_ms as f64 / 3_600_000.0, 2),
                cues: cues_per_day.iter().sum(),
                days: days.clone(),
                cues_per_day,
                learning,
            }
        })
        .collect())
}

// @L
/// Hot fixes more than this in a behaviour range: walks come from
/// [`WALK_SAMPLES`] fixes a collar at most, the first of each time bucket.
pub const WALK_ROWS: i64 = 20_000;
pub const WALK_SAMPLES: i64 = 2_000;
/// A walk's buckets are at least this long (ms): a fix a minute.
const WALK_BUCKET_MIN_MS: i64 = 60_000;

/// Hot fixes per collar in `[?, ?)` (and the herd), from the collar index alone.
pub fn hot_counts_sql(herd: bool) -> String {
    let herd_filter = if herd { " AND herd_id = ?" } else { "" };
    format!("SELECT collar_id, COUNT(*) FROM fixes WHERE t >= ? AND t < ?{herd_filter} GROUP BY collar_id")
}

/// The animal a collar's fixes in `[?3, ?2)` name (`?1` collar, `?4` herd
/// or NULL): the newest of its last 200 that names one, else of its first
/// 200 (an animal's collar taken off partway through keeps reporting with
/// none, and the stretch before is still that animal's), so a collar with
/// no animal costs 400 rows, not its day.
pub const HOT_ANIMAL_SQL: &str = "SELECT animal_id FROM (
         SELECT * FROM (SELECT animal_id, t, id FROM fixes WHERE collar_id = ?1 AND t >= ?3 AND t < ?2 AND (?4 IS NULL OR herd_id = ?4) ORDER BY t DESC, id DESC LIMIT 200)
         UNION ALL
         SELECT * FROM (SELECT animal_id, t, id FROM fixes WHERE collar_id = ?1 AND t >= ?3 AND t < ?2 AND (?4 IS NULL OR herd_id = ?4) ORDER BY t, id LIMIT 200)
     ) WHERE animal_id IS NOT NULL ORDER BY t DESC, id DESC LIMIT 1";

/// A collar's first fix in each of `?5 + 1` buckets of `?4` ms from `?3`
/// (to `?2`; `?1` collar, `?6` herd or NULL), one index seek a bucket.
pub const WALK_SAMPLES_SQL: &str = "WITH RECURSIVE b(k) AS (SELECT 0 UNION ALL SELECT k + 1 FROM b WHERE k < ?5)
     SELECT f.t, f.lon, f.lat, f.accuracy_m FROM b JOIN fixes f ON f.id = (
         SELECT id FROM fixes WHERE collar_id = ?1 AND (?6 IS NULL OR herd_id = ?6) AND t >= ?3 + k * ?4 AND t < min(?2, ?3 + (k + 1) * ?4)
         ORDER BY t, id LIMIT 1)
     ORDER BY f.t";

/// Every hot fix of `[?, ?)` (and the herd) a walk needs, per collar in time order.
pub fn walk_rows_sql(herd: bool) -> String {
    let herd_filter = if herd { " AND herd_id = ?" } else { "" };
    format!("SELECT collar_id, t, lon, lat, accuracy_m FROM fixes WHERE t >= ? AND t < ?{herd_filter} ORDER BY collar_id, t, id")
}

/// Behaviour's hot part: fix counts and each collar's animal, and its walk,
/// from every hot fix when the range holds at most [`WALK_ROWS`], else from
/// one fix a bucket (a day of 250 collars is 4.3 M fixes; its walks come from
/// 1,440 a collar). Dwell is [`hot_dwell`]'s; cues are the scan's.
async fn hot_behaviour(ctx: &Ctx, from: i64, to: i64, herd: Option<&str>, per: &mut BTreeMap<String, CollarBehaviour>) -> anyhow::Result<()> {
    let sql = hot_counts_sql(herd.is_some());
    let mut q = sqlx::query_as::<_, (String, i64)>(&sql).bind(from).bind(to);
    if let Some(h) = herd {
        q = q.bind(h);
    }
    let counts = q.fetch_all(ctx.db()).await?;
    let total: i64 = counts.iter().map(|c| c.1).sum();
    let herd_owned = herd.map(str::to_owned);
    let names: Vec<String> = counts.iter().map(|c| c.0.clone()).collect();
    let reads = names.clone().into_iter().map(|collar| {
        let (ctx, herd) = (ctx.clone(), herd_owned.clone());
        async move {
            let a: Option<String> = sqlx::query_scalar(HOT_ANIMAL_SQL).bind(&collar).bind(to).bind(from).bind(herd).fetch_optional(ctx.db()).await?;
            anyhow::Ok((collar, a))
        }
    });
    let animals: Vec<(String, Option<String>)> = futures::stream::iter(reads).buffer_unordered(PARALLEL_READS).try_collect().await?;
    for (collar, n) in &counts {
        per.entry(collar.clone()).or_default().fixes += n;
    }
    for (collar, animal) in animals {
        if animal.is_some() {
            per.entry(collar).or_default().animal_id = animal;
        }
    }
    if total <= WALK_ROWS {
        let sql = walk_rows_sql(herd.is_some());
        let mut q = sqlx::query_as::<_, (String, i64, f64, f64, Option<f64>)>(&sql).bind(from).bind(to);
        if let Some(h) = herd {
            q = q.bind(h);
        }
        for (collar, t, lon, lat, acc) in q.fetch_all(ctx.db()).await? {
            per.entry(collar).or_default().walk.push(t, [lon, lat], acc);
        }
        return Ok(());
    }
    let width = ((to - from) / WALK_SAMPLES).max(WALK_BUCKET_MIN_MS);
    let last = (to - from - 1) / width;
    let reads = names.into_iter().map(|collar| {
        let (ctx, herd) = (ctx.clone(), herd_owned.clone());
        async move {
            let rows: Vec<(i64, f64, f64, Option<f64>)> =
                sqlx::query_as(WALK_SAMPLES_SQL).bind(&collar).bind(to).bind(from).bind(width).bind(last).bind(herd).fetch_all(ctx.db()).await?;
            anyhow::Ok((collar, rows))
        }
    });
    let walks: Vec<(String, Vec<(i64, f64, f64, Option<f64>)>)> = futures::stream::iter(reads).buffer_unordered(PARALLEL_READS).try_collect().await?;
    for (collar, rows) in walks {
        let c = per.entry(collar).or_default();
        for (t, lon, lat, acc) in rows {
            c.walk.push(t, [lon, lat], acc);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- heatmap

pub async fn heatmap(State(ctx): State<Ctx>, Query(p): Query<Params>) -> ApiResult<Json<Vec<[f64; 3]>>> {
    let range = p.range(Duration::hours(24))?;
    let cell_m: f64 = parse_num(p.cell_m.as_deref(), "cell_m")?.unwrap_or(10.0);
    if !(1.0..=1000.0).contains(&cell_m) {
        return Err(ApiError::bad_request("`cell_m` must be between 1 and 1000."));
    }
    let normalize = !matches!(p.normalize.as_deref(), Some("false" | "0" | "no"));
    Ok(Json(heat_points(&ctx, range, p.herd(), p.collar(), cell_m, normalize).await?))
}

/// Fix counts on a square grid of `cell_m` metres around the farm centre.
pub async fn heat_points(ctx: &Ctx, range: TimeRange, herd: Option<&str>, collar: Option<&str>, cell_m: f64, normalize: bool) -> anyhow::Result<Vec<[f64; 3]>> {
    let origin = ctx.store().get_farm().await?.map(|f| f.center);
    let scope = Scope { collar_ids: collar.map(|c| vec![c.to_owned()]), herd_id: herd.map(str::to_owned) };
    let mut grid: Option<Grid> = origin.map(|o| Grid::new(o, cell_m));
    // @L: with the farm's centre known, the hot fixes are counted per cell in SQLite (below).
    let (rx, read) = match origin {
        Some(_) => {
            let (rx, read) = scan_cold(ctx, "fixes", range, scope, Order::Time);
            (rx, Some(read))
        }
        None => (scan_in(ctx, "fixes", range, scope, Source::All, Order::Time), None),
    };
    for_each(rx, |b| {
        each_fix(b, |f| {
            grid.get_or_insert_with(|| Grid::new([f.lon, f.lat], cell_m)).add([f.lon, f.lat], 1.0);
        })
    })
    .await?;
    let rolled = match read {
        Some(read) => read.await.unwrap_or_default(),
        None => Vec::new(),
    };
    if let Some(o) = origin {
        let g = grid.get_or_insert_with(|| Grid::new(o, cell_m));
        let (from, to) = (range.from_ms(), range.to_ms());
        let rolled: Vec<_> = rolled.into_iter().filter(|r| r.from < to && r.to > from).collect();
        let p = op_geo::Projection::new(o);
        let sql = heat_hot_sql(herd.is_some(), collar.is_some(), rolled.len());
        let mut q = sqlx::query(&sql)
            .bind(p.lon0)
            .bind(p.m_per_deg_lon)
            .bind(cell_m)
            .bind(p.lat0)
            .bind(op_geo::projection::M_PER_DEG_LAT)
            .bind(cell_m)
            .bind(from)
            .bind(to);
        if let Some(h) = herd {
            q = q.bind(h);
        }
        if let Some(c) = collar {
            q = q.bind(c);
        }
        for r in &rolled {
            q = q.bind(r.from).bind(r.to).bind(r.max_id);
        }
        for row in q.fetch_all(ctx.db()).await? {
            *g.cells.entry((row.try_get(0)?, row.try_get(1)?)).or_default() += row.try_get::<i64, _>(2)? as f64;
        }
    }
    Ok(grid.map(|g| g.points(normalize)).unwrap_or_default())
}

// @L
/// The hot fixes counted per grid cell in SQLite, exactly as [`Grid::add`]
/// bins them: binds are the origin's longitude, metres per degree of
/// longitude, the cell, the origin's latitude, metres per degree of
/// latitude, the cell, then from, to, the herd and collar when asked for,
/// and the rolled days (see [`crate::telemetry::Rolled`]).
pub fn heat_hot_sql(herd: bool, collar: bool, rolled: usize) -> String {
    let mut sql = "WITH p AS (SELECT ((lon - ?) * ?) / ? AS x, ((lat - ?) * ?) / ? AS y FROM fixes WHERE t >= ? AND t < ?".to_owned();
    if herd {
        sql.push_str(" AND herd_id = ?");
    }
    if collar {
        sql.push_str(" AND collar_id = ?");
    }
    sql.push_str(&crate::telemetry::not_rolled_sql(rolled));
    sql.push_str(
        ") SELECT CAST(x AS INTEGER) - (x < CAST(x AS INTEGER)) AS cx, CAST(y AS INTEGER) - (y < CAST(y AS INTEGER)) AS cy, COUNT(*) FROM p GROUP BY cx, cy",
    );
    sql
}

// ---------------------------------------------------------------- pasture

#[derive(Debug, Serialize)]
pub struct PaddockPasture {
    pub paddock_id: String,
    pub name: String,
    pub area_ha: f64,
    pub status: String,
    /// Days in the window the herd spent at least an hour's share of its day here.
    pub grazing_days: Option<i64>,
    pub rest_days: Option<f64>,
    /// AU-days per hectare over the window.
    pub pressure: Option<f64>,
    pub au_days: Option<f64>,
    pub last_grazed: Option<String>,
    pub ndvi: Option<f64>,
    pub herds: Vec<String>,
}

/// Share of a herd's tracked day that makes it a grazing day for a paddock.
const GRAZING_DAY_SHARE: f64 = 1.0 / 24.0;

pub async fn pasture(State(ctx): State<Ctx>, Query(p): Query<Params>) -> ApiResult<Json<Vec<PaddockPasture>>> {
    let range = p.range(Duration::days(90))?;
    Ok(Json(paddock_pasture(&ctx, range, p.herd()).await?))
}

/// Per paddock: grazing days, AU-days/ha and last grazed, from daily dwell.
/// Rolled and imported days come from the `paddock_days` view
/// (`analytics_paddock_days` plus `imported_paddock_days`), the rest from SQLite.
/// Pressure follows the agent kit: a herd's AU times its share of the day
/// in the paddock, summed over days.
///
/// Everything is as of the end of the range (or now, if sooner): no day or
/// fix after it counts, `last_grazed` is the last grazing day up to it (days
/// before `from` included, from the daily summaries), and rest runs to it.
/// A day the range's end falls inside counts whole once it is rolled.
pub async fn paddock_pasture(ctx: &Ctx, range: TimeRange, herd: Option<&str>) -> anyhow::Result<Vec<PaddockPasture>> {
    let now = op_core::time::now();
    let end = range.to.min(now).max(range.from + Duration::milliseconds(1));
    let end_ms = end.timestamp_millis();
    // (day, herd, paddock) -> (dwell_ms, last_t)
    let mut days: HashMap<(i64, String, String), (f64, i64)> = HashMap::new();
    let mut sql = "SELECT date, herd_id, paddock_id, SUM(dwell_s) AS dwell, MAX(last_t) AS last_t FROM paddock_days WHERE date <= ?".to_owned();
    if herd.is_some() {
        sql.push_str(" AND herd_id = ?");
    }
    sql.push_str(" GROUP BY date, herd_id, paddock_id");
    let mut q = sqlx::query(&sql).bind(range::date_of(end_ms - 1).format("%Y-%m-%d").to_string());
    if let Some(h) = herd {
        q = q.bind(h);
    }
    for r in q.fetch_all(ctx.db()).await? {
        let date: String = r.try_get("date")?;
        let Ok(d) = NaiveDate::parse_from_str(&date, "%Y-%m-%d") else { continue };
        let day = crate::range::day_start_ms(d).div_euclid(DAY_MS);
        let e = days.entry((day, r.try_get("herd_id")?, r.try_get("paddock_id")?)).or_default();
        e.0 += r.try_get::<f64, _>("dwell")? * 1000.0;
        e.1 = e.1.max(r.try_get("last_t")?);
    }

    // Hot fixes after the last day the rollup summarised (a rolled day's rows
    // may still be in SQLite while it deletes them), up to the end.
    let rolled: Option<String> = sqlx::query_scalar("SELECT MAX(date) FROM analytics_paddock_days").fetch_one(ctx.db()).await?;
    let hot_from =
        rolled.and_then(|d| NaiveDate::parse_from_str(&d, "%Y-%m-%d").ok()).map(|d| crate::range::day_start_ms(d) + DAY_MS).unwrap_or(i64::MIN / 2).max(0);
    let paddock_list = ctx.store().list_paddocks().await?;
    let index = PaddockIndex::new(&paddock_list);
    if hot_from < end_ms {
        for ((day, h, _collar, paddock), agg) in hot_dwell(ctx, hot_from, end_ms, herd, &index).await? {
            let e = days.entry((day, h, paddock)).or_default();
            e.0 += agg.dwell_ms as f64;
            e.1 = e.1.max(agg.last_t);
        }
    }

    let mut herd_total: HashMap<(i64, String), f64> = HashMap::new();
    for ((day, h, _), (ms, _)) in &days {
        *herd_total.entry((*day, h.clone())).or_default() += ms;
    }
    let au: HashMap<String, f64> = ctx.store().list_herds().await?.into_iter().map(|h| (h.id, au_per_head(&h.species.as_db()) * h.count as f64)).collect();

    let (w0, w1) = (range.from_ms().div_euclid(DAY_MS), (range.to_ms() - 1).div_euclid(DAY_MS));
    let any_in_window = herd_total.iter().any(|((d, _), ms)| *d >= w0 && *d <= w1 && *ms > 0.0);

    struct Acc {
        grazing: BTreeSet<i64>,
        au_days: f64,
        last: Option<i64>,
        herds: BTreeSet<String>,
    }
    let mut per: HashMap<String, Acc> = HashMap::new();
    for ((day, h, paddock), (ms, last_t)) in &days {
        if paddock.is_empty() {
            continue;
        }
        let total = herd_total.get(&(*day, h.clone())).copied().unwrap_or(0.0);
        if total <= 0.0 {
            continue;
        }
        let share = ms / total;
        let a = per.entry(paddock.clone()).or_insert_with(|| Acc { grazing: BTreeSet::new(), au_days: 0.0, last: None, herds: BTreeSet::new() });
        if share >= GRAZING_DAY_SHARE {
            a.last = Some(a.last.map_or(*last_t, |l| l.max(*last_t)));
        }
        if *day >= w0 && *day <= w1 {
            if share >= GRAZING_DAY_SHARE {
                a.grazing.insert(*day);
            }
            if let Some(u) = au.get(h) {
                a.au_days += u * share;
            }
            if !h.is_empty() {
                a.herds.insert(h.clone());
            }
        }
    }

    let ndvi = latest_ndvi(ctx).await?;
    Ok(paddock_list
        .into_iter()
        .map(|p| {
            let a = per.remove(&p.id);
            let last = a.as_ref().and_then(|a| a.last);
            let au_days = any_in_window.then(|| a.as_ref().map_or(0.0, |a| a.au_days));
            PaddockPasture {
                grazing_days: any_in_window.then(|| a.as_ref().map_or(0, |a| a.grazing.len() as i64)),
                rest_days: last.map(|l| round(((end_ms - l).max(0)) as f64 / DAY_MS as f64, 1)),
                pressure: au_days.and_then(|d| (p.area_ha > 0.0).then(|| round(d / p.area_ha, 2))),
                au_days: au_days.map(|d| round(d, 2)),
                last_grazed: last.map(rfc3339),
                ndvi: ndvi.get(&p.id).copied(),
                herds: a.map(|a| a.herds.into_iter().collect()).unwrap_or_default(),
                status: p.status.as_db(),
                paddock_id: p.id,
                name: p.name,
                area_ha: round(p.area_ha, 3),
            }
        })
        .collect())
}

/// The hot dwell in `[?1, ?2)` (`?3` herd or NULL, `?4` the longest gap),
/// the way [`Dwell`] sums it: each fix's time until its collar's next one,
/// capped at the gap and the end of its UTC day (the last fix's until
/// `?2`), goes to the fix's day, herd and stored paddock. SQLite does the
/// window and the sums in one pass over the range sorted by collar (@L: per
/// collar by index, 250 interleaved collars read every page of a day 250
/// times); fixes whose stored paddock is gone or unknown come back one by
/// one with their point, for the caller to place.
pub fn hot_dwell_sql(herd: bool) -> String {
    let herd_filter = if herd { "AND herd_id = ?3" } else { "AND ?3 IS NULL" };
    format!(
        "WITH w AS (
             SELECT collar_id, herd_id, t, lon, lat, CASE WHEN paddock_id IN (SELECT id FROM paddocks) THEN paddock_id END AS pad,
                    LEAD(t) OVER (PARTITION BY collar_id ORDER BY t, id) AS nt
             FROM fixes WHERE t >= ?1 AND t < ?2 {herd_filter}
         ), d AS (
             SELECT collar_id, COALESCE(herd_id, '') AS herd, t, lon, lat, pad,
                    MIN(COALESCE(nt, ?2) - t, ?4, (t / {DAY_MS} + 1) * {DAY_MS} - t) AS dt
             FROM w
         )
         SELECT collar_id, t / {DAY_MS} AS day, herd, pad, COUNT(*) AS n, SUM(dt) AS dwell, MAX(t) AS last_t, NULL AS lon, NULL AS lat
         FROM d WHERE pad IS NOT NULL GROUP BY collar_id, day, herd, pad
         UNION ALL
         SELECT collar_id, t / {DAY_MS}, herd, NULL, 1, dt, t, lon, lat FROM d WHERE pad IS NULL"
    )
}

/// Dwell per (day, herd, collar, paddock) of the hot fixes in `[from, end)`,
/// summed in SQLite in one pass: a day of 250 collars is never decoded row
/// by row. The rows are folded in as they stream: fixes with no stored
/// paddock (none drawn yet, a lane, paddocks imported again with new ids)
/// come one per fix, 4.3 M a day at 250 collars, and are never all held.
pub async fn hot_dwell(ctx: &Ctx, from: i64, end: i64, herd: Option<&str>, index: &PaddockIndex) -> anyhow::Result<HashMap<DwellKey, DwellAgg>> {
    let sql = hot_dwell_sql(herd.is_some());
    let mut rows = sqlx::query(&sql).bind(from).bind(end).bind(herd).bind(MAX_DWELL_GAP_MS).fetch(ctx.db());
    let mut out: HashMap<DwellKey, DwellAgg> = HashMap::new();
    while let Some(r) = rows.try_next().await? {
        let pad: Option<String> = r.try_get("pad")?;
        let paddock = match pad {
            Some(p) => p,
            None => {
                let (lon, lat): (Option<f64>, Option<f64>) = (r.try_get("lon")?, r.try_get("lat")?);
                lon.zip(lat).and_then(|(lon, lat)| index.locate([lon, lat])).unwrap_or("").to_owned()
            }
        };
        let e = out.entry((r.try_get("day")?, r.try_get("herd")?, r.try_get("collar_id")?, paddock)).or_default();
        e.fixes += r.try_get::<i64, _>("n")?;
        e.dwell_ms += r.try_get::<i64, _>("dwell")?;
        e.last_t = e.last_t.max(r.try_get("last_t")?);
    }
    Ok(out)
}

// @L
/// Per-collar reads of one request run this many at a time (on as many
/// pooled connections; WAL readers don't wait on each other or on reports).
pub const PARALLEL_READS: usize = 6;

/// Latest imagery NDVI mean per paddock from op-engine's land reports
/// (`land_reports`), only where the imagery section came back ok.
async fn latest_ndvi(ctx: &Ctx) -> anyhow::Result<HashMap<String, f64>> {
    let rows = sqlx::query(
        "SELECT paddock_id, json_extract(report, '$.sections.imagery.ndvi_stats.mean') AS ndvi FROM land_reports \
         WHERE paddock_id IS NOT NULL AND json_extract(report, '$.sections.imagery.status') = 'ok' \
         AND json_extract(report, '$.sections.imagery.ndvi_stats.mean') IS NOT NULL ORDER BY as_of DESC",
    )
    .fetch_all(ctx.db())
    .await?;
    let mut out = HashMap::new();
    for r in rows {
        let (Ok(id), Ok(v)) = (r.try_get::<String, _>("paddock_id"), r.try_get::<f64, _>("ndvi")) else { continue };
        if v.is_finite() {
            out.entry(id).or_insert(round(v, 3));
        }
    }
    Ok(out)
}

// ---------------------------------------------------------------- sql

#[derive(Deserialize)]
pub struct SqlBody {
    pub query: String,
    pub limit: Option<usize>,
}

pub async fn run_sql(State(ctx): State<Ctx>, ApiJson(body): ApiJson<SqlBody>) -> ApiResult<Json<sql::SqlResult>> {
    if body.query.trim().is_empty() {
        return Err(ApiError::bad_request("Write a query."));
    }
    Ok(Json(sql::run(&ctx, &body.query, body.limit.unwrap_or(sql::MAX_ROWS)).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn collar(created: &str) -> CollarInfo {
        let t = DateTime::parse_from_rfc3339(created).unwrap().timestamp_millis();
        CollarInfo {
            id: "c".into(),
            name: "c".into(),
            herd_id: "h".into(),
            animal_id: None,
            battery: None,
            boundary_version: None,
            created_ms: Some(t),
            cadence_s: None,
        }
    }

    #[test]
    fn health_buckets_the_span_collars_cover() {
        let now = DateTime::parse_from_rfc3339("2026-09-26T12:00:00Z").unwrap().with_timezone(&Utc);
        let day = TimeRange::new(now - Duration::hours(24), now);
        // Linked 40 minutes ago: one-minute buckets from then.
        let r = data_span(day, &[collar("2026-09-26T11:20:00Z"), collar("2026-09-26T11:30:00Z")], now);
        assert_eq!(r.from.to_rfc3339(), "2026-09-26T11:20:00+00:00");
        assert_eq!(bucket_ms(None, &r, 150).unwrap(), 60_000);
        // Linked last week: the whole day, ten-minute buckets.
        let r = data_span(day, &[collar("2026-09-19T00:00:00Z")], now);
        assert_eq!(bucket_ms(None, &r, 150).unwrap(), 600_000);
        // Linked a minute ago: still ten minutes wide.
        assert_eq!(data_span(day, &[collar("2026-09-26T11:59:00Z")], now).span_ms(), 600_000);
    }
}
