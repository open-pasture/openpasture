//! Test plans: the steps a fixture runs on each unit of a class, with the
//! limits the server judges them by. Versioned: editing adds a version, and
//! a run is judged by the version it names. This server ships plans for
//! `node` and `anchor`, from the end-of-line test in
//! `opencollar/docs/LORA-V1-PLAN.md` and `TEST-FIXTURE.md`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::time::{now, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Identity};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::units::check_class;
use crate::who;

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/fleet/test-plans", get(list)).route("/api/fleet/test-plans/{class}", get(current).put(edit))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepKind {
    /// A number the server checks against the limits.
    Measure,
    /// Pass or fail, as the fixture says.
    Check,
    /// Kept (an IMEI, a firmware hash); never fails.
    Record,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanStep {
    pub name: String,
    pub kind: StepKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_lo: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit_hi: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TestPlan {
    pub id: String,
    pub version: i64,
    pub device_class: String,
    pub steps: Vec<PlanStep>,
    pub shipped: bool,
    pub created_at: DateTime<Utc>,
}

pub const MAX_STEPS: usize = 64;

fn step(name: &str, kind: StepKind, lo: Option<f64>, hi: Option<f64>, unit: Option<&str>, required: bool, note: &str) -> PlanStep {
    PlanStep {
        name: name.into(),
        kind,
        limit_lo: lo,
        limit_hi: hi,
        unit: unit.map(Into::into),
        required,
        note: (!note.is_empty()).then(|| note.into()),
    }
}

/// The node's end-of-line test: flash, rails, sleep current, charger, data
/// flash, IMU, hall wake, both buzzers, GNSS, LoRa both ways, BLE.
pub fn node_steps() -> Vec<PlanStep> {
    use StepKind::*;
    vec![
        step("fw_flash", Check, None, None, None, true, "Firmware written over SWD and read back"),
        step("rail_vsys", Measure, Some(3.0), Some(3.6), Some("V"), true, "Charger output, battery stand-in at 3.3 V"),
        step("rail_3v3", Measure, Some(3.2), Some(3.4), Some("V"), true, ""),
        step("sleep_current", Measure, Some(0.0), Some(15.0), Some("uA"), true, "Whole board asleep; 5 uA is the design figure"),
        step("charger", Check, None, None, None, true, "Charges from the solar stand-in"),
        step("flash_ext", Check, None, None, None, true, "16 MB data flash: write, read, erase"),
        step("imu", Check, None, None, None, true, "Answers, and reads about 1 g at rest"),
        step("hall_wake", Check, None, None, None, true, "Wakes on the magnet"),
        step("buzzer_left", Check, None, None, None, true, "Heard by the fixture microphone"),
        step("buzzer_right", Check, None, None, None, true, "Heard by the fixture microphone"),
        step("gnss_fix", Check, None, None, None, true, "3D fix from the re-radiator"),
        step("gnss_ttff", Measure, Some(0.0), Some(60.0), Some("s"), true, "Cold start"),
        step("gnss_cn0", Measure, Some(30.0), Some(55.0), Some("dBHz"), true, "Mean of the four strongest satellites"),
        step("lora_tx_rssi", Measure, Some(-70.0), Some(-20.0), Some("dBm"), true, "Unit to the station's reference radio; set for your fixture"),
        step("lora_rx_rssi", Measure, Some(-70.0), Some(-20.0), Some("dBm"), true, "Reference radio to the unit; set for your fixture"),
        step("ble", Check, None, None, None, false, "Advertises for phone setup"),
    ]
}

/// The anchor's: the node's, plus the fuel gauge, LTE attach and signal, and
/// the SIM.
pub fn anchor_steps() -> Vec<PlanStep> {
    use StepKind::*;
    let mut steps = node_steps();
    steps.extend([
        step("fuel_gauge", Check, None, None, None, true, "Reads the pack"),
        step("lte_attach", Check, None, None, None, true, "Attaches to the network with the test SIM"),
        step("lte_rsrp", Measure, Some(-115.0), Some(-44.0), Some("dBm"), true, ""),
        step("sim_iccid", Record, None, None, None, false, ""),
    ]);
    steps
}

/// The plans this server ships: (id, class, steps).
pub fn shipped() -> [(&'static str, &'static str, Vec<PlanStep>); 2] {
    [("node-fct", "node", node_steps()), ("anchor-fct", "anchor", anchor_steps())]
}

/// Store the shipped plans as version 1 the first time.
pub async fn ensure_shipped(ctx: &Ctx) -> anyhow::Result<()> {
    for (id, class, steps) in shipped() {
        sqlx::query("INSERT OR IGNORE INTO fleet_test_plans (id, version, device_class, steps, shipped, created_at) VALUES (?, 1, ?, ?, 1, ?)")
            .bind(id)
            .bind(class)
            .bind(serde_json::to_string(&steps)?)
            .bind(to_db(&now()))
            .execute(ctx.db())
            .await?;
    }
    Ok(())
}

fn from_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<TestPlan> {
    Ok(TestPlan {
        id: r.try_get("id")?,
        version: r.try_get("version")?,
        device_class: r.try_get("device_class")?,
        steps: serde_json::from_str(&r.try_get::<String, _>("steps")?)?,
        shipped: r.try_get::<i64, _>("shipped")? != 0,
        created_at: op_core::time::from_db(&r.try_get::<String, _>("created_at")?)?,
    })
}

/// The class's current plan, or one version of it.
pub async fn for_class(ctx: &Ctx, class: &str, version: Option<i64>) -> anyhow::Result<Option<TestPlan>> {
    ensure_shipped(ctx).await?;
    let row = match version {
        Some(v) => sqlx::query("SELECT * FROM fleet_test_plans WHERE device_class = ? AND version = ?").bind(class).bind(v).fetch_optional(ctx.db()).await?,
        None => sqlx::query("SELECT * FROM fleet_test_plans WHERE device_class = ? ORDER BY version DESC LIMIT 1").bind(class).fetch_optional(ctx.db()).await?,
    };
    row.map(|r| from_row(&r)).transpose()
}

/// One version of a plan by id.
pub async fn by_id(ctx: &Ctx, id: &str, version: i64) -> anyhow::Result<Option<TestPlan>> {
    ensure_shipped(ctx).await?;
    let row = sqlx::query("SELECT * FROM fleet_test_plans WHERE id = ? AND version = ?").bind(id).bind(version).fetch_optional(ctx.db()).await?;
    row.map(|r| from_row(&r)).transpose()
}

async fn list(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<TestPlan>>> {
    ensure_shipped(&ctx).await?;
    let rows = sqlx::query(
        "SELECT p.* FROM fleet_test_plans p
         WHERE p.version = (SELECT MAX(version) FROM fleet_test_plans q WHERE q.device_class = p.device_class)
         ORDER BY p.device_class",
    )
    .fetch_all(ctx.db())
    .await?;
    Ok(Json(rows.iter().map(from_row).collect::<anyhow::Result<_>>()?))
}

#[derive(Deserialize)]
struct VersionQuery {
    version: Option<i64>,
}

async fn current(State(ctx): State<Ctx>, Path(class): Path<String>, Query(q): Query<VersionQuery>) -> ApiResult<Json<TestPlan>> {
    check_class(&class)?;
    match for_class(&ctx, &class, q.version).await? {
        Some(p) => Ok(Json(p)),
        None if q.version.is_some() => Err(ApiError::not_found(format!("No version {} of the {class} test plan.", q.version.unwrap_or_default()))),
        None => Err(ApiError::not_found(format!("No test plan for {class} units yet. The owner can add one under Fleet, Test plans."))),
    }
}

/// Step names, kinds and limits make sense: unique lowercase names, limits
/// only on measures and in order, at least one required step.
pub fn check_steps(steps: &[PlanStep]) -> Result<(), String> {
    if steps.is_empty() || steps.len() > MAX_STEPS {
        return Err(format!("A plan has 1–{MAX_STEPS} steps."));
    }
    let mut seen = std::collections::HashSet::new();
    for s in steps {
        if s.name.is_empty() || s.name.len() > 64 || !s.name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
            return Err(format!("Step {:?}: names are lowercase letters, digits and _.", s.name));
        }
        if !seen.insert(s.name.as_str()) {
            return Err(format!("Step {} is in the plan twice.", s.name));
        }
        let limits = s.limit_lo.is_some() || s.limit_hi.is_some();
        if s.kind != StepKind::Measure && limits {
            return Err(format!("Step {}: only measure steps have limits.", s.name));
        }
        if s.kind == StepKind::Measure && !limits {
            return Err(format!("Step {}: a measure step needs a low or high limit.", s.name));
        }
        if [s.limit_lo, s.limit_hi].iter().flatten().any(|v| !v.is_finite()) {
            return Err(format!("Step {}: limits must be numbers.", s.name));
        }
        if let (Some(lo), Some(hi)) = (s.limit_lo, s.limit_hi)
            && lo > hi
        {
            return Err(format!("Step {}: the low limit is above the high one.", s.name));
        }
        if s.unit.as_deref().is_some_and(|u| u.is_empty() || u.len() > 16) || s.note.as_deref().is_some_and(|n| n.len() > 200) {
            return Err(format!("Step {}: units are 1–16 characters, notes up to 200.", s.name));
        }
    }
    if !steps.iter().any(|s| s.required) {
        return Err("At least one step must be required.".into());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditPlan {
    steps: Vec<PlanStep>,
}

/// `PUT /api/fleet/test-plans/:class` (owner): a new version of the class's
/// plan, or its first. The id stays the class's (`<class>-fct`).
async fn edit(State(ctx): State<Ctx>, identity: Identity, Path(class): Path<String>, ApiJson(b): ApiJson<EditPlan>) -> ApiResult<(StatusCode, Json<TestPlan>)> {
    check_class(&class)?;
    check_steps(&b.steps).map_err(ApiError::bad_request)?;
    let current = for_class(&ctx, &class, None).await?;
    let (id, version) = match &current {
        Some(p) => (p.id.clone(), p.version + 1),
        None => (format!("{class}-fct"), 1),
    };
    sqlx::query("INSERT INTO fleet_test_plans (id, version, device_class, steps, shipped, created_at, created_by) VALUES (?, ?, ?, ?, 0, ?, ?)")
        .bind(&id)
        .bind(version)
        .bind(&class)
        .bind(serde_json::to_string(&b.steps).map_err(anyhow::Error::from)?)
        .bind(to_db(&now()))
        .bind(who(&identity))
        .execute(ctx.db())
        .await?;
    tracing::info!(plan = %id, version, "test plan edited");
    let plan = by_id(&ctx, &id, version).await?.ok_or_else(|| ApiError::internal("plan vanished"))?;
    Ok((StatusCode::CREATED, Json(plan)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_plans_are_valid_and_the_anchor_adds_lte() {
        for (_, _, steps) in shipped() {
            check_steps(&steps).unwrap();
        }
        let names: Vec<String> = anchor_steps().into_iter().map(|s| s.name).collect();
        for n in ["fw_flash", "sleep_current", "gnss_cn0", "lora_tx_rssi", "buzzer_left", "hall_wake", "lte_attach", "lte_rsrp"] {
            assert!(names.iter().any(|x| x == n), "{n}");
        }
        assert!(!node_steps().iter().any(|s| s.name.starts_with("lte")));
    }

    #[test]
    fn bad_plans_say_why() {
        let ok = || step("rail", StepKind::Measure, Some(1.0), Some(2.0), Some("V"), true, "");
        let cases: Vec<(Vec<PlanStep>, &str)> = vec![
            (vec![], "1–64"),
            (vec![ok(), ok()], "twice"),
            (vec![PlanStep { name: "Rail 1".into(), ..ok() }], "lowercase"),
            (vec![PlanStep { kind: StepKind::Check, ..ok() }], "only measure"),
            (vec![PlanStep { limit_lo: None, limit_hi: None, ..ok() }], "needs a low or high"),
            (vec![PlanStep { limit_lo: Some(3.0), ..ok() }], "above the high"),
            (vec![PlanStep { required: false, ..ok() }], "required"),
        ];
        for (steps, want) in cases {
            let e = check_steps(&steps).unwrap_err();
            assert!(e.contains(want), "{e} / {want}");
        }
    }
}
