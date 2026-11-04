//! Collar re-arm: queue a signed command after an auto shut-off, deliver it in
//! the report response, and record acks. Optional guards (Cody) default off.
//!
//! Role gate: [`Role::Manager`] and above for human sessions and for agent/API
//! tokens (no human confirm step). Viewer and Hand get 403.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Extension, Json, Router};
use chrono::{DateTime, Duration, Utc};
use op_core::identity::{Identity, Role, Via};
use op_core::people::SessionToken;
use op_core::time::{from_db, now, opt_from_db, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Collar, CollarRearm, Ctx, Event, FenceState, id};
use op_protocol::rearm::{REARM_TTL_HOURS, RearmCommand, ReasonCode, WelfareEvent, WelfareReport, sign_rearm, welfare_kind};
use op_protocol::{AckStatus, caps};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection};

/// Id prefix of re-arm commands.
pub const REARM: &str = "rrm";
/// Settings key of [`RearmGuards`].
pub const GUARDS_KEY: &str = "collars.rearm";
/// Longest free-text reason, bytes.
pub const MAX_REASON_BYTES: usize = 500;

/// Optional Cody guards; every field defaults to off.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct RearmGuards {
    /// At most one agent re-arm per collar per shut-off, and two per collar per
    /// 24 h; further agent attempts need a human (Manager+) instead.
    pub agent_rate_limit: bool,
    /// Refuse while the collar's latest fix is outside the boundary.
    pub refuse_outside: bool,
    /// Notify a person on every agent re-arm (opens an `agent_rearm` alert).
    pub notify_on_agent: bool,
}

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/collars/{id}/rearm", post(post_rearm))
}

#[derive(Debug, Deserialize)]
struct RearmBody {
    reason: String,
    #[serde(default)]
    reason_code: Option<String>,
}

#[derive(Debug, Serialize)]
struct RearmResponse {
    collar: Collar,
    command: RearmView,
}

#[derive(Debug, Clone, Serialize)]
struct RearmView {
    id: String,
    collar_id: String,
    herd_id: String,
    rearm_seq: u32,
    reason: String,
    reason_code: String,
    shutoff_at: DateTime<Utc>,
    issued_at: DateTime<Utc>,
    expires_at: DateTime<Utc>,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    ack_status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ack_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ack_at: Option<DateTime<Utc>>,
    by: Value,
}

async fn post_rearm(
    State(ctx): State<Ctx>,
    identity: Identity,
    token: Option<Extension<SessionToken>>,
    Path(collar_id): Path<String>,
    ApiJson(body): ApiJson<RearmBody>,
) -> ApiResult<(StatusCode, Json<RearmResponse>)> {
    identity.require(Role::Manager).map_err(|e| {
        if e.status == StatusCode::FORBIDDEN { ApiError::forbidden("Your role can't re-arm a collar. Manager or above is required.") } else { e }
    })?;
    let token_id = token.map(|Extension(SessionToken(id))| id);
    let collar = queue(&ctx, &collar_id, &body.reason, body.reason_code.as_deref(), &identity, token_id.as_deref(), false).await?;
    let command = latest_view(ctx.db(), &collar_id).await?.ok_or_else(|| ApiError::internal("rearm missing after queue"))?;
    Ok((StatusCode::CREATED, Json(RearmResponse { collar, command })))
}

/// Shared path for the HTTP route and the MCP tool.
pub async fn queue(
    ctx: &Ctx,
    collar_id: &str,
    reason: &str,
    reason_code: Option<&str>,
    identity: &Identity,
    token_id: Option<&str>,
    from_mcp: bool,
) -> ApiResult<Collar> {
    identity.require(Role::Manager).map_err(|e| {
        if e.status == StatusCode::FORBIDDEN { ApiError::forbidden("Your role can't re-arm a collar. Manager or above is required.") } else { e }
    })?;
    let reason = clean_reason(reason)?;
    let reason_code = parse_reason_code(reason_code)?;
    let guards = load_guards(ctx).await?;
    let at = now();
    let who = who_json(identity, token_id, from_mcp);

    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    expire_pending(&mut tx, Some(collar_id), at).await?;

    let row = sqlx::query("SELECT * FROM collars WHERE id = ?").bind(collar_id).fetch_optional(&mut *tx).await?;
    let mut collar = match row {
        Some(r) => op_core::store::collar_from_row(&r)?,
        None => return Err(ApiError::not_found("No such collar.")),
    };
    if !collar.caps.iter().any(|c| c == caps::REARM) {
        return Err(ApiError::conflict("This collar does not support re-arm.").with_details(json!({ "code": "rearm_not_supported" })));
    }
    let shutoff_at = match collar.shutoff_at {
        Some(t) => t,
        None => return Err(ApiError::conflict("This collar is not shut off.").with_details(json!({ "code": "not_shut_off" }))),
    };

    if let Some(pending) = pending_row(&mut tx, collar_id).await? {
        return Err(ApiError::conflict("A re-arm is already pending for this collar.").with_details(json!({ "code": "rearm_pending", "pending": pending })));
    }

    if guards.refuse_outside && collar.state == FenceState::Outside {
        return Err(ApiError::conflict("The collar's latest fix is outside the boundary.").with_details(json!({ "code": "outside_boundary" })));
    }

    let agent = who["kind"].as_str() == Some("agent");
    if guards.agent_rate_limit && agent {
        check_agent_rate(&mut tx, collar_id, shutoff_at, at).await?;
    }

    let server_seq: i64 = sqlx::query_scalar("SELECT rearm_seq FROM collars WHERE id = ?").bind(collar_id).fetch_one(&mut *tx).await?;
    let reported: i64 = sqlx::query_scalar("SELECT COALESCE(welfare_rearm_seq, 0) FROM collars WHERE id = ?").bind(collar_id).fetch_one(&mut *tx).await?;
    // Collar never accepts rearm_seq 0; max(DB, reported)+1 is always ≥ 1.
    let rearm_seq = (server_seq.max(reported) + 1).max(1) as u32;
    let command_id = id::new_id(REARM);
    let expires_at = at + Duration::hours(REARM_TTL_HOURS);

    sqlx::query(
        "INSERT INTO collar_rearms (id, collar_id, herd_id, rearm_seq, reason, reason_code, shutoff_at, issued_at, expires_at, status, actor, created_at, refresh_of)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, NULL)",
    )
    .bind(&command_id)
    .bind(collar_id)
    .bind(&collar.herd_id)
    .bind(rearm_seq as i64)
    .bind(&reason)
    .bind(reason_code.as_str())
    .bind(to_db(&shutoff_at))
    .bind(to_db(&at))
    .bind(to_db(&expires_at))
    .bind(who.to_string())
    .bind(to_db(&at))
    .execute(&mut *tx)
    .await?;

    sqlx::query("UPDATE collars SET rearm_seq = ? WHERE id = ?").bind(rearm_seq as i64).bind(collar_id).execute(&mut *tx).await?;

    audit(&mut tx, collar_id, Some(&command_id), at, &who, &reason, reason_code.as_str(), Some(shutoff_at), "issued", None, None).await?;

    tx.commit().await?;

    if guards.notify_on_agent && agent {
        if let Err(e) = notify_agent_rearm(ctx, &collar, &reason, reason_code.as_str(), &who, at).await {
            tracing::warn!(collar = %collar_id, "agent rearm notify: {e:#}");
        }
    }

    collar = enrich_one(ctx.db(), collar).await?;
    ctx.publish(Event::Collar { collar: collar.clone() });
    Ok(collar)
}

fn clean_reason(reason: &str) -> ApiResult<String> {
    let reason = reason.trim();
    if reason.is_empty() {
        return Err(ApiError::bad_request("A reason is required."));
    }
    if reason.len() > MAX_REASON_BYTES {
        return Err(ApiError::bad_request(format!("The reason is too long (max {MAX_REASON_BYTES} characters).")));
    }
    Ok(reason.to_owned())
}

fn parse_reason_code(s: Option<&str>) -> ApiResult<ReasonCode> {
    match s.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(ReasonCode::Other),
        Some(s) => ReasonCode::parse(s)
            .ok_or_else(|| ApiError::bad_request(format!("Unknown reason_code {s:?}. Use animal_back_in, fence_fixed, false_trigger, or other."))),
    }
}

fn who_json(identity: &Identity, token_id: Option<&str>, from_mcp: bool) -> Value {
    let kind = actor_kind(identity.via, from_mcp);
    let mut m = serde_json::Map::new();
    m.insert("via".into(), json!(identity.via));
    m.insert("kind".into(), json!(kind));
    if let Some(uid) = &identity.user_id {
        m.insert("user_id".into(), json!(uid));
    }
    if let Some(name) = &identity.name {
        m.insert("name".into(), json!(name));
    }
    if let Some(tid) = token_id {
        m.insert("token_id".into(), json!(tid));
    }
    Value::Object(m)
}

/// `agent` for brain/app-token/MCP callers; `human` for interactive sessions.
fn actor_kind(via: Via, from_mcp: bool) -> &'static str {
    if from_mcp || matches!(via, Via::Brain | Via::AppToken | Via::System) { "agent" } else { "human" }
}

pub async fn load_guards(ctx: &Ctx) -> anyhow::Result<RearmGuards> {
    Ok(ctx.store().get_setting::<RearmGuards>(GUARDS_KEY).await?.unwrap_or_default())
}

async fn check_agent_rate(tx: &mut SqliteConnection, collar_id: &str, shutoff_at: DateTime<Utc>, at: DateTime<Utc>) -> ApiResult<()> {
    let for_shutoff: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM collar_rearms WHERE collar_id = ? AND shutoff_at = ? AND json_extract(actor, '$.kind') = 'agent'")
            .bind(collar_id)
            .bind(to_db(&shutoff_at))
            .fetch_one(&mut *tx)
            .await?;
    if for_shutoff >= 1 {
        return Err(ApiError::conflict("An agent already re-armed this shut-off; a person must re-arm.").with_details(json!({ "code": "agent_rearm_limit" })));
    }
    let since = at - Duration::hours(REARM_TTL_HOURS);
    let in_window: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM collar_rearms WHERE collar_id = ? AND issued_at >= ? AND json_extract(actor, '$.kind') = 'agent'")
            .bind(collar_id)
            .bind(to_db(&since))
            .fetch_one(&mut *tx)
            .await?;
    if in_window >= 2 {
        return Err(ApiError::conflict("Agents may re-arm this collar at most twice per 24 hours; a person must re-arm.")
            .with_details(json!({ "code": "agent_rearm_limit" })));
    }
    Ok(())
}

async fn pending_row(tx: &mut SqliteConnection, collar_id: &str) -> anyhow::Result<Option<Value>> {
    let row = sqlx::query(
        "SELECT id, rearm_seq, reason, reason_code, shutoff_at, issued_at, expires_at, status, actor FROM collar_rearms
         WHERE collar_id = ? AND status = 'pending' ORDER BY rearm_seq DESC LIMIT 1",
    )
    .bind(collar_id)
    .fetch_optional(&mut *tx)
    .await?;
    row.map(|r| view_from_row(&r)).transpose()
}

fn view_from_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Value> {
    Ok(json!({
        "id": r.try_get::<String, _>("id")?,
        "rearm_seq": r.try_get::<i64, _>("rearm_seq")? as u32,
        "reason": r.try_get::<String, _>("reason")?,
        "reason_code": r.try_get::<String, _>("reason_code")?,
        "shutoff_at": r.try_get::<String, _>("shutoff_at")?,
        "issued_at": r.try_get::<String, _>("issued_at")?,
        "expires_at": r.try_get::<String, _>("expires_at")?,
        "status": r.try_get::<String, _>("status")?,
        "by": serde_json::from_str::<Value>(&r.try_get::<String, _>("actor")?).unwrap_or(Value::Null),
    }))
}

async fn audit(
    tx: &mut SqliteConnection,
    collar_id: &str,
    rearm_id: Option<&str>,
    at: DateTime<Utc>,
    who: &Value,
    reason: &str,
    reason_code: &str,
    shutoff_at: Option<DateTime<Utc>>,
    event: &str,
    ack_status: Option<&str>,
    ack_code: Option<&str>,
) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO collar_rearm_audit (id, collar_id, rearm_id, at, who, reason, reason_code, shutoff_at, event, ack_status, ack_code)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id::new_id("rra"))
    .bind(collar_id)
    .bind(rearm_id)
    .bind(to_db(&at))
    .bind(who.to_string())
    .bind(reason)
    .bind(reason_code)
    .bind(shutoff_at.as_ref().map(to_db))
    .bind(event)
    .bind(ack_status)
    .bind(ack_code)
    .execute(&mut *tx)
    .await?;
    Ok(())
}

/// Mark pending commands past `expires_at` as expired (no further delivery).
pub async fn expire_pending(tx: &mut SqliteConnection, collar_id: Option<&str>, at: DateTime<Utc>) -> anyhow::Result<usize> {
    let rows = match collar_id {
        Some(id) => {
            sqlx::query(
                "UPDATE collar_rearms SET status = 'expired' WHERE collar_id = ? AND status = 'pending' AND expires_at <= ? RETURNING id, collar_id, reason, reason_code, shutoff_at, actor",
            )
            .bind(id)
            .bind(to_db(&at))
            .fetch_all(&mut *tx)
            .await?
        }
        None => {
            sqlx::query(
                "UPDATE collar_rearms SET status = 'expired' WHERE status = 'pending' AND expires_at <= ? RETURNING id, collar_id, reason, reason_code, shutoff_at, actor",
            )
            .bind(to_db(&at))
            .fetch_all(&mut *tx)
            .await?
        }
    };
    let n = rows.len();
    for r in rows {
        let shutoff = opt_from_db(r.try_get("shutoff_at")?)?;
        let who: Value = serde_json::from_str(&r.try_get::<String, _>("actor")?).unwrap_or(json!({}));
        audit(
            tx,
            &r.try_get::<String, _>("collar_id")?,
            Some(&r.try_get::<String, _>("id")?),
            at,
            &who,
            &r.try_get::<String, _>("reason")?,
            &r.try_get::<String, _>("reason_code")?,
            shutoff,
            "expired",
            None,
            Some("expired"),
        )
        .await?;
    }
    Ok(n)
}

/// Pending command for a collar, re-signed with the current key, or `None` when
/// expired/absent. Never returns an expired command.
pub async fn pending_command(tx: &mut SqliteConnection, ctx: &Ctx, collar: &Collar, at: DateTime<Utc>) -> anyhow::Result<Option<RearmCommand>> {
    if !collar.caps.iter().any(|c| c == caps::REARM) {
        return Ok(None);
    }
    expire_pending(tx, Some(&collar.id), at).await?;
    let row = sqlx::query(
        "SELECT id, herd_id, rearm_seq, reason_code, shutoff_at, issued_at, expires_at FROM collar_rearms
         WHERE collar_id = ? AND status = 'pending' ORDER BY rearm_seq DESC LIMIT 1",
    )
    .bind(&collar.id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(r) = row else { return Ok(None) };
    let expires_at = from_db(&r.try_get::<String, _>("expires_at")?)?;
    if at >= expires_at {
        // Race with expire_pending: treat as gone.
        return Ok(None);
    }
    let reason_code = ReasonCode::parse(&r.try_get::<String, _>("reason_code")?).unwrap_or(ReasonCode::Other);
    let mut cmd = RearmCommand {
        command_id: r.try_get("id")?,
        collar_id: collar.id.clone(),
        herd_id: r.try_get("herd_id")?,
        rearm_seq: r.try_get::<i64, _>("rearm_seq")? as u32,
        issued_at: from_db(&r.try_get::<String, _>("issued_at")?)?,
        expires_at,
        shutoff_at: from_db(&r.try_get::<String, _>("shutoff_at")?)?,
        reason_code,
        sig: None,
    };
    // Re-sign on every delivery so a server key rotation does not drop pending
    // commands (chosen over drop-and-mark-superseded).
    sign_rearm(&mut cmd, ctx.signing_key());
    Ok(Some(cmd))
}

/// Apply a collar ack for a re-arm command. `version` on the wire is `rearm_seq`.
/// Returns `Ok(false)` when `command_id` is not a re-arm for this collar.
///
/// A rejection with registry code `stale` (6) — rearm_seq ≤ held, shutoff_at
/// mismatch, or enforcement already on — triggers one refresh: re-read the
/// collar's latest `shutoff_at` / `welfare_rearm_seq`, queue a new pending
/// command, and stop. A second `stale` on that refresh is not retried.
pub async fn apply_ack(
    tx: &mut SqliteConnection,
    collar: &Collar,
    command_id: &str,
    rearm_seq: u32,
    status: AckStatus,
    code: Option<&str>,
    at: DateTime<Utc>,
) -> ApiResult<bool> {
    let row = sqlx::query("SELECT * FROM collar_rearms WHERE id = ? AND collar_id = ?").bind(command_id).bind(&collar.id).fetch_optional(&mut *tx).await?;
    let Some(r) = row else { return Ok(false) };
    let seq = r.try_get::<i64, _>("rearm_seq")? as u32;
    if seq != rearm_seq {
        return Err(ApiError::bad_request(format!("Command {command_id} is rearm_seq {seq}, not {rearm_seq}.")));
    }
    if r.try_get::<String, _>("status")? != "pending" {
        return Ok(true); // already settled; idempotent
    }
    let (new_status, event) = match status {
        AckStatus::Applied | AckStatus::Received => ("acked", "acked"),
        AckStatus::Rejected => ("rejected", "rejected"),
    };
    let ack_code = code.filter(|_| status == AckStatus::Rejected);
    sqlx::query("UPDATE collar_rearms SET status = ?, ack_status = ?, ack_code = ?, ack_at = ? WHERE id = ?")
        .bind(new_status)
        .bind(status.as_str())
        .bind(ack_code)
        .bind(to_db(&at))
        .bind(command_id)
        .execute(&mut *tx)
        .await?;
    let who: Value = serde_json::from_str(&r.try_get::<String, _>("actor")?).unwrap_or(json!({}));
    let reason: String = r.try_get("reason")?;
    let reason_code: String = r.try_get("reason_code")?;
    let shutoff = opt_from_db(r.try_get("shutoff_at")?)?;
    let refresh_of: Option<String> = r.try_get("refresh_of")?;
    audit(tx, &collar.id, Some(command_id), at, &who, &reason, &reason_code, shutoff, event, Some(status.as_str()), ack_code).await?;
    if status == AckStatus::Applied {
        // Enforcement is back on; clear shut-off until the collar reports again.
        sqlx::query("UPDATE collars SET welfare_state = 'on', shutoff_at = NULL WHERE id = ?").bind(&collar.id).execute(&mut *tx).await?;
    } else if status == AckStatus::Rejected && is_stale_code(ack_code) && refresh_of.is_none() {
        refresh_after_stale(tx, collar, command_id, &reason, &reason_code, &who, at).await?;
    }
    Ok(true)
}

fn is_stale_code(code: Option<&str>) -> bool {
    matches!(code, Some("stale") | Some("6"))
}

/// One-shot re-sign after `stale` (6): latest reported shutoff_at + next seq.
async fn refresh_after_stale(
    tx: &mut SqliteConnection,
    collar: &Collar,
    prior_id: &str,
    reason: &str,
    reason_code: &str,
    who: &Value,
    at: DateTime<Utc>,
) -> anyhow::Result<()> {
    let row = sqlx::query("SELECT shutoff_at, welfare_state, rearm_seq, COALESCE(welfare_rearm_seq, 0) AS welfare_rearm_seq FROM collars WHERE id = ?")
        .bind(&collar.id)
        .fetch_one(&mut *tx)
        .await?;
    let shutoff_at = opt_from_db(row.try_get("shutoff_at")?)?;
    let welfare_state: Option<String> = row.try_get("welfare_state")?;
    // Enforcement on / no shut-off: nothing to refresh with.
    if shutoff_at.is_none() || welfare_state.as_deref() == Some("on") {
        tracing::info!(collar = %collar.id, prior = %prior_id, "stale rearm: no shut-off to refresh against");
        return Ok(());
    }
    let shutoff_at = shutoff_at.unwrap();
    let server_seq: i64 = row.try_get("rearm_seq")?;
    let reported: i64 = row.try_get("welfare_rearm_seq")?;
    let rearm_seq = (server_seq.max(reported) + 1).max(1) as u32;
    let command_id = id::new_id(REARM);
    let expires_at = at + Duration::hours(REARM_TTL_HOURS);
    sqlx::query(
        "INSERT INTO collar_rearms (id, collar_id, herd_id, rearm_seq, reason, reason_code, shutoff_at, issued_at, expires_at, status, actor, created_at, refresh_of)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?, ?, ?)",
    )
    .bind(&command_id)
    .bind(&collar.id)
    .bind(&collar.herd_id)
    .bind(rearm_seq as i64)
    .bind(reason)
    .bind(reason_code)
    .bind(to_db(&shutoff_at))
    .bind(to_db(&at))
    .bind(to_db(&expires_at))
    .bind(who.to_string())
    .bind(to_db(&at))
    .bind(prior_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE collars SET rearm_seq = ? WHERE id = ?").bind(rearm_seq as i64).bind(&collar.id).execute(&mut *tx).await?;
    audit(tx, &collar.id, Some(&command_id), at, who, reason, reason_code, Some(shutoff_at), "refreshed", None, None).await?;
    tracing::info!(collar = %collar.id, prior = %prior_id, new = %command_id, rearm_seq, "stale rearm refreshed once");
    Ok(())
}

/// Persist welfare from a report: collar fields and events. Returns whether an
/// auto_shutoff event was stored (for alerting).
pub async fn apply_welfare(
    tx: &mut SqliteConnection,
    collar: &mut Collar,
    herd_id: &str,
    welfare: Option<&WelfareReport>,
    events: &[WelfareEvent],
    received: DateTime<Utc>,
) -> anyhow::Result<bool> {
    let mut auto_shutoff = false;
    if let Some(w) = welfare {
        if let Some(e) = &w.enforcement {
            collar.welfare_state = Some(e.as_str().to_owned());
        }
        if let Some(s) = w.shutoff_at {
            collar.shutoff_at = Some(s);
        }
        if let Some(seq) = w.rearm_seq {
            sqlx::query("UPDATE collars SET welfare_rearm_seq = MAX(COALESCE(welfare_rearm_seq, 0), ?) WHERE id = ?")
                .bind(seq as i64)
                .bind(&collar.id)
                .execute(&mut *tx)
                .await?;
        }
        sqlx::query("UPDATE collars SET welfare_state = ?, shutoff_at = ? WHERE id = ?")
            .bind(&collar.welfare_state)
            .bind(collar.shutoff_at.as_ref().map(to_db))
            .bind(&collar.id)
            .execute(&mut *tx)
            .await?;
    }
    for e in events {
        let at = e.at.unwrap_or(received);
        let payload = serde_json::to_string(&e.extra).ok();
        let res = sqlx::query(
            "INSERT INTO collar_welfare_events (id, collar_id, herd_id, kind, at, t, payload) VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(collar_id, t, kind) DO NOTHING",
        )
        .bind(id::new_id("wfe"))
        .bind(&collar.id)
        .bind(herd_id)
        .bind(e.kind as i64)
        .bind(to_db(&at))
        .bind(op_core::time::unix_ms(&at))
        .bind(payload)
        .execute(&mut *tx)
        .await?;
        if res.rows_affected() > 0 && welfare_kind::is_auto_shutoff(e.kind) {
            auto_shutoff = true;
            if collar.welfare_state.as_deref() != Some("off") {
                collar.welfare_state = Some("off".into());
            }
            if collar.shutoff_at.is_none() {
                collar.shutoff_at = Some(at);
            }
            sqlx::query("UPDATE collars SET welfare_state = ?, shutoff_at = COALESCE(shutoff_at, ?) WHERE id = ?")
                .bind(&collar.welfare_state)
                .bind(to_db(&at))
                .bind(&collar.id)
                .execute(&mut *tx)
                .await?;
        }
    }
    Ok(auto_shutoff)
}

/// Open (or refresh) an `auto_shutoff` alert for a collar.
pub async fn raise_auto_shutoff_alert(ctx: &Ctx, collar: &Collar, at: DateTime<Utc>) -> anyhow::Result<()> {
    let key = format!("auto_shutoff:{}", collar.id);
    let existing: Option<String> =
        sqlx::query_scalar("SELECT id FROM alerts WHERE key = ? AND status != 'resolved'").bind(&key).fetch_optional(ctx.db()).await?;
    if existing.is_some() {
        sqlx::query("UPDATE alerts SET seen_at = ?, updated_at = ? WHERE key = ? AND status != 'resolved'")
            .bind(to_db(&at))
            .bind(to_db(&at))
            .bind(&key)
            .execute(ctx.db())
            .await?;
        return Ok(());
    }
    let label = collar.name.clone();
    let alert_id = id::new_id("alr");
    let data = json!({
        "label": label,
        "since": collar.shutoff_at.as_ref().map(to_db),
        "collar_id": &collar.id,
    });
    let title = format!("{label} auto shut-off");
    let targets = json!([["collar", &collar.id]]);
    let point = collar.last_fix.as_ref().map(|f| f.point);
    let inserted = sqlx::query(
        "INSERT INTO alerts (id, kind, key, severity, status, herd_id, title, body, at_lon, at_lat, targets, data, opened_at, updated_at, seen_at, notify, batch_at)
         VALUES (?, 'auto_shutoff', ?, 'critical', 'open', ?, ?, NULL, ?, ?, ?, ?, ?, ?, ?, 1, ?)
         ON CONFLICT DO NOTHING",
    )
    .bind(&alert_id)
    .bind(&key)
    .bind(&collar.herd_id)
    .bind(&title)
    .bind(point.map(|p| p[0]))
    .bind(point.map(|p| p[1]))
    .bind(targets.to_string())
    .bind(data.to_string())
    .bind(to_db(&at))
    .bind(to_db(&at))
    .bind(to_db(&at))
    .bind(to_db(&at))
    .execute(ctx.db())
    .await?
    .rows_affected()
        > 0;
    if inserted {
        ctx.publish(Event::Alert {
            alert: op_core::alert::Alert {
                id: alert_id,
                kind: "auto_shutoff".into(),
                key,
                severity: op_core::Severity::Critical,
                status: op_core::alert::AlertStatus::Open,
                herd_id: Some(collar.herd_id.clone()),
                title,
                body: None,
                at: point,
                targets: vec![("collar".into(), collar.id.clone())],
                data,
                opened_at: at,
                updated_at: at,
                acked_at: None,
                acked_by: None,
                resolved_at: None,
                resolved_by: None,
                rolled_into: None,
            },
        });
    }
    Ok(())
}

async fn notify_agent_rearm(ctx: &Ctx, collar: &Collar, reason: &str, reason_code: &str, who: &Value, at: DateTime<Utc>) -> anyhow::Result<()> {
    let key = format!("agent_rearm:{}:{}", collar.id, to_db(&at));
    let alert_id = id::new_id("alr");
    let title = format!("{} re-armed by an agent", collar.name);
    let data = json!({ "label": &collar.name, "reason": reason, "reason_code": reason_code, "by": who, "collar_id": &collar.id });
    let targets = json!([["collar", &collar.id]]);
    sqlx::query(
        "INSERT INTO alerts (id, kind, key, severity, status, herd_id, title, body, targets, data, opened_at, updated_at, seen_at, notify, batch_at)
         VALUES (?, 'agent_rearm', ?, 'warning', 'open', ?, ?, NULL, ?, ?, ?, ?, ?, 1, ?)
         ON CONFLICT DO NOTHING",
    )
    .bind(&alert_id)
    .bind(&key)
    .bind(&collar.herd_id)
    .bind(&title)
    .bind(targets.to_string())
    .bind(data.to_string())
    .bind(to_db(&at))
    .bind(to_db(&at))
    .bind(to_db(&at))
    .bind(to_db(&at))
    .execute(ctx.db())
    .await?;
    Ok(())
}

pub async fn enrich_one(db: &sqlx::SqlitePool, mut collar: Collar) -> anyhow::Result<Collar> {
    expire_on_pool(db, Some(&collar.id), now()).await?;
    collar.rearm = latest_status(db, &collar.id).await?;
    collar.rearm_available = collar.caps.iter().any(|c| c == caps::REARM);
    Ok(collar)
}

pub async fn enrich_many(db: &sqlx::SqlitePool, mut collars: Vec<Collar>) -> anyhow::Result<Vec<Collar>> {
    expire_on_pool(db, None, now()).await?;
    for c in &mut collars {
        c.rearm = latest_status(db, &c.id).await?;
        c.rearm_available = c.caps.iter().any(|cap| cap == caps::REARM);
    }
    Ok(collars)
}

async fn expire_on_pool(db: &sqlx::SqlitePool, collar_id: Option<&str>, at: DateTime<Utc>) -> anyhow::Result<()> {
    let mut tx = op_core::store::begin_immediate(db).await?;
    expire_pending(&mut tx, collar_id, at).await?;
    tx.commit().await?;
    Ok(())
}

async fn latest_status(db: &sqlx::SqlitePool, collar_id: &str) -> anyhow::Result<Option<CollarRearm>> {
    let row = sqlx::query(
        "SELECT rearm_seq, status, issued_at, expires_at, ack_at, ack_status, ack_code, reason_code, actor
         FROM collar_rearms WHERE collar_id = ? ORDER BY rearm_seq DESC LIMIT 1",
    )
    .bind(collar_id)
    .fetch_optional(db)
    .await?;
    let Some(r) = row else { return Ok(None) };
    Ok(Some(CollarRearm {
        status: r.try_get("status")?,
        rearm_seq: r.try_get::<i64, _>("rearm_seq")? as u32,
        issued_at: from_db(&r.try_get::<String, _>("issued_at")?)?,
        expires_at: from_db(&r.try_get::<String, _>("expires_at")?)?,
        ack_at: opt_from_db(r.try_get("ack_at")?)?,
        ack_status: r.try_get("ack_status")?,
        ack_code: r.try_get("ack_code")?,
        reason_code: r.try_get("reason_code")?,
        by: serde_json::from_str(&r.try_get::<String, _>("actor")?).unwrap_or(Value::Null),
    }))
}

async fn latest_view(db: &sqlx::SqlitePool, collar_id: &str) -> anyhow::Result<Option<RearmView>> {
    let row = sqlx::query("SELECT * FROM collar_rearms WHERE collar_id = ? ORDER BY rearm_seq DESC LIMIT 1").bind(collar_id).fetch_optional(db).await?;
    let Some(r) = row else { return Ok(None) };
    Ok(Some(RearmView {
        id: r.try_get("id")?,
        collar_id: r.try_get("collar_id")?,
        herd_id: r.try_get("herd_id")?,
        rearm_seq: r.try_get::<i64, _>("rearm_seq")? as u32,
        reason: r.try_get("reason")?,
        reason_code: r.try_get("reason_code")?,
        shutoff_at: from_db(&r.try_get::<String, _>("shutoff_at")?)?,
        issued_at: from_db(&r.try_get::<String, _>("issued_at")?)?,
        expires_at: from_db(&r.try_get::<String, _>("expires_at")?)?,
        status: r.try_get("status")?,
        ack_status: r.try_get("ack_status")?,
        ack_code: r.try_get("ack_code")?,
        ack_at: opt_from_db(r.try_get("ack_at")?)?,
        by: serde_json::from_str(&r.try_get::<String, _>("actor")?).unwrap_or(Value::Null),
    }))
}

pub fn tool_specs() -> Vec<op_core::tools::ToolSpec> {
    use op_core::tools::{ToolCall, ToolSpec};
    vec![ToolSpec {
        name: "rearm_collar",
        description: "Queue a signed re-arm for a collar that has auto shut-off. Requires Manager or above. reason is required free text (audit only); reason_code is a short wire enum (animal_back_in, fence_fixed, false_trigger, other).",
        input_schema: json!({
            "type": "object",
            "properties": {
                "collar_id": { "type": "string", "description": "Collar id." },
                "reason": { "type": "string", "description": "Why the collar is being re-armed (required, stored on the server)." },
                "reason_code": { "type": "string", "description": "Wire reason: animal_back_in, fence_fixed, false_trigger, or other (default other)." }
            },
            "required": ["collar_id", "reason"],
            "additionalProperties": false
        }),
        read: false,
        brain: false,
        min_role: Role::Manager,
        run: ToolSpec::run_fn(|c: ToolCall| async move {
            #[derive(Deserialize)]
            struct Args {
                collar_id: String,
                reason: String,
                reason_code: Option<String>,
            }
            let args: Args = serde_json::from_value(if c.args.is_null() { json!({}) } else { c.args }).map_err(|e| ApiError::bad_request(e.to_string()))?;
            let collar = queue(&c.ctx, &args.collar_id, &args.reason, args.reason_code.as_deref(), &c.identity, None, true).await?;
            Ok(json!({ "collar": collar }))
        }),
    }]
}
