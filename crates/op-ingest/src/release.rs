//! Cross-farm handover: owner releases a collar with a one-time transfer
//! code. The receiving farm claims with that token (or the card); this farm
//! revokes the v1 key so the collar cannot report here again.

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use chrono::{Duration, Utc};
use op_core::time::{now, to_db};
use op_core::{ActivityEvent, ApiError, ApiJson, ApiResult, Ctx, Event, FenceState, Identity, Role, id, keys};
use op_fleet::transfer_token;
use op_protocol::v2::identity::{CLAIM_CODE_LEN, new_claim_code};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::collars;
use crate::db;

/// Default transfer-code lifetime.
pub const DEFAULT_EXPIRY_HOURS: i64 = 72;
const MAX_EXPIRY_HOURS: i64 = 24 * 30;

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/collars/{id}/release", post(release))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReleaseBody {
    /// Hours until the transfer code expires (default 72, max 720).
    #[serde(default)]
    expires_hours: Option<i64>,
}

#[derive(Serialize)]
pub struct Released {
    pub collar_id: String,
    /// Portable token for the receiving farm: `OPT1:…` (shown once).
    pub transfer_token: String,
    /// The 16-character code alone (also inside the token).
    pub transfer_code: String,
    pub expires_at: chrono::DateTime<Utc>,
    /// History export while the record still lives here.
    pub export_path: String,
}

fn code_hash(code: &str) -> String {
    format!("{:x}", Sha256::digest(code.as_bytes()))
}

async fn release(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>, ApiJson(b): ApiJson<ReleaseBody>) -> ApiResult<Json<Released>> {
    identity.require(Role::Owner)?;
    let hours = b.expires_hours.unwrap_or(DEFAULT_EXPIRY_HOURS);
    if !(1..=MAX_EXPIRY_HOURS).contains(&hours) {
        return Err(ApiError::bad_request(format!("expires_hours is 1–{MAX_EXPIRY_HOURS}.")));
    }
    let caller = identity.user_id.clone().unwrap_or_else(|| format!("{:?}", identity.via));
    let target = format!("release:{caller}");
    op_fleet::claim_limit::may_try(&ctx, &caller, None, &target).await?;

    let collar = db::get_collar(ctx.db(), &id).await?.ok_or_else(|| ApiError::not_found("No such collar."))?;
    if collar.released_at.is_some() {
        return Err(ApiError::conflict("This collar is already released for handover."));
    }
    if collar.simulated {
        return Err(ApiError::conflict(op_core::sim::WRONG_SIDE));
    }

    let serial: Option<String> = sqlx::query_scalar("SELECT serial FROM fleet_units WHERE collar_id = ?").bind(&id).fetch_optional(ctx.db()).await?;
    let device_pubkey: Option<String> =
        sqlx::query_scalar("SELECT device_pubkey FROM fleet_units WHERE collar_id = ?").bind(&id).fetch_optional(ctx.db()).await?;
    let (serial, device_pubkey) = match (serial, device_pubkey) {
        (Some(s), Some(p)) => (s, p),
        _ => return Err(ApiError::conflict("Only a collar claimed from a fleet unit can be released for another farm.")),
    };

    match crate::escapes::stop_escape(&ctx, &id).await {
        Ok(_) => {}
        Err(e) if e.status == axum::http::StatusCode::CONFLICT => {}
        Err(e) => return Err(e),
    }
    if collar.animal_id.is_some() {
        collars::link_collar(&ctx, &id, None).await?;
    }

    let code = new_claim_code();
    debug_assert_eq!(code.len(), CLAIM_CODE_LEN);
    let at = now();
    let expires = at + Duration::hours(hours);
    let release_id = id::new_id("rel");
    let transfer_token = transfer_token::format(&serial, &device_pubkey, &id, &code, expires, ctx.signing_key());
    let dead = keys::new_collar_key();

    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    sqlx::query(
        "INSERT INTO collar_releases (id, collar_id, serial, device_pubkey, code_hash, expires_at, created_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&release_id)
    .bind(&id)
    .bind(&serial)
    .bind(&device_pubkey)
    .bind(code_hash(&code))
    .bind(to_db(&expires))
    .bind(to_db(&at))
    .bind(identity.user_id.as_deref().or(identity.name.as_deref()))
    .execute(&mut *tx)
    .await?;
    sqlx::query("UPDATE collars SET key_hash = ?, released_at = ?, boundary_version = NULL, state = ?, outside_since = NULL, animal_id = NULL WHERE id = ?")
        .bind(keys::hash_key(&dead))
        .bind(to_db(&at))
        .bind(FenceState::Unknown.as_str())
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE fleet_units SET state = 'retired', updated_at = ? WHERE collar_id = ? AND state IN ('claimed', 'in_service')")
        .bind(to_db(&at))
        .bind(&id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;

    op_fleet::claim_limit::record(&ctx, &caller, None, &target).await;

    let e = ActivityEvent {
        id: id::new_id(id::EVENT),
        kind: "collar.released".into(),
        source: "farmer".into(),
        occurred_at: at,
        recorded_at: at,
        title: "Collar released for another farm".into(),
        body: Some("The v1 key was revoked. Take the transfer token or the card to the receiving farm.".into()),
        payload: json!({
            "collar_id": id,
            "serial": serial,
            "release_id": release_id,
            "expires_at": expires,
            "by": identity.actor(),
        }),
        targets: vec![("collar".into(), id.clone()), ("unit".into(), serial.clone())],
    };
    let _ = ctx.store().record_event(&e).await;

    let next = db::get_collar(ctx.db(), &id).await?.ok_or_else(|| ApiError::internal("collar vanished"))?;
    ctx.publish(Event::Collar { collar: next });

    Ok(Json(Released { collar_id: id.clone(), transfer_token, transfer_code: code, expires_at: expires, export_path: format!("/api/collars/{id}/export") }))
}
