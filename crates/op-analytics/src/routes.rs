//! HTTP handlers for tracks, health, behaviour, heatmap, pasture and SQL.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use axum::Json;
use axum::extract::{Query, State};
use chrono::{DateTime, Duration, NaiveDate, Utc};
use op_core::domain::DbEnum;
use op_core::{ApiError, ApiJson, ApiResult, Ctx};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

use crate::metrics::{Cadence, Dwell, Grid, PaddockIndex, Walk, au_per_head, percentile, round, slope};
use crate::range::{TimeRange, bucket_ms};
use crate::schema::table_schema;
use crate::sql;
use crate::telemetry::{Scope, Source, each_cue, each_fix, for_each, scan};

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

#[derive(Serialize)]
pub struct Track {
    pub collar_id: String,
    pub points: Vec<[f64; 3]>,
}

#[derive(Default)]
struct TrackAcc {
    points: Vec<[f64; 3]>,
    bucket: Option<i64>,
    last: Option<(i64, [f64; 3])>,
    last_emitted: i64,
}

pub async fn tracks(State(ctx): State<Ctx>, Query(p): Query<Params>) -> ApiResult<Json<Vec<Track>>> {
    let range = p.range(Duration::hours(24))?;
    let max_points: usize = parse_num(p.max_points.as_deref(), "max_points")?.unwrap_or(2000).clamp(2, 50_000);
    Ok(Json(track_points(&ctx, range, p.collar(), p.herd(), max_points).await?))
}

/// Tracks downsampled by time: the first fix in each of `max_points` equal
/// time buckets, plus each collar's latest fix.
pub async fn track_points(ctx: &Ctx, range: TimeRange, collar: Option<&str>, herd: Option<&str>, max_points: usize) -> anyhow::Result<Vec<Track>> {
    let scope = Scope { collar_ids: collar.map(|c| vec![c.to_owned()]), herd_id: herd.map(str::to_owned) };
    let width = (range.span_ms() / max_points as i64).max(1);
    let from = range.from_ms();
    let mut accs: BTreeMap<String, TrackAcc> = BTreeMap::new();
    for_each(scan(ctx, "fixes", range, scope, Source::All), |b| {
        each_fix(b, |f| {
            if !accs.contains_key(f.collar_id) {
                accs.insert(f.collar_id.to_owned(), TrackAcc::default());
            }
            let a = accs.get_mut(f.collar_id).expect("inserted");
            if a.last.is_some_and(|(t, _)| f.t <= t) {
                return;
            }
            let pt = [round(f.lon, 7), round(f.lat, 7), unix_s(f.t)];
            let bucket = (f.t - from) / width;
            if a.bucket != Some(bucket) {
                a.points.push(pt);
                a.bucket = Some(bucket);
                a.last_emitted = f.t;
            }
            a.last = Some((f.t, pt));
        })
    })
    .await?;
    Ok(accs
        .into_iter()
        .map(|(collar_id, mut a)| {
            if let Some((t, pt)) = a.last {
                if t != a.last_emitted {
                    a.points.push(pt);
                }
            }
            Track { collar_id, points: a.points }
        })
        .collect())
}

// ---------------------------------------------------------------- health

#[derive(Default)]
struct HealthAcc {
    fixes: i64,
    /// Accuracy samples for percentiles, thinned to at most [`MAX_ACC_SAMPLES`].
    acc: Vec<f64>,
    /// Keep one sample in `acc_stride` once thinning started.
    acc_stride: u64,
    sats: (f64, i64),
    cn0: (f64, i64),
    ttf: (f64, i64),
    cues: i64,
}

/// Accuracy samples kept per bucket (and per collar summary) for p50/p95.
const MAX_ACC_SAMPLES: usize = 20_000;

impl HealthAcc {
    /// Keep a sample, halving what is kept (every other one, in time order)
    /// whenever the cap is reached, so memory stays bounded for long ranges.
    fn push_acc(&mut self, v: f64) {
        let stride = self.acc_stride.max(1);
        if (self.fixes as u64) % stride != 0 {
            return;
        }
        self.acc.push(v);
        if self.acc.len() >= MAX_ACC_SAMPLES {
            let mut i = 0;
            self.acc.retain(|_| {
                i += 1;
                i % 2 == 1
            });
            self.acc_stride = stride * 2;
        }
    }
}

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
    for_each(scan(ctx, "fixes", range, scope.clone(), Source::All), |b| {
        each_fix(b, |f| {
            let Some((buckets, total, cadence)) = per.get_mut(f.collar_id) else { return };
            cadence.push(f.t);
            for a in [&mut buckets[idx(f.t).min(n_buckets - 1)], total] {
                a.fixes += 1;
                if let Some(v) = f.accuracy_m {
                    a.push_acc(v);
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
    for_each(scan(ctx, "cues", range, scope, Source::All), |b| {
        each_cue(b, |c| {
            if let Some((buckets, total, _)) = per.get_mut(c.collar_id) {
                buckets[idx(c.t).min(n_buckets - 1)].cues += 1;
                total.cues += 1;
            }
        })
    })
    .await?;

    let battery = battery_readings(ctx, &ids, from, to).await?;
    let acks = latest_acks(ctx, &ids).await?;
    let now = op_core::time::now().timestamp_millis();

    let mut out = Vec::with_capacity(list.len());
    for c in list {
        let Some((buckets, mut total, cadence)) = per.remove(&c.id) else { continue };
        let cadence_s = c.cadence_s.or_else(|| cadence.median_s());
        let readings = battery.get(&c.id).map(Vec::as_slice).unwrap_or(&[]);
        let point = |a: &mut HealthAcc, start: i64, end: i64| -> HealthPoint {
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
                acc_p50: percentile(&mut a.acc, 0.5).map(|v| round(v, 2)),
                acc_p95: percentile(&mut a.acc, 0.95).map(|v| round(v, 2)),
                sats: mean(a.sats).map(|v| round(v, 1)),
                cn0: mean(a.cn0).map(|v| round(v, 1)),
                ttf_s: mean(a.ttf).map(|v| round(v, 1)),
                battery: battery_at(readings, start, end),
                cues: a.cues,
            }
        };
        let points: Vec<HealthPoint> =
            buckets.into_iter().enumerate().map(|(i, mut a)| point(&mut a, first + i as i64 * bucket, first + (i as i64 + 1) * bucket)).collect();
        let mut summary = point(&mut total, from, to);
        summary.t = rfc3339(from);
        summary.battery = battery_at(readings, from, to).or(c.battery);
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

/// Mean of the readings (0-1) in `[start, end)`, else the last earlier reading if
/// it is recent enough.
fn battery_at(readings: &[(i64, f64)], start: i64, end: i64) -> Option<f64> {
    let inside: Vec<f64> = readings.iter().filter(|(t, _)| *t >= start && *t < end).map(|(_, b)| *b).collect();
    if !inside.is_empty() {
        return Some(round(inside.iter().sum::<f64>() / inside.len() as f64, 3));
    }
    readings.iter().rev().find(|(t, _)| *t < start).filter(|(t, _)| start - t <= BATTERY_CARRY_MS).map(|(_, b)| round(*b, 3))
}

/// Battery readings per collar in `[from, to)`, plus the last one before `from`.
async fn battery_readings(ctx: &Ctx, ids: &[String], from: i64, to: i64) -> anyhow::Result<HashMap<String, Vec<(i64, f64)>>> {
    let mut out: HashMap<String, Vec<(i64, f64)>> = HashMap::new();
    if ids.is_empty() {
        return Ok(out);
    }
    let marks = vec!["?"; ids.len()].join(",");
    // op-ingest's `health` table: one row per position report.
    let before =
        format!("SELECT collar_id, MAX(t) AS t, battery FROM health WHERE battery IS NOT NULL AND t < ? AND collar_id IN ({marks}) GROUP BY collar_id");
    let within =
        format!("SELECT collar_id, t, battery FROM health WHERE battery IS NOT NULL AND t >= ? AND t < ? AND collar_id IN ({marks}) ORDER BY collar_id, t");
    let mut q = sqlx::query(&before).bind(from);
    for id in ids {
        q = q.bind(id);
    }
    let mut rows = q.fetch_all(ctx.db()).await?;
    let mut q = sqlx::query(&within).bind(from).bind(to);
    for id in ids {
        q = q.bind(id);
    }
    rows.extend(q.fetch_all(ctx.db()).await?);
    for r in rows {
        out.entry(r.try_get("collar_id")?).or_default().push((r.try_get("t")?, r.try_get("battery")?));
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
    for_each(scan(ctx, "fixes", range, scope.clone(), Source::All), |b| {
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
    for_each(scan(ctx, "cues", range, scope, Source::All), |b| {
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
    let dwell = dwell.finish(range.to_ms().min(op_core::time::now().timestamp_millis()));

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
    for_each(scan(ctx, "fixes", range, scope, Source::All), |b| {
        each_fix(b, |f| {
            grid.get_or_insert_with(|| Grid::new([f.lon, f.lat], cell_m)).add([f.lon, f.lat], 1.0);
        })
    })
    .await?;
    Ok(grid.map(|g| g.points(normalize)).unwrap_or_default())
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
pub async fn paddock_pasture(ctx: &Ctx, range: TimeRange, herd: Option<&str>) -> anyhow::Result<Vec<PaddockPasture>> {
    // (day, herd, paddock) -> (dwell_ms, last_t)
    let mut days: HashMap<(i64, String, String), (f64, i64)> = HashMap::new();
    let mut sql = "SELECT date, herd_id, paddock_id, SUM(dwell_s) AS dwell, MAX(last_t) AS last_t FROM paddock_days".to_owned();
    if herd.is_some() {
        sql.push_str(" WHERE herd_id = ?");
    }
    sql.push_str(" GROUP BY date, herd_id, paddock_id");
    let mut q = sqlx::query(&sql);
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

    let paddock_list = ctx.store().list_paddocks().await?;
    let index = PaddockIndex::new(&paddock_list);
    let now = op_core::time::now();
    let hot_range = TimeRange::new(DateTime::<Utc>::UNIX_EPOCH, now + Duration::days(1));
    let mut dwell = Dwell::default();
    let scope = Scope { collar_ids: None, herd_id: herd.map(str::to_owned) };
    for_each(scan(ctx, "fixes", hot_range, scope, Source::Hot), |b| {
        each_fix(b, |f| dwell.push(f.collar_id, f.herd_id, index.resolve(f.paddock_id, [f.lon, f.lat]), f.t));
    })
    .await?;
    for ((day, h, _collar, paddock), agg) in dwell.finish(now.timestamp_millis()) {
        let e = days.entry((day, h, paddock)).or_default();
        e.0 += agg.dwell_ms as f64;
        e.1 = e.1.max(agg.last_t);
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
    let now_ms = now.timestamp_millis();
    Ok(paddock_list
        .into_iter()
        .map(|p| {
            let a = per.remove(&p.id);
            let last = a.as_ref().and_then(|a| a.last);
            let au_days = any_in_window.then(|| a.as_ref().map_or(0.0, |a| a.au_days));
            PaddockPasture {
                grazing_days: any_in_window.then(|| a.as_ref().map_or(0, |a| a.grazing.len() as i64)),
                rest_days: last.map(|l| round(((now_ms - l).max(0)) as f64 / DAY_MS as f64, 1)),
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
