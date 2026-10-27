//! Rollouts, in the stages protocol v2 asks of a server (`server.md` §8,
//! `firmware.md` "Rollout"): one collar of a first herd; the rest of that
//! herd once it confirmed and `wave_hours` (24 h) have passed; then every
//! herd after three times that (72 h). Every minute, and whenever something
//! changes, each collar's place is worked out again: waiting for its stage,
//! held (paused, its herd held at its version, the herd being moved, an
//! escape, a low battery, no image yet), unsupported (no `firmware`
//! capability), ready, offered, confirmed, failed or rolled back. A rollback,
//! a failure, or a collar silent since installing pauses the rollout.
//!
//! The collar ingest asks [`offer_for`] what to put in a report response and
//! tells [`record_result`] what the collar said. A rollback the collar does
//! by itself also shows in its reported version going back down.

use std::sync::LazyLock;
use std::time::Duration as StdDuration;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use chrono::{DateTime, Duration, Utc};
use op_core::time::{now, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Collar, Ctx, Identity, id};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use sqlx::Row;
use tokio::sync::Notify;

use super::{Firmware, FirmwarePolicy, Mode, UnitsQuery, firmware_from_row, older, policy};
use crate::units::{self, Unit};
use crate::who;

/// The capability a collar reports when it takes firmware update commands
/// (protocol registry: `firmware`).
pub const FOTA_CAP: &str = "firmware";
pub const TICK: StdDuration = StdDuration::from_secs(60);
/// A collar not heard this long after it wrote the image has failed.
pub const SILENT_AFTER: Duration = Duration::hours(2);

pub const UNSUPPORTED: &str = "This collar can't take updates over the air yet";

static WAKE: LazyLock<Notify> = LazyLock::new(Notify::new);

/// Work the rollouts out again now rather than at the next tick.
pub fn wake() {
    WAKE.notify_one();
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitState {
    Current,
    Waiting,
    Held,
    Unsupported,
    Ready,
    Offered,
    Confirmed,
    Failed,
    RolledBack,
}

impl op_core::DbEnum for UnitState {}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UnitFirmware {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    pub collar_id: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollout_id: Option<String>,
    pub state: UnitState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum WaveKind {
    /// One collar of the first herd.
    Collar,
    /// The rest of that herd.
    Herd,
    /// Every herd.
    All,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Wave {
    pub kind: WaveKind,
    /// Hours after the rollout started before this stage may start.
    pub after_h: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    /// The first collar (stage 1), once chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
    /// The first herd (stage 2), once chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
}

/// The three stages for a policy's `wave_hours`.
pub fn waves(wave_hours: f64) -> Vec<Wave> {
    [(WaveKind::Collar, 0.0), (WaveKind::Herd, wave_hours), (WaveKind::All, wave_hours * 3.0)]
        .into_iter()
        .map(|(kind, after_h)| Wave { kind, after_h, started_at: None, collar_id: None, herd_id: None })
        .collect()
}

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Counts {
    pub targeted: i64,
    pub waiting: i64,
    pub held: i64,
    pub ready: i64,
    pub offered: i64,
    pub confirmed: i64,
    pub failed: i64,
    pub rolled_back: i64,
    pub unsupported: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Rollout {
    pub id: String,
    pub firmware_id: String,
    pub version: String,
    pub classes: Vec<String>,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused_reason: Option<String>,
    pub waves: Vec<Wave>,
    pub wave: i64,
    pub counts: Counts,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_by: Option<String>,
}

/// A stable order for choosing the first collar: a hash of its id.
pub fn bucket(collar_id: &str) -> u16 {
    let h = Sha256::digest(collar_id.as_bytes());
    u16::from_be_bytes([h[0], h[1]])
}

/// The stage a collar belongs to: the first collar, the rest of its herd, or everyone.
pub fn wave_of(collar_id: &str, herd_id: &str, waves: &[Wave]) -> i64 {
    let first = waves.first().and_then(|w| w.collar_id.as_deref());
    let herd = waves.get(1).and_then(|w| w.herd_id.as_deref());
    if first == Some(collar_id) {
        0
    } else if herd == Some(herd_id) {
        1
    } else {
        2
    }
}

// ---- reading -----------------------------------------------------------------------------------

async fn counts(ctx: &Ctx, rollout_id: &str) -> anyhow::Result<Counts> {
    let mut c = Counts::default();
    for (state, n) in sqlx::query_as::<_, (String, i64)>("SELECT state, COUNT(*) FROM fleet_rollout_units WHERE rollout_id = ? GROUP BY state").bind(rollout_id).fetch_all(ctx.db()).await? {
        c.targeted += n;
        match state.as_str() {
            "waiting" => c.waiting = n,
            "held" => c.held = n,
            "ready" => c.ready = n,
            "offered" => c.offered = n,
            "confirmed" => c.confirmed = n,
            "failed" => c.failed = n,
            "rolled_back" => c.rolled_back = n,
            "unsupported" => c.unsupported = n,
            _ => {}
        }
    }
    Ok(c)
}

async fn rollout_from_row(ctx: &Ctx, r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Rollout> {
    let id: String = r.try_get("id")?;
    Ok(Rollout {
        firmware_id: r.try_get("firmware_id")?,
        version: r.try_get("version")?,
        classes: serde_json::from_str(&r.try_get::<String, _>("classes")?)?,
        state: r.try_get("state")?,
        paused_reason: r.try_get("paused_reason")?,
        waves: serde_json::from_str(&r.try_get::<String, _>("waves")?)?,
        wave: r.try_get("wave")?,
        counts: counts(ctx, &id).await?,
        created_at: op_core::time::from_db(&r.try_get::<String, _>("created_at")?)?,
        created_by: r.try_get("created_by")?,
        id,
    })
}

const SELECT: &str = "SELECT r.*, f.version, f.classes FROM fleet_rollouts r JOIN fleet_firmware f ON f.id = r.firmware_id";

pub async fn get_rollout(ctx: &Ctx, id: &str) -> anyhow::Result<Option<Rollout>> {
    match sqlx::query(&format!("{SELECT} WHERE r.id = ?")).bind(id).fetch_optional(ctx.db()).await? {
        Some(r) => Ok(Some(rollout_from_row(ctx, &r).await?)),
        None => Ok(None),
    }
}

pub(crate) async fn list(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<Rollout>>> {
    let rows = sqlx::query(&format!("{SELECT} ORDER BY r.created_at DESC, r.id DESC LIMIT 200")).fetch_all(ctx.db()).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(rollout_from_row(&ctx, r).await?);
    }
    Ok(Json(out))
}

// ---- writing -----------------------------------------------------------------------------------

async fn start_rollout(ctx: &Ctx, fw: &Firmware, by: Option<&str>) -> anyhow::Result<String> {
    // A newer release for a class replaces any older rollout still going for it.
    let live = sqlx::query("SELECT r.id, f.version, f.classes FROM fleet_rollouts r JOIN fleet_firmware f ON f.id = r.firmware_id WHERE r.state IN ('active', 'paused')")
        .fetch_all(ctx.db())
        .await?;
    for row in &live {
        let classes: Vec<String> = serde_json::from_str(&row.try_get::<String, _>("classes")?)?;
        let version: String = row.try_get("version")?;
        if classes.iter().any(|c| fw.classes.contains(c)) && older(&version, &fw.version) {
            set_state(ctx, &row.try_get::<String, _>("id")?, "cancelled", Some(&format!("Replaced by {}", fw.version))).await?;
        }
    }
    let id = id::new_id("rol");
    let at = now();
    let p = policy(ctx).await?;
    let mut waves = waves(p.wave_hours);
    waves[0].started_at = Some(at);
    sqlx::query("INSERT INTO fleet_rollouts (id, firmware_id, state, waves, wave, created_at, created_by, updated_at) VALUES (?, ?, 'active', ?, 0, ?, ?, ?)")
        .bind(&id)
        .bind(&fw.id)
        .bind(serde_json::to_string(&waves)?)
        .bind(to_db(&at))
        .bind(by)
        .bind(to_db(&at))
        .execute(ctx.db())
        .await?;
    tracing::info!(rollout = %id, firmware = %fw.id, version = %fw.version, "firmware rollout started");
    Ok(id)
}

async fn set_state(ctx: &Ctx, id: &str, state: &str, reason: Option<&str>) -> anyhow::Result<()> {
    sqlx::query("UPDATE fleet_rollouts SET state = ?, paused_reason = ?, updated_at = ? WHERE id = ?").bind(state).bind(reason).bind(to_db(&now())).bind(id).execute(ctx.db()).await?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewRollout {
    firmware_id: String,
}

/// `POST /api/fleet/firmware/rollouts`: start one by hand (a beta image, or
/// while the policy is paused).
pub(crate) async fn create(State(ctx): State<Ctx>, identity: Identity, ApiJson(b): ApiJson<NewRollout>) -> ApiResult<(StatusCode, Json<Rollout>)> {
    let fw = super::get_firmware(&ctx, &b.firmware_id).await?.ok_or_else(|| ApiError::not_found("No such firmware image."))?;
    if fw.withdrawn_at.is_some() {
        return Err(ApiError::conflict("That image was withdrawn."));
    }
    if super::trusted_key(&ctx, &fw.signer_kid).await?.is_none() {
        return Err(ApiError::conflict(format!("Key {} no longer signs images here.", fw.signer_kid)));
    }
    let id = start_rollout(&ctx, &fw, who(&identity).as_deref()).await?;
    tick(&ctx).await?;
    Ok((StatusCode::CREATED, Json(get_rollout(&ctx, &id).await?.ok_or_else(|| ApiError::internal("rollout vanished"))?)))
}

/// `POST /api/fleet/firmware/rollouts/:id/pause|resume|cancel`.
pub(crate) async fn act(State(ctx): State<Ctx>, identity: Identity, Path((id, action)): Path<(String, String)>) -> ApiResult<Json<Rollout>> {
    let r = get_rollout(&ctx, &id).await?.ok_or_else(|| ApiError::not_found("No such rollout."))?;
    let live = r.state == "active" || r.state == "paused";
    let by = who(&identity).unwrap_or_else(|| "someone".into());
    match action.as_str() {
        "pause" if live => set_state(&ctx, &id, "paused", Some(&format!("Paused by {by}"))).await?,
        "resume" if live => {
            sqlx::query("UPDATE fleet_rollouts SET state = 'active', paused_reason = NULL, updated_at = ? WHERE id = ?").bind(to_db(&now())).bind(&id).execute(ctx.db()).await?;
        }
        "cancel" if live => set_state(&ctx, &id, "cancelled", Some(&format!("Cancelled by {by}"))).await?,
        "pause" | "resume" | "cancel" => return Err(ApiError::conflict(format!("This rollout is {}.", r.state))),
        _ => return Err(ApiError::not_found("Rollouts can be paused, resumed or cancelled.")),
    }
    tick(&ctx).await?;
    Ok(Json(get_rollout(&ctx, &id).await?.ok_or_else(|| ApiError::internal("rollout vanished"))?))
}

pub(crate) async fn cancel_for_firmware(ctx: &Ctx, firmware_id: &str, reason: &str) -> anyhow::Result<()> {
    sqlx::query("UPDATE fleet_rollouts SET state = 'cancelled', paused_reason = ?, updated_at = ? WHERE firmware_id = ? AND state IN ('active', 'paused')")
        .bind(reason)
        .bind(to_db(&now()))
        .bind(firmware_id)
        .execute(ctx.db())
        .await?;
    Ok(())
}

pub(crate) async fn stop_for_signer(ctx: &Ctx, kid: &str) -> anyhow::Result<()> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM fleet_firmware WHERE signer_kid = ?").bind(kid).fetch_all(ctx.db()).await?;
    for id in ids {
        cancel_for_firmware(ctx, &id, "Its signing key was revoked").await?;
    }
    Ok(())
}

// ---- the tick ----------------------------------------------------------------------------------

/// Every minute (sooner on [`wake`]): auto rollouts, each collar's place,
/// waves, and units turning `in_service`.
pub fn spawn(ctx: &Ctx) {
    let ctx = ctx.clone();
    tokio::spawn(async move {
        loop {
            if let Err(e) = tick(&ctx).await {
                tracing::warn!("firmware rollout tick: {e:#}");
            }
            if let Err(e) = units::sweep_in_service(&ctx).await {
                tracing::warn!("fleet in-service sweep: {e:#}");
            }
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tokio::time::sleep(TICK) => {}
                _ = WAKE.notified() => {}
            }
        }
    });
}

/// Start a rollout for the newest release of each class the policy takes,
/// when it has none yet.
async fn auto_start(ctx: &Ctx, p: &FirmwarePolicy) -> anyhow::Result<()> {
    if p.mode != Mode::Auto {
        return Ok(());
    }
    let channels: &[&str] = if p.channel == "beta" { &["stable", "beta"] } else { &["stable"] };
    let rows = sqlx::query("SELECT * FROM fleet_firmware WHERE withdrawn_at IS NULL ORDER BY created_at").fetch_all(ctx.db()).await?;
    let mut eligible = Vec::new();
    for r in &rows {
        let f = firmware_from_row(r)?;
        if channels.contains(&f.channel.as_str()) && super::trusted_key(ctx, &f.signer_kid).await?.is_some() {
            eligible.push(f);
        }
    }
    let mut newest: std::collections::BTreeMap<String, Firmware> = std::collections::BTreeMap::new();
    for f in &eligible {
        for c in &f.classes {
            match newest.get(c) {
                Some(n) if !older(&n.version, &f.version) => {}
                _ => {
                    newest.insert(c.clone(), f.clone());
                }
            }
        }
    }
    let mut started = std::collections::HashSet::new();
    for f in newest.values() {
        if !started.insert(f.id.clone()) {
            continue;
        }
        let had: Option<String> = sqlx::query_scalar("SELECT id FROM fleet_rollouts WHERE firmware_id = ?").bind(&f.id).fetch_optional(ctx.db()).await?;
        if had.is_none() {
            start_rollout(ctx, f, Some("auto")).await?;
        }
    }
    Ok(())
}

/// A collar an image could go to.
struct Target {
    collar: Collar,
    unit: Unit,
}

/// Claimed units of the release's classes, on a board it names, whose
/// bootloader trusts its signer (when the factory record said which keys).
async fn targets(ctx: &Ctx, fw: &Firmware) -> anyhow::Result<Vec<Target>> {
    let rows = sqlx::query("SELECT u.serial, c.* FROM fleet_units u JOIN collars c ON c.id = u.collar_id WHERE u.state IN ('claimed', 'in_service') ORDER BY c.id")
        .fetch_all(ctx.db())
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        let serial: String = r.try_get("serial")?;
        let Some(unit) = units::get_unit(ctx, &serial).await? else { continue };
        if !fw.classes.contains(&unit.device_class) || !fw.hw_revs.contains(&unit.hw_rev) {
            continue;
        }
        if !unit.image_keys.is_empty() && !unit.image_keys.contains(&fw.signer_kid) {
            continue;
        }
        out.push(Target { collar: op_core::store::collar_from_row(r)?, unit });
    }
    Ok(out)
}

/// What a collar runs: its reported version, else the one it was built with.
fn current_of(collar: &Collar, unit: Option<&Unit>) -> Option<String> {
    collar.fw.clone().or_else(|| unit.map(|u| u.fw_version.clone()).filter(|v| !v.is_empty()))
}

/// Why a collar can't take the image right now, if it can't.
async fn held_because(ctx: &Ctx, collar: &Collar, fw: &Firmware, p: &FirmwarePolicy, rollout_paused: bool) -> anyhow::Result<Option<String>> {
    if p.mode == Mode::Paused || rollout_paused {
        return Ok(Some("Updates are paused".into()));
    }
    if p.held_herds.contains(&collar.herd_id) {
        return Ok(Some("Its herd is held at its version".into()));
    }
    if !fw.has_image {
        return Ok(Some("The image isn't on this server yet".into()));
    }
    let moving: Option<String> = sqlx::query_scalar("SELECT id FROM moves WHERE herd_id = ? AND status = 'sweeping'").bind(&collar.herd_id).fetch_optional(ctx.db()).await?;
    if moving.is_some() {
        return Ok(Some("The herd is being moved".into()));
    }
    let escaped: Option<String> = sqlx::query_scalar("SELECT id FROM escapes WHERE collar_id = ? AND status = 'returning'").bind(&collar.id).fetch_optional(ctx.db()).await?;
    if escaped.is_some() {
        return Ok(Some("Out on an escape".into()));
    }
    let floor = p.battery_floor.max(fw.min_battery.unwrap_or(0) as f64 / 100.0);
    match collar.battery {
        None => return Ok(Some("Battery not reported yet".into())),
        Some(b) if b < floor => return Ok(Some(format!("Battery {:.0} %, needs {:.0} %", b * 100.0, floor * 100.0))),
        _ => {}
    }
    Ok(None)
}

#[derive(Debug, Clone)]
struct Prev {
    state: UnitState,
    reason: Option<String>,
    current: Option<String>,
    applied_at: Option<DateTime<Utc>>,
}

async fn unit_row(ctx: &Ctx, rollout_id: &str, collar_id: &str) -> anyhow::Result<Option<Prev>> {
    let r = sqlx::query("SELECT state, reason, current, applied_at FROM fleet_rollout_units WHERE rollout_id = ? AND collar_id = ?").bind(rollout_id).bind(collar_id).fetch_optional(ctx.db()).await?;
    r.map(|r| -> anyhow::Result<Prev> {
        Ok(Prev {
            state: op_core::DbEnum::from_db(&r.try_get::<String, _>("state")?)?,
            reason: r.try_get("reason")?,
            current: r.try_get("current")?,
            applied_at: op_core::time::opt_from_db(r.try_get("applied_at")?)?,
        })
    })
    .transpose()
}

/// Work out every live rollout again.
pub async fn tick(ctx: &Ctx) -> anyhow::Result<()> {
    let p = policy(ctx).await?;
    auto_start(ctx, &p).await?;
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM fleet_rollouts WHERE state IN ('active', 'paused') ORDER BY created_at").fetch_all(ctx.db()).await?;
    for id in ids {
        step(ctx, &id, &p).await?;
    }
    Ok(())
}

async fn step(ctx: &Ctx, rollout_id: &str, p: &FirmwarePolicy) -> anyhow::Result<()> {
    let Some(r) = get_rollout(ctx, rollout_id).await? else { return Ok(()) };
    let Some(fw) = super::get_firmware(ctx, &r.firmware_id).await? else { return Ok(()) };
    let paused = r.state == "paused";
    let at = now();
    let all = targets(ctx, &fw).await?;
    let mut waves = r.waves.clone();
    // The first collar: one that can take it, in the herd with the most collars it fits.
    if waves.first().is_some_and(|w| w.collar_id.is_none()) {
        let mut herds: std::collections::BTreeMap<&str, usize> = std::collections::BTreeMap::new();
        for t in &all {
            *herds.entry(t.collar.herd_id.as_str()).or_default() += 1;
        }
        let first = all
            .iter()
            .filter(|t| t.collar.caps.iter().any(|c| c == FOTA_CAP) && current_of(&t.collar, Some(&t.unit)).is_none_or(|c| older(&c, &fw.version)))
            .max_by_key(|t| (herds.get(t.collar.herd_id.as_str()).copied().unwrap_or(0), std::cmp::Reverse(bucket(&t.collar.id))));
        if let Some(t) = first {
            waves[0].collar_id = Some(t.collar.id.clone());
            if let Some(w) = waves.get_mut(1) {
                w.herd_id = Some(t.collar.herd_id.clone());
            }
            sqlx::query("UPDATE fleet_rollouts SET waves = ?, updated_at = ? WHERE id = ?").bind(serde_json::to_string(&waves)?).bind(to_db(&at)).bind(rollout_id).execute(ctx.db()).await?;
        }
    }
    let mut problem: Option<String> = None;
    let mut all_confirmed = true;
    // Earlier stages done: every collar in them that took the image confirmed it.
    let mut pending_in = [false; 3];
    let mut confirmed_in = [false; 3];
    for t in &all {
        let collar = &t.collar;
        let current = current_of(collar, Some(&t.unit));
        let wave = if waves.first().is_some_and(|w| w.collar_id.is_some()) { wave_of(&collar.id, &collar.herd_id, &waves) } else { 0 };
        let chosen = waves.first().is_some_and(|w| w.collar_id.is_some());
        let prev = unit_row(ctx, rollout_id, &collar.id).await?;
        let at_target = current.as_deref() == Some(fw.version.as_str());
        let silent = prev.as_ref().and_then(|x| x.applied_at).is_some_and(|applied| at - applied > SILENT_AFTER && collar.last_seen.is_none_or(|seen| seen < applied));
        let (state, reason) = match prev.as_ref().map(|x| x.state) {
            // Ran the release, now runs something older: the collar went back.
            Some(UnitState::Confirmed) if !at_target && current.as_deref().is_some_and(|c| older(c, &fw.version)) => {
                (UnitState::RolledBack, Some(format!("Went back to {}", current.clone().unwrap_or_default())))
            }
            Some(s @ (UnitState::Failed | UnitState::RolledBack)) => (s, prev.as_ref().and_then(|x| x.reason.clone())),
            _ if at_target => (UnitState::Confirmed, None),
            _ if current.as_deref().is_some_and(|c| !older(c, &fw.version)) => (UnitState::Confirmed, Some(format!("Runs {}", current.clone().unwrap_or_default()))),
            Some(UnitState::Offered) if silent => (UnitState::Failed, Some("Not heard from since it installed".into())),
            _ if !collar.caps.iter().any(|c| c == FOTA_CAP) => (UnitState::Unsupported, Some(UNSUPPORTED.into())),
            _ if !chosen || wave > r.wave => (UnitState::Waiting, Some(stage_words(wave))),
            Some(UnitState::Offered) => (UnitState::Offered, prev.as_ref().and_then(|x| x.reason.clone())),
            _ => match held_because(ctx, collar, &fw, p, paused).await? {
                Some(why) => (UnitState::Held, Some(why)),
                None => (UnitState::Ready, None),
            },
        };
        let w = wave.clamp(0, 2) as usize;
        if state == UnitState::Confirmed {
            confirmed_in[w] = true;
        } else {
            all_confirmed = false;
        }
        if matches!(state, UnitState::Offered) {
            pending_in[w] = true;
        }
        if matches!(state, UnitState::Failed | UnitState::RolledBack) && problem.is_none() {
            problem = Some(format!("{} {}", collar.name, if state == UnitState::Failed { "failed to update" } else { "rolled back" }));
        }
        let changed = prev.as_ref().is_none_or(|x| x.state != state || x.current != current);
        sqlx::query(
            "INSERT INTO fleet_rollout_units (rollout_id, collar_id, serial, wave, state, reason, current, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(rollout_id, collar_id) DO UPDATE SET wave = excluded.wave, state = excluded.state, reason = excluded.reason, current = excluded.current,
               updated_at = CASE WHEN fleet_rollout_units.state != excluded.state OR fleet_rollout_units.current IS NOT excluded.current THEN excluded.updated_at ELSE fleet_rollout_units.updated_at END",
        )
        .bind(rollout_id)
        .bind(&collar.id)
        .bind(&t.unit.serial)
        .bind(wave)
        .bind(op_core::DbEnum::as_db(&state))
        .bind(&reason)
        .bind(&current)
        .bind(to_db(&at))
        .execute(ctx.db())
        .await?;
        if changed && matches!(state, UnitState::Confirmed | UnitState::RolledBack | UnitState::Failed) && prev.is_some() {
            let mut conn = ctx.db().acquire().await?;
            units::log(&mut conn, &t.unit.serial, "firmware", json!({ "rollout_id": rollout_id, "version": fw.version, "state": state, "current": current }), None).await?;
        }
    }
    if let Some(why) = problem
        && r.state == "active"
    {
        set_state(ctx, rollout_id, "paused", Some(&why)).await?;
        tracing::warn!(rollout = %rollout_id, "{why}; rollout paused");
        return Ok(());
    }
    if paused {
        return Ok(());
    }
    if !all.is_empty() && all_confirmed {
        set_state(ctx, rollout_id, "done", None).await?;
        return Ok(());
    }
    // The next stage: its time has come, nothing in the earlier stages is
    // still installing, and the herd stage waits for the first collar's confirm.
    let next = r.wave as usize + 1;
    let started = waves.first().and_then(|w| w.started_at);
    let due = match (waves.get(next), started) {
        (Some(w), Some(s)) => at - s >= Duration::minutes((w.after_h * 60.0) as i64),
        _ => false,
    };
    let settled = !pending_in[..next.min(3)].iter().any(|&p| p);
    let first_ok = next != 1 || confirmed_in[0];
    if due && settled && first_ok {
        waves[next].started_at = Some(at);
        sqlx::query("UPDATE fleet_rollouts SET wave = ?, waves = ?, updated_at = ? WHERE id = ?")
            .bind(next as i64)
            .bind(serde_json::to_string(&waves)?)
            .bind(to_db(&at))
            .bind(rollout_id)
            .execute(ctx.db())
            .await?;
        tracing::info!(rollout = %rollout_id, stage = next + 1, "firmware rollout: next stage");
    }
    Ok(())
}

fn stage_words(wave: i64) -> String {
    match wave {
        0 => "Waiting for a first collar".into(),
        1 => "After the first collar confirms".into(),
        _ => "After the first herd".into(),
    }
}

// ---- per collar ----------------------------------------------------------------------------------

fn unit_firmware_from_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<UnitFirmware> {
    Ok(UnitFirmware {
        serial: r.try_get("serial")?,
        collar_id: r.try_get("collar_id")?,
        name: r.try_get("name")?,
        herd_id: r.try_get("herd_id")?,
        current: r.try_get("current")?,
        target: r.try_get("version")?,
        rollout_id: r.try_get("rollout_id")?,
        state: op_core::DbEnum::from_db(&r.try_get::<String, _>("state")?)?,
        reason: r.try_get("reason")?,
        updated_at: op_core::time::from_db(&r.try_get::<String, _>("updated_at")?)?,
    })
}

const UNITS_SELECT: &str = "SELECT ru.*, c.name, c.herd_id, f.version FROM fleet_rollout_units ru
    JOIN fleet_rollouts r ON r.id = ru.rollout_id JOIN fleet_firmware f ON f.id = r.firmware_id JOIN collars c ON c.id = ru.collar_id";

/// A collar with no rollout: current, or unsupported without `fota`.
fn standing(collar: &Collar, unit: Option<&Unit>) -> UnitFirmware {
    let fota = collar.caps.iter().any(|c| c == FOTA_CAP);
    UnitFirmware {
        serial: unit.map(|u| u.serial.clone()),
        collar_id: collar.id.clone(),
        name: collar.name.clone(),
        herd_id: Some(collar.herd_id.clone()),
        current: current_of(collar, unit),
        target: None,
        rollout_id: None,
        state: if fota { UnitState::Current } else { UnitState::Unsupported },
        reason: (!fota).then(|| UNSUPPORTED.to_owned()),
        updated_at: collar.last_seen.unwrap_or_else(now),
    }
}

/// Where one collar stands: its place in a live rollout, else its standing.
pub async fn status_for_collar(ctx: &Ctx, collar_id: &str) -> anyhow::Result<Option<UnitFirmware>> {
    let row = sqlx::query(&format!("{UNITS_SELECT} WHERE ru.collar_id = ? AND r.state IN ('active', 'paused') ORDER BY r.created_at DESC LIMIT 1"))
        .bind(collar_id)
        .fetch_optional(ctx.db())
        .await?;
    if let Some(r) = row {
        return Ok(Some(unit_firmware_from_row(&r)?));
    }
    let Some(c) = sqlx::query("SELECT * FROM collars WHERE id = ?").bind(collar_id).fetch_optional(ctx.db()).await? else { return Ok(None) };
    let collar = op_core::store::collar_from_row(&c)?;
    let unit = units::for_collar(ctx, collar_id).await?;
    Ok(Some(standing(&collar, unit.as_ref())))
}

/// `GET /api/fleet/firmware/units?herd_id=&rollout_id=`.
pub(crate) async fn units(State(ctx): State<Ctx>, Query(q): Query<UnitsQuery>) -> ApiResult<Json<Vec<UnitFirmware>>> {
    if let Some(rid) = q.rollout_id.as_deref().filter(|s| !s.is_empty()) {
        let rows = sqlx::query(&format!("{UNITS_SELECT} WHERE ru.rollout_id = ? ORDER BY c.name, c.id")).bind(rid).fetch_all(ctx.db()).await?;
        return Ok(Json(rows.iter().map(unit_firmware_from_row).collect::<anyhow::Result<_>>()?));
    }
    let collars = match q.herd_id.as_deref().filter(|s| !s.is_empty()) {
        Some(h) => sqlx::query("SELECT * FROM collars WHERE herd_id = ? ORDER BY name, id").bind(h).fetch_all(ctx.db()).await?,
        None => sqlx::query("SELECT * FROM collars ORDER BY name, id").fetch_all(ctx.db()).await?,
    };
    let mut out = Vec::with_capacity(collars.len());
    for c in &collars {
        let id: String = c.try_get("id")?;
        if let Some(s) = status_for_collar(&ctx, &id).await? {
            out.push(s);
        }
    }
    Ok(Json(out))
}

// ---- hooks for the collar ingest -------------------------------------------------------------------

/// What goes in the collar's firmware command (protocol v2 type 9).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FirmwareOffer {
    pub rollout_id: String,
    pub firmware_id: String,
    pub version: String,
    pub sha256: String,
    pub size: i64,
    /// The signed release manifest, as is: the command's key 16.
    pub manifest: Vec<u8>,
}

/// An image for this collar if it is `ready` in a live rollout and nothing
/// holds it now (checked again here). Marks it `offered`.
pub async fn offer_for(ctx: &Ctx, collar_id: &str) -> anyhow::Result<Option<FirmwareOffer>> {
    let row = sqlx::query(
        "SELECT ru.rollout_id, r.state AS rollout_state, f.* FROM fleet_rollout_units ru JOIN fleet_rollouts r ON r.id = ru.rollout_id JOIN fleet_firmware f ON f.id = r.firmware_id
         WHERE ru.collar_id = ? AND ru.state = 'ready' AND r.state = 'active' AND f.withdrawn_at IS NULL ORDER BY r.created_at DESC LIMIT 1",
    )
    .bind(collar_id)
    .fetch_optional(ctx.db())
    .await?;
    let Some(row) = row else { return Ok(None) };
    let rollout_id: String = row.try_get("rollout_id")?;
    let fw = firmware_from_row(&row)?;
    let Some(c) = sqlx::query("SELECT * FROM collars WHERE id = ?").bind(collar_id).fetch_optional(ctx.db()).await? else { return Ok(None) };
    let collar = op_core::store::collar_from_row(&c)?;
    if !collar.caps.iter().any(|c| c == FOTA_CAP) {
        return Ok(None);
    }
    let p = policy(ctx).await?;
    if let Some(why) = held_because(ctx, &collar, &fw, &p, false).await? {
        sqlx::query("UPDATE fleet_rollout_units SET state = 'held', reason = ?, updated_at = ? WHERE rollout_id = ? AND collar_id = ?")
            .bind(why)
            .bind(to_db(&now()))
            .bind(&rollout_id)
            .bind(collar_id)
            .execute(ctx.db())
            .await?;
        return Ok(None);
    }
    sqlx::query("UPDATE fleet_rollout_units SET state = 'offered', reason = NULL, offered_at = ?, updated_at = ? WHERE rollout_id = ? AND collar_id = ?")
        .bind(to_db(&now()))
        .bind(to_db(&now()))
        .bind(&rollout_id)
        .bind(collar_id)
        .execute(ctx.db())
        .await?;
    let manifest = crate::keys::unhex(&fw.manifest).unwrap_or_default();
    Ok(Some(FirmwareOffer { rollout_id, firmware_id: fw.id, version: fw.version, sha256: fw.sha256, size: fw.size, manifest }))
}

/// What a collar said about an image.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "result")]
pub enum FwResult {
    /// Written, waiting to boot.
    Applied,
    /// Booted and confirmed itself.
    Confirmed,
    /// Didn't confirm; the bootloader went back.
    RolledBack,
    /// Refused or couldn't write it, with the collar's code.
    Failed { code: Option<String> },
}

/// Record a collar's result for the image `version`. A rollback or failure
/// pauses the rollout at its next tick.
pub async fn record_result(ctx: &Ctx, collar_id: &str, version: &str, result: &FwResult) -> anyhow::Result<()> {
    let row = sqlx::query(
        "SELECT ru.rollout_id, ru.serial FROM fleet_rollout_units ru JOIN fleet_rollouts r ON r.id = ru.rollout_id JOIN fleet_firmware f ON f.id = r.firmware_id
         WHERE ru.collar_id = ? AND f.version = ? ORDER BY r.created_at DESC LIMIT 1",
    )
    .bind(collar_id)
    .bind(version)
    .fetch_optional(ctx.db())
    .await?;
    let Some(row) = row else { return Ok(()) };
    let rollout_id: String = row.try_get("rollout_id")?;
    let serial: Option<String> = row.try_get("serial")?;
    let (state, reason) = match result {
        FwResult::Applied => (UnitState::Offered, Some("Written, waiting to boot".to_owned())),
        FwResult::Confirmed => (UnitState::Confirmed, None),
        FwResult::RolledBack => (UnitState::RolledBack, Some("Didn't confirm; went back".to_owned())),
        FwResult::Failed { code } => (UnitState::Failed, Some(code.clone().map(|c| format!("Refused: {c}")).unwrap_or_else(|| "Failed".into()))),
    };
    sqlx::query(
        "UPDATE fleet_rollout_units SET state = ?, reason = ?, updated_at = ?, applied_at = CASE WHEN ? THEN ? ELSE applied_at END WHERE rollout_id = ? AND collar_id = ?",
    )
    .bind(op_core::DbEnum::as_db(&state))
    .bind(&reason)
    .bind(to_db(&now()))
    .bind(matches!(result, FwResult::Applied))
    .bind(to_db(&now()))
        .bind(&rollout_id)
        .bind(collar_id)
        .execute(ctx.db())
        .await?;
    if let Some(serial) = serial {
        let mut conn = ctx.db().acquire().await?;
        units::log(&mut conn, &serial, "firmware", json!({ "rollout_id": rollout_id, "version": version, "result": result }), None).await?;
    }
    if !matches!(result, FwResult::Applied) {
        wake();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_stages_one_collar_its_herd_then_every_herd() {
        let mut w = waves(24.0);
        assert_eq!(w.iter().map(|w| (w.kind, w.after_h)).collect::<Vec<_>>(), vec![(WaveKind::Collar, 0.0), (WaveKind::Herd, 24.0), (WaveKind::All, 72.0)]);
        w[0].collar_id = Some("col_a".into());
        w[1].herd_id = Some("herd_1".into());
        assert_eq!(wave_of("col_a", "herd_1", &w), 0);
        assert_eq!(wave_of("col_b", "herd_1", &w), 1);
        assert_eq!(wave_of("col_c", "herd_2", &w), 2);
        assert_eq!(bucket("col_a"), bucket("col_a"), "stable");
    }
}
