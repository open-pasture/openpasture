//! Test runs: a station posts what each step of a plan measured, and the
//! server judges the run against that plan version. For a `measure` step the
//! server's verdict wins over the station's; a required step that is missing
//! or skipped fails the run.
//!
//! Which run counts: the latest by the server's own order (`seq`, given in
//! the write that stores it) among the runs of the unit's current
//! generation. A retry with the station's `run_key` is the same run. A pass
//! that finished before a run already stored, or before the unit's
//! generation began (it was sent back or retired since), is kept but not
//! counted: a delayed retry can't overrule a newer failure, and a repaired
//! unit needs a fresh pass. A failure always counts.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use op_core::time::{now, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Event, Identity, id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

use crate::plans::{PlanStep, StepKind, TestPlan};
use crate::units::{self, UnitState};
use crate::{stations, who};

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/fleet/units/{serial}/tests", post(create))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepStatus {
    Pass,
    Fail,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    Pass,
    Fail,
    Skip,
    Missing,
    /// Not in the plan; kept, doesn't count.
    Extra,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StepResult {
    pub name: String,
    pub status: StepStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JudgedStep {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StepStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    pub verdict: Verdict,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<StepKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_lo: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_hi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_unit: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RunResult {
    Pass,
    Fail,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestRun {
    pub id: String,
    pub serial: String,
    pub plan_id: String,
    pub plan_version: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub station: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fixture_id: Option<String>,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub received_at: DateTime<Utc>,
    pub result: RunResult,
    pub failed: Vec<String>,
    pub steps: Vec<JudgedStep>,
    /// The server's order: higher is later, whatever the stations' clocks say.
    pub seq: i64,
    /// The unit's generation the run belongs to.
    pub generation: i64,
    /// Whether the run can decide the unit's eligibility (see the module docs).
    pub counted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_key: Option<String>,
}

fn fmt_num(v: f64) -> String {
    let s = format!("{v:.3}");
    let s = s.trim_end_matches('0').trim_end_matches('.');
    if s.is_empty() || s == "-0" { "0".into() } else { s.to_owned() }
}

fn with_unit(v: f64, unit: Option<&str>) -> String {
    match unit {
        Some(u) => format!("{} {u}", fmt_num(v)),
        None => fmt_num(v),
    }
}

/// One plan step against what the station sent for it.
pub fn judge_step(plan: &PlanStep, got: Option<&StepResult>) -> JudgedStep {
    let mut j = JudgedStep {
        name: plan.name.clone(),
        status: got.map(|g| g.status),
        value: got.and_then(|g| g.value.clone()),
        unit: got.and_then(|g| g.unit.clone()),
        detail: got.and_then(|g| g.detail.clone()),
        verdict: Verdict::Pass,
        why: None,
        kind: Some(plan.kind),
        limit_lo: plan.limit_lo,
        limit_hi: plan.limit_hi,
        plan_unit: plan.unit.clone(),
        required: plan.required,
    };
    let Some(got) = got else {
        j.verdict = Verdict::Missing;
        j.why = Some("Not in the run".into());
        return j;
    };
    let fail = |why: String| (Verdict::Fail, Some(why));
    let (verdict, why) = match plan.kind {
        // Kept as sent; a record never fails, but a skip is still a skip.
        StepKind::Record if got.status == StepStatus::Skip => (Verdict::Skip, None),
        StepKind::Record => (Verdict::Pass, None),
        _ if got.status == StepStatus::Skip => (Verdict::Skip, Some("Skipped".into())),
        StepKind::Check if got.status == StepStatus::Fail => fail("The fixture failed it".into()),
        StepKind::Check => (Verdict::Pass, None),
        StepKind::Measure => {
            let unit = plan.unit.as_deref();
            match got.value.as_ref().and_then(Value::as_f64) {
                None => fail("No number measured".into()),
                Some(_) if unit.is_some() && got.unit.as_deref().is_some_and(|u| Some(u) != unit) => {
                    fail(format!("Measured in {}, the plan wants {}", got.unit.as_deref().unwrap_or_default(), unit.unwrap_or_default()))
                }
                Some(v) if plan.limit_lo.is_some_and(|lo| v < lo) => {
                    fail(format!("{} below {}", with_unit(v, unit), with_unit(plan.limit_lo.unwrap_or_default(), unit)))
                }
                Some(v) if plan.limit_hi.is_some_and(|hi| v > hi) => {
                    fail(format!("{} above {}", with_unit(v, unit), with_unit(plan.limit_hi.unwrap_or_default(), unit)))
                }
                // In limits, but the fixture saw something else wrong (its detail says what).
                Some(_) if got.status == StepStatus::Fail => fail("In limits, but the fixture failed it".into()),
                Some(_) => (Verdict::Pass, None),
            }
        }
    };
    j.verdict = verdict;
    j.why = why;
    j
}

/// The whole run: every plan step in plan order, then steps the plan doesn't
/// have. Passes when every required step passes.
pub fn judge(plan: &TestPlan, steps: &[StepResult]) -> (RunResult, Vec<String>, Vec<JudgedStep>) {
    let mut out: Vec<JudgedStep> = plan.steps.iter().map(|p| judge_step(p, steps.iter().rev().find(|s| s.name == p.name))).collect();
    for s in steps {
        if !plan.steps.iter().any(|p| p.name == s.name) && !out.iter().any(|j| j.name == s.name && j.verdict == Verdict::Extra) {
            out.push(JudgedStep {
                name: s.name.clone(),
                status: Some(s.status),
                value: s.value.clone(),
                unit: s.unit.clone(),
                detail: s.detail.clone(),
                verdict: Verdict::Extra,
                why: None,
                kind: None,
                limit_lo: None,
                limit_hi: None,
                plan_unit: None,
                required: false,
            });
        }
    }
    let failed: Vec<String> = out.iter().filter(|j| j.required && j.verdict != Verdict::Pass).map(|j| j.name.clone()).collect();
    let result = if failed.is_empty() { RunResult::Pass } else { RunResult::Fail };
    (result, failed, out)
}

fn from_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<TestRun> {
    let ts = |c: &str| -> anyhow::Result<DateTime<Utc>> { op_core::time::from_db(&r.try_get::<String, _>(c)?) };
    Ok(TestRun {
        id: r.try_get("id")?,
        serial: r.try_get("serial")?,
        plan_id: r.try_get("plan_id")?,
        plan_version: r.try_get("plan_version")?,
        station: r.try_get("station")?,
        fixture_id: r.try_get("fixture_id")?,
        started_at: ts("started_at")?,
        finished_at: ts("finished_at")?,
        received_at: ts("received_at")?,
        result: if r.try_get::<String, _>("result")? == "pass" { RunResult::Pass } else { RunResult::Fail },
        failed: serde_json::from_str(&r.try_get::<String, _>("failed")?)?,
        steps: serde_json::from_str(&r.try_get::<String, _>("steps")?)?,
        seq: r.try_get("seq")?,
        generation: r.try_get("generation")?,
        counted: r.try_get::<i64, _>("counted")? != 0,
        run_key: r.try_get("run_key")?,
    })
}

/// A unit's runs, newest first (by the server's order).
pub async fn for_unit(ctx: &Ctx, serial: &str) -> anyhow::Result<Vec<TestRun>> {
    let rows = sqlx::query("SELECT * FROM fleet_test_runs WHERE serial = ? ORDER BY seq DESC LIMIT 200").bind(serial).fetch_all(ctx.db()).await?;
    rows.iter().map(from_row).collect()
}

/// One run by id, on any connection (inside a transaction too).
pub async fn load(conn: &mut sqlx::SqliteConnection, id: &str) -> anyhow::Result<Option<TestRun>> {
    let row = sqlx::query("SELECT * FROM fleet_test_runs WHERE id = ?").bind(id).fetch_optional(&mut *conn).await?;
    row.map(|r| from_row(&r)).transpose()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRun {
    plan_id: String,
    plan_version: i64,
    station: Option<String>,
    fixture_id: Option<String>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    steps: Vec<StepResult>,
    /// The station's own id for this run: a retry with it is the same run.
    run_key: Option<String>,
    /// The unit's generation when the station started (from registering it).
    /// A unit sent back or retired since refuses the run.
    generation: Option<i64>,
}

const MAX_RUN_STEPS: usize = 256;

fn check_run(b: &NewRun, at: DateTime<Utc>) -> ApiResult<()> {
    if b.finished_at < b.started_at {
        return Err(ApiError::bad_request("finished_at is before started_at."));
    }
    if b.finished_at - b.started_at > Duration::hours(24) {
        return Err(ApiError::bad_request("A run longer than a day? Check the station's clock."));
    }
    if b.finished_at > at + Duration::minutes(10) {
        return Err(ApiError::bad_request("finished_at is in the future. Check the station's clock."));
    }
    if b.steps.len() > MAX_RUN_STEPS {
        return Err(ApiError::bad_request(format!("A run has at most {MAX_RUN_STEPS} steps.")));
    }
    for s in &b.steps {
        if s.name.is_empty() || s.name.len() > 64 {
            return Err(ApiError::bad_request("Step names are 1–64 characters."));
        }
        if s.detail.as_deref().is_some_and(|d| d.len() > 500) || s.unit.as_deref().is_some_and(|u| u.len() > 16) {
            return Err(ApiError::bad_request(format!("Step {}: details are up to 500 characters, units up to 16.", s.name)));
        }
        if let Some(v) = &s.value
            && !(v.is_number() || v.as_str().is_some_and(|t| t.len() <= 200) || v.is_boolean())
        {
            return Err(ApiError::bad_request(format!("Step {}: a value is a number, true/false or a short text.", s.name)));
        }
    }
    for (what, v) in [("station", &b.station), ("fixture_id", &b.fixture_id), ("run_key", &b.run_key)] {
        if v.as_deref().is_some_and(|s| s.trim().is_empty() || s.len() > 64) {
            return Err(ApiError::bad_request(format!("{what} is 1–64 characters.")));
        }
    }
    Ok(())
}

/// `POST /api/fleet/units/:serial/tests`. 201 with the judged run; 200 with
/// the stored run when the station sent this `run_key` before.
async fn create(State(ctx): State<Ctx>, identity: Identity, Path(serial): Path<String>, ApiJson(b): ApiJson<NewRun>) -> ApiResult<(StatusCode, Json<TestRun>)> {
    check_run(&b, now())?;
    let plan = crate::plans::by_id(&ctx, &b.plan_id, b.plan_version)
        .await?
        .ok_or_else(|| ApiError::bad_request(format!("No test plan {} version {}.", b.plan_id, b.plan_version)))?;
    let (result, failed, steps) = judge(&plan, &b.steps);
    let station = b.station.clone().or_else(|| stations::station_name(&identity));
    let run_key = b.run_key.as_deref().map(str::trim).map(str::to_owned);

    // Everything the decision reads is read in the write that stores it.
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let at = now();
    let unit = units::load(&mut tx, &serial).await?.ok_or_else(|| ApiError::not_found(format!("No unit with serial {serial}.")))?;
    if let Some(key) = &run_key {
        let id: Option<String> =
            sqlx::query_scalar("SELECT id FROM fleet_test_runs WHERE serial = ? AND run_key = ?").bind(&serial).bind(key).fetch_optional(&mut *tx).await?;
        if let Some(id) = id {
            let run = load(&mut tx, &id).await?.ok_or_else(|| ApiError::internal("run vanished"))?;
            if run.plan_id != plan.id || run.plan_version != plan.version || to_db(&run.finished_at) != to_db(&b.finished_at) {
                return Err(ApiError::conflict(format!("run_key {key} is already another run of {serial}. Each run needs its own.")));
            }
            return Ok((StatusCode::OK, Json(run)));
        }
    }
    if unit.state.on_farm() || unit.state == UnitState::Retired {
        return Err(ApiError::conflict(format!("{serial} is {}. Mark it RMA before testing it again.", state_words(unit.state))));
    }
    if b.generation.is_some_and(|g| g != unit.generation) {
        return Err(ApiError::conflict(format!("{serial} was sent back or retired after this run started. Test it again.")));
    }
    if plan.device_class != unit.device_class {
        return Err(ApiError::bad_request(format!("{} is a plan for {} units; {serial} is a {}.", plan.id, plan.device_class, unit.device_class)));
    }
    // A pass can't overrule a run the station finished later, nor count for a
    // generation that began after it finished.
    let later: Option<String> = sqlx::query_scalar("SELECT id FROM fleet_test_runs WHERE serial = ? AND generation = ? AND finished_at > ? LIMIT 1")
        .bind(&serial)
        .bind(unit.generation)
        .bind(to_db(&b.finished_at))
        .fetch_optional(&mut *tx)
        .await?;
    let stale = unit.generation_at.is_some_and(|g| b.finished_at < g);
    let counted = result == RunResult::Fail || (later.is_none() && !stale);
    let next = match (counted, result, unit.state) {
        (true, RunResult::Pass, UnitState::Built | UnitState::Rma) => Some(UnitState::Tested),
        (true, RunResult::Fail, UnitState::Tested | UnitState::Provisioned) => Some(UnitState::Built),
        _ => None,
    };
    let run_id = id::new_id("tst");
    sqlx::query(
        "INSERT INTO fleet_test_runs (id, serial, plan_id, plan_version, station, fixture_id, started_at, finished_at, received_at, result, failed, steps, seq, run_key, generation, counted)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, (SELECT COALESCE(MAX(seq), 0) + 1 FROM fleet_test_runs), ?, ?, ?)",
    )
    .bind(&run_id)
    .bind(&serial)
    .bind(&plan.id)
    .bind(plan.version)
    .bind(&station)
    .bind(&b.fixture_id)
    .bind(to_db(&b.started_at))
    .bind(to_db(&b.finished_at))
    .bind(to_db(&at))
    .bind(if result == RunResult::Pass { "pass" } else { "fail" })
    .bind(serde_json::to_string(&failed).map_err(anyhow::Error::from)?)
    .bind(serde_json::to_string(&steps).map_err(anyhow::Error::from)?)
    .bind(&run_key)
    .bind(unit.generation)
    .bind(counted)
    .execute(&mut *tx)
    .await?;
    let state = next.unwrap_or(unit.state);
    sqlx::query("UPDATE fleet_units SET state = ?, station = COALESCE(?, station), updated_at = ? WHERE serial = ? AND state = ? AND generation = ?")
        .bind(op_core::DbEnum::as_db(&state))
        .bind(&station)
        .bind(to_db(&at))
        .bind(&serial)
        .bind(op_core::DbEnum::as_db(&unit.state))
        .bind(unit.generation)
        .execute(&mut *tx)
        .await?;
    let why_not = (!counted).then_some(if stale { "finished before the unit came back" } else { "a later run is already in" });
    units::log(
        &mut tx,
        &serial,
        "tested",
        json!({ "run_id": run_id, "result": result, "failed": failed, "plan": format!("{} v{}", plan.id, plan.version), "station": station, "not_counted": why_not }),
        who(&identity).as_deref(),
    )
    .await?;
    let run = load(&mut tx, &run_id).await?.ok_or_else(|| ApiError::internal("run vanished"))?;
    tx.commit().await?;
    tracing::info!(serial = %serial, run = %run_id, result = ?result, counted, failed = ?run.failed, "unit tested");
    ctx.publish(Event::FleetTest { run: serde_json::to_value(&run).unwrap_or(Value::Null) });
    units::publish(&ctx, &serial).await;
    Ok((StatusCode::CREATED, Json(run)))
}

fn state_words(s: UnitState) -> &'static str {
    match s {
        UnitState::Claimed => "claimed by a farm",
        UnitState::InService => "in service on a farm",
        UnitState::Retired => "retired",
        _ => "not ready",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plans::node_steps;

    fn plan() -> TestPlan {
        TestPlan { id: "node-fct".into(), version: 1, device_class: "node".into(), steps: node_steps(), shipped: true, created_at: now() }
    }

    fn sent(name: &str, status: StepStatus, value: Option<Value>, unit: Option<&str>) -> StepResult {
        StepResult { name: name.into(), status, value, unit: unit.map(Into::into), detail: None }
    }

    /// Every node step passing, measures in the middle of their limits.
    fn all_pass() -> Vec<StepResult> {
        node_steps()
            .into_iter()
            .map(|p| {
                let value = match (p.limit_lo, p.limit_hi) {
                    (Some(lo), Some(hi)) => Some(json!((lo + hi) / 2.0)),
                    (Some(lo), None) => Some(json!(lo)),
                    (None, Some(hi)) => Some(json!(hi)),
                    _ => None,
                };
                sent(&p.name, StepStatus::Pass, value, p.unit.as_deref())
            })
            .collect()
    }

    #[test]
    fn a_clean_run_passes() {
        let (result, failed, steps) = judge(&plan(), &all_pass());
        assert_eq!(result, RunResult::Pass, "{failed:?}");
        assert!(steps.iter().all(|s| s.verdict == Verdict::Pass));
    }

    #[test]
    fn the_server_judges_measures_against_the_plan() {
        let mut run = all_pass();
        let rail = run.iter_mut().find(|s| s.name == "rail_3v3").unwrap();
        rail.value = Some(json!(2.1));
        let (result, failed, steps) = judge(&plan(), &run);
        assert_eq!(result, RunResult::Fail);
        assert_eq!(failed, vec!["rail_3v3"]);
        let j = steps.iter().find(|s| s.name == "rail_3v3").unwrap();
        assert_eq!(j.status, Some(StepStatus::Pass), "the station said pass");
        assert_eq!(j.verdict, Verdict::Fail);
        assert_eq!(j.why.as_deref(), Some("2.1 V below 3.2 V"));
    }

    #[test]
    fn missing_skipped_and_wrong_unit_required_steps_fail() {
        let mut run: Vec<StepResult> = all_pass().into_iter().filter(|s| s.name != "imu").collect();
        run.iter_mut().find(|s| s.name == "hall_wake").unwrap().status = StepStatus::Skip;
        run.iter_mut().find(|s| s.name == "sleep_current").unwrap().unit = Some("mA".into());
        let (result, failed, steps) = judge(&plan(), &run);
        assert_eq!(result, RunResult::Fail);
        assert_eq!(failed, vec!["sleep_current", "imu", "hall_wake"], "plan order");
        assert_eq!(steps.iter().find(|s| s.name == "imu").unwrap().verdict, Verdict::Missing);
        assert_eq!(steps.iter().find(|s| s.name == "hall_wake").unwrap().verdict, Verdict::Skip);
        assert!(steps.iter().find(|s| s.name == "sleep_current").unwrap().why.as_deref().unwrap().contains("mA"));
    }

    #[test]
    fn optional_failures_and_extra_steps_dont_fail_the_run() {
        let mut run = all_pass();
        run.iter_mut().find(|s| s.name == "ble").unwrap().status = StepStatus::Fail;
        run.push(sent("vibration", StepStatus::Fail, Some(json!(3)), None));
        let (result, failed, steps) = judge(&plan(), &run);
        assert_eq!(result, RunResult::Pass, "{failed:?}");
        assert_eq!(steps.iter().find(|s| s.name == "ble").unwrap().verdict, Verdict::Fail);
        let extra = steps.last().unwrap();
        assert_eq!((extra.name.as_str(), extra.verdict), ("vibration", Verdict::Extra));
    }

    #[test]
    fn a_check_failed_by_the_fixture_fails_and_a_measure_needs_a_number() {
        let mut run = all_pass();
        run.iter_mut().find(|s| s.name == "charger").unwrap().status = StepStatus::Fail;
        run.iter_mut().find(|s| s.name == "gnss_ttff").unwrap().value = Some(json!("fast"));
        let (_, failed, steps) = judge(&plan(), &run);
        assert_eq!(failed, vec!["charger", "gnss_ttff"]);
        assert_eq!(steps.iter().find(|s| s.name == "gnss_ttff").unwrap().why.as_deref(), Some("No number measured"));
    }

    #[test]
    fn numbers_read_plainly() {
        assert_eq!(fmt_num(3.2), "3.2");
        assert_eq!(fmt_num(-70.0), "-70");
        assert_eq!(fmt_num(0.0004), "0");
        assert_eq!(with_unit(4.25, Some("uA")), "4.25 uA");
    }
}
