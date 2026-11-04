//! Persisted claim-attempt limits: per caller, per serial, per attempt
//! target. Wrong codes count; a successful claim clears the serial's
//! lockout. Survives restart via `claim_lockouts`.
//!
//! A farm-wide counter still runs for abuse detection, but crossing it only
//! raises an audit/log alarm — it does **not** lock out other callers.

use std::time::{SystemTime, UNIX_EPOCH};

use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use op_core::time::{from_db, now, to_db};
use op_core::{ActivityEvent, ApiError, ApiResult, Ctx, id};
use serde_json::json;
use sha2::{Digest, Sha256};

/// Wrong claim codes allowed per caller / serial / target per minute.
pub const TRIES_PER_MINUTE: usize = 5;
/// Farm-wide wrong-code rate that raises a soft abuse alarm (does not 429 others).
pub const GLOBAL_ALARM_PER_MINUTE: usize = 60;
/// First lockout after a full window of failures on one serial/target (seconds);
/// doubles each time, capped.
const LOCK_BASE_SECS: i64 = 60;
const LOCK_MAX_SECS: i64 = 3600;

const SCOPE_CALLER: &str = "caller";
const SCOPE_SERIAL: &str = "serial";
const SCOPE_TARGET: &str = "target";
const SCOPE_GLOBAL: &str = "global";
const GLOBAL_KEY: &str = "_";

#[derive(Debug, Clone)]
struct Row {
    fails: Vec<i64>,
    lock_until: Option<DateTime<Utc>>,
    lock_level: i64,
}

fn unix_now() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn parse_fails(raw: &str) -> Vec<i64> {
    serde_json::from_str::<Vec<i64>>(raw).unwrap_or_default()
}

fn prune(fails: &mut Vec<i64>, now_s: i64) {
    fails.retain(|&t| now_s.saturating_sub(t) < 60);
}

async fn load(ctx: &Ctx, scope: &str, key: &str) -> anyhow::Result<Row> {
    let row: Option<(String, Option<String>, i64)> = sqlx::query_as("SELECT fails, lock_until, lock_level FROM claim_lockouts WHERE scope = ? AND key = ?")
        .bind(scope)
        .bind(key)
        .fetch_optional(ctx.db())
        .await?;
    Ok(match row {
        Some((fails, lock_until, lock_level)) => {
            Row { fails: parse_fails(&fails), lock_until: lock_until.as_deref().and_then(|s| from_db(s).ok()), lock_level }
        }
        None => Row { fails: Vec::new(), lock_until: None, lock_level: 0 },
    })
}

async fn save(ctx: &Ctx, scope: &str, key: &str, row: &Row) -> anyhow::Result<()> {
    let fails = serde_json::to_string(&row.fails)?;
    let lock = row.lock_until.as_ref().map(to_db);
    sqlx::query(
        "INSERT INTO claim_lockouts (scope, key, fails, lock_until, lock_level) VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(scope, key) DO UPDATE SET fails = excluded.fails, lock_until = excluded.lock_until, lock_level = excluded.lock_level",
    )
    .bind(scope)
    .bind(key)
    .bind(fails)
    .bind(lock)
    .bind(row.lock_level)
    .execute(ctx.db())
    .await?;
    Ok(())
}

fn locked_for(row: &Row, at: DateTime<Utc>) -> Option<u64> {
    row.lock_until.filter(|u| *u > at).map(|u| (u - at).num_seconds().max(1) as u64)
}

fn window_retry(fails: &[i64], limit: usize, now_s: i64) -> Option<u64> {
    if fails.len() < limit {
        return None;
    }
    let oldest = fails.iter().copied().min()?;
    Some(60i64.saturating_sub(now_s.saturating_sub(oldest)).max(1) as u64)
}

fn too_many(retry_after: u64) -> ApiError {
    ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Too many wrong claim codes. Wait and try again.").with_retry_after(retry_after)
}

/// Stable key for an attempt that names no serial yet (typed code alone).
pub fn target_for_code(code: &str) -> String {
    let dig = Sha256::digest(code.as_bytes());
    format!("code:{:x}", u64::from_be_bytes(dig[..8].try_into().unwrap()))
}

/// Stable key for a card or path serial.
pub fn target_for_serial(serial: &str) -> String {
    format!("serial:{serial}")
}

fn blocked(row: &Row, limit: usize, at: DateTime<Utc>, now_s: i64) -> Option<u64> {
    if let Some(s) = locked_for(row, at) {
        return Some(s);
    }
    let mut fails = row.fails.clone();
    prune(&mut fails, now_s);
    window_retry(&fails, limit, now_s)
}

/// Whether this caller may try a claim against `target` (and optional `serial`) now.
///
/// Only per-caller / per-serial / per-target ceilings block. The farm-wide
/// counter is checked for a soft alarm elsewhere (`wrong`), never here.
pub async fn may_try(ctx: &Ctx, caller: &str, serial: Option<&str>, target: &str) -> ApiResult<()> {
    let at = now();
    let now_s = unix_now();
    let mut retry = 0u64;
    let mut take = |row: &Row, limit: usize| {
        if let Some(s) = blocked(row, limit, at, now_s) {
            retry = retry.max(s);
        }
    };
    take(&load(ctx, SCOPE_CALLER, caller).await?, TRIES_PER_MINUTE);
    take(&load(ctx, SCOPE_TARGET, target).await?, TRIES_PER_MINUTE);
    if let Some(serial) = serial {
        take(&load(ctx, SCOPE_SERIAL, serial).await?, TRIES_PER_MINUTE);
    }
    if retry > 0 {
        return Err(too_many(retry));
    }
    Ok(())
}

async fn bump(ctx: &Ctx, scope: &str, key: &str, limit: usize, lock_on_full: bool) -> anyhow::Result<()> {
    let at = now();
    let now_s = unix_now();
    let mut row = load(ctx, scope, key).await?;
    if locked_for(&row, at).is_some() {
        return Ok(());
    }
    prune(&mut row.fails, now_s);
    row.fails.push(now_s);
    if lock_on_full && row.fails.len() >= limit {
        let level = row.lock_level.saturating_add(1).min(16);
        let secs = (LOCK_BASE_SECS.saturating_mul(1i64 << (level.saturating_sub(1).min(10)))).min(LOCK_MAX_SECS);
        row.lock_level = level;
        row.lock_until = Some(at + Duration::seconds(secs));
        row.fails.clear();
    }
    save(ctx, scope, key, &row).await?;
    Ok(())
}

async fn bump_all(ctx: &Ctx, caller: &str, serial: Option<&str>, target: &str, lock_targets: bool) {
    let bump_one = async |scope: &str, key: &str, limit: usize, lock: bool| {
        if let Err(e) = bump(ctx, scope, key, limit, lock).await {
            tracing::warn!("claim lockout: {e:#}");
        }
    };
    bump_one(SCOPE_CALLER, caller, TRIES_PER_MINUTE, false).await;
    bump_one(SCOPE_TARGET, target, TRIES_PER_MINUTE, lock_targets).await;
    if let Some(serial) = serial {
        bump_one(SCOPE_SERIAL, serial, TRIES_PER_MINUTE, lock_targets).await;
    }
}

/// Record a wrong-code attempt. Always the same 404 (wrong code and unknown
/// serial look identical); the next try may get 429 from [`may_try`].
pub async fn wrong(ctx: &Ctx, caller: &str, serial: Option<&str>, target: &str) -> ApiError {
    bump_all(ctx, caller, serial, target, true).await;
    // Farm-wide counter: soft alarm only — never blocks other callers.
    let before = load(ctx, SCOPE_GLOBAL, GLOBAL_KEY).await.ok();
    let before_n = before
        .as_ref()
        .map(|r| {
            let mut f = r.fails.clone();
            prune(&mut f, unix_now());
            f.len()
        })
        .unwrap_or(0);
    if let Err(e) = bump(ctx, SCOPE_GLOBAL, GLOBAL_KEY, GLOBAL_ALARM_PER_MINUTE, false).await {
        tracing::warn!("claim lockout: {e:#}");
    }
    if before_n < GLOBAL_ALARM_PER_MINUTE {
        if let Ok(after) = load(ctx, SCOPE_GLOBAL, GLOBAL_KEY).await {
            let mut fails = after.fails.clone();
            prune(&mut fails, unix_now());
            if fails.len() >= GLOBAL_ALARM_PER_MINUTE {
                soft_global_alarm(ctx, fails.len()).await;
            }
        }
    }
    ApiError::not_found("That claim code doesn't match a unit here. Check it against the card.")
}

/// Count one release / sensitive action toward the same ceilings (no 404).
pub async fn record(ctx: &Ctx, caller: &str, serial: Option<&str>, target: &str) {
    bump_all(ctx, caller, serial, target, false).await;
}

async fn soft_global_alarm(ctx: &Ctx, count: usize) {
    tracing::warn!(count, limit = GLOBAL_ALARM_PER_MINUTE, "claim abuse: farm-wide wrong-code rate exceeded (soft alarm, no lockout)");
    let at = now();
    let e = ActivityEvent {
        id: id::new_id(id::EVENT),
        kind: "fleet.claim.abuse_alarm".into(),
        source: "system".into(),
        occurred_at: at,
        recorded_at: at,
        title: "Many wrong claim codes across the farm".into(),
        body: Some(format!("{count} wrong codes in the last minute (alarm at {GLOBAL_ALARM_PER_MINUTE}/min). Callers are not locked out farm-wide.")),
        payload: json!({ "count": count, "limit": GLOBAL_ALARM_PER_MINUTE }),
        targets: vec![("farm".into(), "farm".into())],
    };
    if let Err(err) = ctx.store().record_event(&e).await {
        tracing::warn!("claim abuse alarm event: {err:#}");
    }
}

/// A successful claim against `serial` clears its lockout ladder.
pub async fn clear_serial(ctx: &Ctx, serial: &str) {
    let row = Row { fails: Vec::new(), lock_until: None, lock_level: 0 };
    if let Err(e) = save(ctx, SCOPE_SERIAL, serial, &row).await {
        tracing::warn!(serial, "claim lockout clear: {e:#}");
    }
}

/// SHA-256 hex of a serial used when auditing an unknown unit.
pub fn serial_hash(serial: &str) -> String {
    format!("{:x}", Sha256::digest(serial.as_bytes()))
}
