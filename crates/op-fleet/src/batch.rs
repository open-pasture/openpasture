//! Batch views over the test runs: yield per plan step, failures by step,
//! and throughput per station. What building 1,000 collars alone needs to
//! see: which step is failing, on which station, and how fast it goes.

use std::collections::{BTreeMap, HashMap, HashSet};

use axum::extract::{Query, State};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Duration, DurationRound, Utc};
use op_core::time::{now, to_db};
use op_core::{ApiError, ApiResult, Ctx};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;

use crate::plans::StepKind;
use crate::runs::{JudgedStep, Verdict};

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/fleet/yield", get(yield_view)).route("/api/fleet/throughput", get(throughput))
}

/// Most runs one view reads.
const MAX_RUNS: i64 = 50_000;

#[derive(Debug, Serialize, Default, PartialEq)]
pub struct Rate {
    pub units: i64,
    pub passed: i64,
    pub rate: f64,
}

impl Rate {
    fn of(units: i64, passed: i64) -> Self {
        Self { units, passed, rate: if units > 0 { passed as f64 / units as f64 } else { 0.0 } }
    }
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Values {
    pub n: usize,
    pub min: f64,
    pub p50: f64,
    pub max: f64,
    pub mean: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct StepYield {
    pub name: String,
    pub required: bool,
    pub runs: i64,
    pub pass: i64,
    pub fail: i64,
    pub skip: i64,
    pub missing: i64,
    pub rate: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub values: Option<Values>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Failure {
    pub name: String,
    pub fail: i64,
}

/// Runs judged by another version of the plan: counted apart, never mixed in.
#[derive(Debug, Serialize, PartialEq)]
pub struct OtherVersion {
    pub plan_version: i64,
    pub runs: i64,
}

/// Yield for one version of a plan: its steps, limits and units, and only
/// the runs judged by it. Runs of other versions are listed apart.
#[derive(Debug, Serialize, PartialEq)]
pub struct Yield {
    pub plan_id: String,
    pub plan_version: i64,
    pub runs: i64,
    pub units: i64,
    pub first_pass: Rate,
    #[serde(rename = "final")]
    pub last: Rate,
    pub steps: Vec<StepYield>,
    pub failures: Vec<Failure>,
    /// Runs in the same window and filters judged by other versions.
    pub other_versions: Vec<OtherVersion>,
}

/// One run as the batch views read it.
#[derive(Debug, Clone)]
pub struct RunRow {
    /// The server's order.
    pub seq: i64,
    pub plan_version: i64,
    pub serial: String,
    pub station: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub pass: bool,
    pub failed: Vec<String>,
    pub steps: Vec<JudgedStep>,
}

fn median(sorted: &[f64]) -> f64 {
    let n = sorted.len();
    if n == 0 {
        return 0.0;
    }
    if n % 2 == 1 { sorted[n / 2] } else { (sorted[n / 2 - 1] + sorted[n / 2]) / 2.0 }
}

/// Yield from runs (any order) of one plan version, by its steps in order.
/// Runs of other versions are left out of every number and counted in
/// `other_versions`: a step's limits and unit belong to its version.
pub fn compute_yield(plan_id: &str, plan_version: i64, plan_steps: &[crate::plans::PlanStep], all: &[RunRow]) -> Yield {
    let mut others: BTreeMap<i64, i64> = BTreeMap::new();
    for r in all.iter().filter(|r| r.plan_version != plan_version) {
        *others.entry(r.plan_version).or_default() += 1;
    }
    let runs: Vec<&RunRow> = all.iter().filter(|r| r.plan_version == plan_version).collect();
    let mut by_unit: HashMap<&str, Vec<&RunRow>> = HashMap::new();
    for r in &runs {
        by_unit.entry(r.serial.as_str()).or_default().push(r);
    }
    let (mut first_ok, mut last_ok) = (0, 0);
    for list in by_unit.values_mut() {
        list.sort_by_key(|r| r.seq);
        first_ok += list.first().is_some_and(|r| r.pass) as i64;
        last_ok += list.last().is_some_and(|r| r.pass) as i64;
    }
    let units = by_unit.len() as i64;
    let steps = plan_steps
        .iter()
        .map(|p| {
            let (mut pass, mut fail, mut skip, mut missing) = (0, 0, 0, 0);
            let mut vals: Vec<f64> = Vec::new();
            for r in &runs {
                match r.steps.iter().find(|s| s.name == p.name).map(|s| (s.verdict, s.value.as_ref().and_then(Value::as_f64))) {
                    Some((v, value)) => {
                        match v {
                            Verdict::Pass => pass += 1,
                            Verdict::Fail => fail += 1,
                            Verdict::Skip => skip += 1,
                            Verdict::Missing | Verdict::Extra => missing += 1,
                        }
                        if p.kind == StepKind::Measure
                            && let Some(x) = value
                        {
                            vals.push(x);
                        }
                    }
                    None => missing += 1,
                }
            }
            vals.sort_by(|a, b| a.total_cmp(b));
            let values = (!vals.is_empty()).then(|| Values {
                n: vals.len(),
                min: vals[0],
                p50: median(&vals),
                max: vals[vals.len() - 1],
                mean: vals.iter().sum::<f64>() / vals.len() as f64,
                unit: p.unit.clone(),
            });
            let n = runs.len() as i64;
            StepYield {
                name: p.name.clone(),
                required: p.required,
                runs: n,
                pass,
                fail,
                skip,
                missing,
                rate: if n > 0 { pass as f64 / n as f64 } else { 0.0 },
                values,
            }
        })
        .collect();
    let mut fails: BTreeMap<&str, i64> = BTreeMap::new();
    for r in &runs {
        for f in &r.failed {
            *fails.entry(f.as_str()).or_default() += 1;
        }
    }
    let mut failures: Vec<Failure> = fails.into_iter().map(|(name, fail)| Failure { name: name.to_owned(), fail }).collect();
    failures.sort_by(|a, b| b.fail.cmp(&a.fail).then_with(|| a.name.cmp(&b.name)));
    Yield {
        plan_id: plan_id.to_owned(),
        plan_version,
        runs: runs.len() as i64,
        units,
        first_pass: Rate::of(units, first_ok),
        last: Rate::of(units, last_ok),
        steps,
        failures,
        other_versions: others.into_iter().map(|(plan_version, runs)| OtherVersion { plan_version, runs }).collect(),
    }
}

fn run_from_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<RunRow> {
    let ts = |c: &str| -> anyhow::Result<DateTime<Utc>> { op_core::time::from_db(&r.try_get::<String, _>(c)?) };
    Ok(RunRow {
        seq: r.try_get("seq")?,
        plan_version: r.try_get("plan_version")?,
        serial: r.try_get("serial")?,
        station: r.try_get("station")?,
        started_at: ts("started_at")?,
        finished_at: ts("finished_at")?,
        received_at: ts("received_at")?,
        pass: r.try_get::<String, _>("result")? == "pass",
        failed: serde_json::from_str(&r.try_get::<String, _>("failed")?)?,
        steps: serde_json::from_str(&r.try_get::<String, _>("steps")?)?,
    })
}

#[derive(Deserialize)]
struct YieldQuery {
    plan_id: Option<String>,
    plan_version: Option<i64>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    station: Option<String>,
}

async fn yield_view(State(ctx): State<Ctx>, Query(q): Query<YieldQuery>) -> ApiResult<Json<Yield>> {
    let plan_id = q.plan_id.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| ApiError::bad_request("Which plan? Add plan_id."))?;
    let version = match q.plan_version {
        Some(v) => v,
        None => sqlx::query_scalar::<_, i64>("SELECT MAX(version) FROM fleet_test_plans WHERE id = ?")
            .bind(plan_id)
            .fetch_optional(ctx.db())
            .await?
            .ok_or_else(|| ApiError::not_found(format!("No test plan {plan_id}.")))?,
    };
    let plan = crate::plans::by_id(&ctx, plan_id, version).await?.ok_or_else(|| ApiError::not_found(format!("No test plan {plan_id} version {version}.")))?;
    // Every version's runs are read so the others can be counted apart.
    let mut sql = String::from("SELECT * FROM fleet_test_runs WHERE plan_id = ?");
    if q.from.is_some() {
        sql.push_str(" AND finished_at >= ?");
    }
    if q.to.is_some() {
        sql.push_str(" AND finished_at < ?");
    }
    if q.station.as_deref().is_some_and(|s| !s.is_empty()) {
        sql.push_str(" AND station = ?");
    }
    sql.push_str(&format!(" ORDER BY seq LIMIT {MAX_RUNS}"));
    let mut query = sqlx::query(&sql).bind(plan_id);
    if let Some(f) = q.from {
        query = query.bind(to_db(&f));
    }
    if let Some(t) = q.to {
        query = query.bind(to_db(&t));
    }
    if let Some(s) = q.station.as_deref().filter(|s| !s.is_empty()) {
        query = query.bind(s);
    }
    let runs: Vec<RunRow> = query.fetch_all(ctx.db()).await?.iter().map(run_from_row).collect::<anyhow::Result<_>>()?;
    Ok(Json(compute_yield(&plan.id, plan.version, &plan.steps, &runs)))
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Bucket {
    pub at: DateTime<Utc>,
    pub runs: i64,
    pub pass: i64,
    pub fail: i64,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct StationThroughput {
    pub station: String,
    pub runs: i64,
    pub units: i64,
    pub pass: i64,
    pub fail: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cycle_s_p50: Option<f64>,
    pub buckets: Vec<Bucket>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct Throughput {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    pub bucket: String,
    pub stations: Vec<StationThroughput>,
}

/// Runs per station, bucketed by finish time (`hour` or `day`, UTC).
pub fn compute_throughput(from: DateTime<Utc>, to: DateTime<Utc>, bucket: &str, runs: &[RunRow]) -> Throughput {
    let step = if bucket == "hour" { Duration::hours(1) } else { Duration::days(1) };
    let mut by: BTreeMap<String, Vec<&RunRow>> = BTreeMap::new();
    for r in runs {
        by.entry(r.station.clone().unwrap_or_else(|| "(no station)".into())).or_default().push(r);
    }
    let stations = by
        .into_iter()
        .map(|(station, list)| {
            let units: HashSet<&str> = list.iter().map(|r| r.serial.as_str()).collect();
            let pass = list.iter().filter(|r| r.pass).count() as i64;
            let mut cycles: Vec<f64> = list.iter().map(|r| (r.finished_at - r.started_at).num_milliseconds() as f64 / 1000.0).collect();
            cycles.sort_by(|a, b| a.total_cmp(b));
            let mut buckets: BTreeMap<DateTime<Utc>, (i64, i64)> = BTreeMap::new();
            for r in &list {
                let at = r.finished_at.duration_trunc(step).unwrap_or(r.finished_at);
                let b = buckets.entry(at).or_default();
                if r.pass {
                    b.0 += 1;
                } else {
                    b.1 += 1;
                }
            }
            StationThroughput {
                station,
                runs: list.len() as i64,
                units: units.len() as i64,
                pass,
                fail: list.len() as i64 - pass,
                cycle_s_p50: (!cycles.is_empty()).then(|| median(&cycles)),
                buckets: buckets.into_iter().map(|(at, (p, f))| Bucket { at, runs: p + f, pass: p, fail: f }).collect(),
            }
        })
        .collect();
    Throughput { from, to, bucket: if bucket == "hour" { "hour".into() } else { "day".into() }, stations }
}

#[derive(Deserialize)]
struct ThroughputQuery {
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
    bucket: Option<String>,
}

async fn throughput(State(ctx): State<Ctx>, Query(q): Query<ThroughputQuery>) -> ApiResult<Json<Throughput>> {
    let to = q.to.unwrap_or_else(now);
    let from = q.from.unwrap_or(to - Duration::days(7));
    if from >= to {
        return Err(ApiError::bad_request("from is after to."));
    }
    let bucket = q.bucket.as_deref().unwrap_or("day");
    if bucket != "hour" && bucket != "day" {
        return Err(ApiError::bad_request("bucket is hour or day."));
    }
    if bucket == "hour" && to - from > Duration::days(31) {
        return Err(ApiError::bad_request("Hourly buckets cover at most 31 days."));
    }
    let rows = sqlx::query(&format!("SELECT * FROM fleet_test_runs WHERE finished_at >= ? AND finished_at < ? ORDER BY finished_at LIMIT {MAX_RUNS}"))
        .bind(to_db(&from))
        .bind(to_db(&to))
        .fetch_all(ctx.db())
        .await?;
    let runs: Vec<RunRow> = rows.iter().map(run_from_row).collect::<anyhow::Result<_>>()?;
    Ok(Json(compute_throughput(from, to, bucket, &runs)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::{PlanStep, StepKind};
    use chrono::TimeZone;

    fn plan() -> Vec<PlanStep> {
        vec![
            PlanStep {
                name: "rail".into(),
                kind: StepKind::Measure,
                limit_lo: Some(3.2),
                limit_hi: Some(3.4),
                unit: Some("V".into()),
                required: true,
                note: None,
            },
            PlanStep { name: "imu".into(), kind: StepKind::Check, limit_lo: None, limit_hi: None, unit: None, required: true, note: None },
        ]
    }

    fn judged(name: &str, verdict: Verdict, value: Option<f64>) -> JudgedStep {
        JudgedStep {
            name: name.into(),
            status: None,
            value: value.map(Value::from),
            unit: None,
            detail: None,
            verdict,
            why: None,
            kind: None,
            limit_lo: None,
            limit_hi: None,
            plan_unit: None,
            required: true,
        }
    }

    fn run(serial: &str, minute: i64, rail: f64, imu: Verdict, station: &str) -> RunRow {
        let t = Utc.with_ymd_and_hms(2026, 10, 26, 14, 0, 0).unwrap() + Duration::minutes(minute);
        let rail_ok = (3.2..=3.4).contains(&rail);
        let mut failed = Vec::new();
        if !rail_ok {
            failed.push("rail".to_string());
        }
        if imu != Verdict::Pass {
            failed.push("imu".to_string());
        }
        RunRow {
            seq: minute,
            plan_version: 1,
            serial: serial.into(),
            station: Some(station.into()),
            started_at: t - Duration::seconds(50),
            finished_at: t,
            received_at: t,
            pass: failed.is_empty(),
            failed,
            steps: vec![judged("rail", if rail_ok { Verdict::Pass } else { Verdict::Fail }, Some(rail)), judged("imu", imu, None)],
        }
    }

    #[test]
    fn first_pass_and_final_yield_with_failures_by_step() {
        let runs = vec![
            run("A", 0, 3.3, Verdict::Pass, "s1"),
            run("B", 1, 2.9, Verdict::Pass, "s1"),
            run("B", 5, 3.31, Verdict::Pass, "s1"),
            run("C", 2, 3.3, Verdict::Fail, "s2"),
        ];
        let y = compute_yield("node-fct", 1, &plan(), &runs);
        assert_eq!((y.runs, y.units), (4, 3));
        assert_eq!(y.first_pass, Rate::of(3, 1));
        assert_eq!(y.last, Rate::of(3, 2), "B passed on its second try");
        let rail = &y.steps[0];
        assert_eq!((rail.pass, rail.fail, rail.missing), (3, 1, 0));
        let v = rail.values.as_ref().unwrap();
        assert_eq!((v.n, v.min, v.max), (4, 2.9, 3.31));
        assert_eq!(v.p50, 3.3);
        assert_eq!(y.failures.iter().map(|f| (f.name.as_str(), f.fail)).collect::<Vec<_>>(), vec![("imu", 1), ("rail", 1)]);
    }

    /// Review P2-03: v1 measured `rail` in volts; v2 judges it in millivolts.
    /// The v2 yield never shows a v1 reading as if it were in mV.
    #[test]
    fn yield_is_per_plan_version() {
        let mut old = run("A", 0, 3.3, Verdict::Pass, "s1");
        old.plan_version = 1;
        let mut v2 = plan();
        v2[0].limit_lo = Some(3100.0);
        v2[0].limit_hi = Some(3500.0);
        v2[0].unit = Some("mV".into());
        let y = compute_yield("node-fct", 2, &v2, std::slice::from_ref(&old));
        assert_eq!((y.plan_version, y.runs, y.units), (2, 0, 0));
        assert_eq!(y.steps[0].values, None, "no v1 volts shown as mV");
        assert_eq!(y.other_versions, vec![OtherVersion { plan_version: 1, runs: 1 }]);
        let mut new = run("B", 3, 3.3, Verdict::Pass, "s1");
        new.plan_version = 2;
        new.steps[0].value = Some(Value::from(3301.0));
        let y = compute_yield("node-fct", 2, &v2, &[old, new]);
        let v = y.steps[0].values.as_ref().unwrap();
        assert_eq!((y.runs, v.n, v.min, v.unit.as_deref()), (1, 1, 3301.0, Some("mV")));
    }

    #[test]
    fn throughput_per_station_and_hour() {
        let runs = vec![run("A", 0, 3.3, Verdict::Pass, "s1"), run("B", 30, 2.0, Verdict::Pass, "s1"), run("C", 70, 3.3, Verdict::Pass, "s2")];
        let from = Utc.with_ymd_and_hms(2026, 10, 26, 0, 0, 0).unwrap();
        let t = compute_throughput(from, from + Duration::days(1), "hour", &runs);
        assert_eq!(t.stations.len(), 2);
        let s1 = &t.stations[0];
        assert_eq!((s1.station.as_str(), s1.runs, s1.units, s1.pass, s1.fail), ("s1", 2, 2, 1, 1));
        assert_eq!(s1.cycle_s_p50, Some(50.0));
        assert_eq!(s1.buckets.len(), 1, "both in the 14:00 hour");
        assert_eq!(t.stations[1].buckets[0].at, Utc.with_ymd_and_hms(2026, 10, 26, 15, 0, 0).unwrap());
    }
}
