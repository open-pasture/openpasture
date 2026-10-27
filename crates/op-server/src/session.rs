//! Owner sign-in without trusting where a request comes from.
//!
//! Being on this machine grants nothing by itself: a proxy can make any
//! request look local. Instead the owner signs in with a one-time code that
//! only this machine can hand out: the desktop app opens its window on
//! `#/signin/<code>`, and `openpasture serve` (or `openpasture sign-in`)
//! prints a sign-in URL. The code works once, within [`CODE_MINUTES`], and is
//! traded for an owner session token (`opo_…`, kept only as a hash) at
//! `POST /api/session`. A session unused for [`op_core::people::TOKEN_IDLE_DAYS`]
//! or older than [`op_core::people::TOKEN_MAX_DAYS`] stops working.
//!
//! The app token is never in a read: the owner reveals it on purpose
//! (`POST /api/settings/app-token/reveal`) or replaces it
//! (`POST /api/settings/app-token/rotate`).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use op_core::time::{from_db, now, opt_from_db, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Identity, Role, Via, keys};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::Row;

/// An owner session token starts with this, then 64 hex characters.
pub const SESSION_PREFIX: &str = "opo_";
/// How long a sign-in code works.
pub const CODE_MINUTES: i64 = 10;
/// Resolved sessions are kept this long before the database is asked again.
const CACHE_TTL: Duration = Duration::from_secs(30);
/// `owner_sessions.last_used` is written at most this often.
const LAST_USED_EVERY: chrono::Duration = chrono::Duration::minutes(1);

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/session", post(post_session).delete(delete_session))
        .route("/api/settings/app-token/reveal", post(reveal))
        .route("/api/settings/app-token/rotate", post(rotate))
}

fn code() -> String {
    keys::new_collar_key()[..32].to_owned()
}

/// A fresh one-time code for this machine's owner, stored in `pool`.
async fn store_code(pool: &sqlx::SqlitePool, label: &str) -> anyhow::Result<String> {
    let c = code();
    let at = now();
    sqlx::query("DELETE FROM signin_codes WHERE expires_at < ? OR used_at IS NOT NULL").bind(to_db(&at)).execute(pool).await?;
    sqlx::query("INSERT INTO signin_codes (code_hash, label, created_at, expires_at) VALUES (?, ?, ?, ?)")
        .bind(keys::hash_key(&c))
        .bind(label)
        .bind(to_db(&at))
        .bind(to_db(&(at + chrono::Duration::minutes(CODE_MINUTES))))
        .execute(pool)
        .await?;
    Ok(c)
}

fn url_for(base: &str, code: &str) -> String {
    format!("{}/#/signin/{code}", base.trim_end_matches('/'))
}

/// A one-time sign-in URL on `base` for this machine's owner. `label` says
/// what asked for it (`desktop`, `serve`).
pub async fn sign_in_url(ctx: &Ctx, base: &str, label: &str) -> anyhow::Result<String> {
    Ok(url_for(base, &store_code(ctx.db(), label).await?))
}

/// [`sign_in_url`] for a server another process runs from `data_dir`: the
/// code goes straight into its database (whoever can write that is its owner).
pub async fn sign_in_url_in(data_dir: &Path, base: &str, label: &str) -> anyhow::Result<String> {
    use anyhow::Context;
    let path = data_dir.join(op_core::store::DB_FILE);
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&path).busy_timeout(Duration::from_secs(10));
    let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.with_context(|| format!("opening {}", path.display()))?;
    let c = store_code(&pool, label).await;
    pool.close().await;
    Ok(url_for(base, &c?))
}

#[derive(Deserialize)]
struct CodeBody {
    code: String,
}

#[derive(Serialize)]
struct NewSession {
    token: String,
}

/// Trade a one-time code for an owner session (no sign-in needed: the code is it).
async fn post_session(State(ctx): State<Ctx>, ApiJson(b): ApiJson<CodeBody>) -> ApiResult<(StatusCode, Json<NewSession>)> {
    let at = now();
    let label: Option<String> =
        sqlx::query_scalar("UPDATE signin_codes SET used_at = ? WHERE code_hash = ? AND used_at IS NULL AND expires_at > ? RETURNING label")
            .bind(to_db(&at))
            .bind(keys::hash_key(b.code.trim()))
            .bind(to_db(&at))
            .fetch_optional(ctx.db())
            .await?;
    let Some(label) = label else {
        return Err(ApiError::unauthorized("This sign-in link has been used or has run out. Open the app again, or run openpasture sign-in."));
    };
    let token = format!("{SESSION_PREFIX}{}", keys::new_collar_key());
    sqlx::query("INSERT INTO owner_sessions (id, token_hash, label, created_at, last_used) VALUES (?, ?, ?, ?, ?)")
        .bind(op_core::id::new_id("ses"))
        .bind(keys::hash_key(&token))
        .bind(&label)
        .bind(to_db(&at))
        .bind(to_db(&at))
        .execute(ctx.db())
        .await?;
    tracing::info!(%label, "owner signed in with a one-time code");
    Ok((StatusCode::CREATED, Json(NewSession { token })))
}

/// Sign this owner session out.
async fn delete_session(State(ctx): State<Ctx>, identity: Identity, session: Option<axum::Extension<OwnerSession>>) -> ApiResult<StatusCode> {
    identity.require(Role::Viewer)?;
    let Some(axum::Extension(OwnerSession(id))) = session else {
        return Err(ApiError::bad_request("This isn't an owner session."));
    };
    sqlx::query("UPDATE owner_sessions SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL").bind(to_db(&now())).bind(&id).execute(ctx.db()).await?;
    CACHE.lock().unwrap_or_else(|e| e.into_inner()).retain(|_, c| c.id != id);
    Ok(StatusCode::NO_CONTENT)
}

/// The owner session a request signed in with, put in its extensions by the guard.
#[derive(Debug, Clone)]
pub struct OwnerSession(pub String);

struct Cached {
    id: String,
    at: Instant,
    data_dir: std::path::PathBuf,
}

static CACHE: LazyLock<Mutex<HashMap<String, Cached>>> = LazyLock::new(Default::default);

/// The owner session an `ops_` token signs in: its id, or `None` for an
/// unknown, revoked or expired one.
pub async fn session_for_token(ctx: &Ctx, token: &str) -> anyhow::Result<Option<String>> {
    let rest = token.strip_prefix(SESSION_PREFIX).unwrap_or_default();
    if rest.len() != 64 || !rest.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Ok(None);
    }
    let hash = keys::hash_key(token);
    if let Some(c) = CACHE.lock().unwrap_or_else(|e| e.into_inner()).get(&hash).filter(|c| c.at.elapsed() < CACHE_TTL && c.data_dir == ctx.data_dir()) {
        return Ok(Some(c.id.clone()));
    }
    let row = sqlx::query("SELECT id, created_at, last_used FROM owner_sessions WHERE token_hash = ? AND revoked_at IS NULL")
        .bind(&hash)
        .fetch_optional(ctx.db())
        .await?;
    let Some(row) = row else { return Ok(None) };
    let (id, created, used): (String, String, Option<String>) = (row.try_get("id")?, row.try_get("created_at")?, row.try_get("last_used")?);
    let (created, used) = (from_db(&created)?, opt_from_db(used)?);
    let at = now();
    if op_core::people::token_expired(created, used, at) {
        return Ok(None);
    }
    if used.is_none_or(|u| at - u >= LAST_USED_EVERY) {
        sqlx::query("UPDATE owner_sessions SET last_used = ? WHERE id = ?").bind(to_db(&at)).bind(&id).execute(ctx.db()).await?;
    }
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    cache.retain(|_, c| c.at.elapsed() < CACHE_TTL);
    cache.insert(hash, Cached { id: id.clone(), at: Instant::now(), data_dir: ctx.data_dir().to_path_buf() });
    Ok(Some(id))
}

/// An owner signed in this way, as a person when the owner is in People.
pub async fn owner_identity(ctx: &Ctx) -> Identity {
    op_core::people::with_owner_person(ctx, Identity::owner(Via::Local)).await
}

async fn reveal(State(ctx): State<Ctx>, identity: Identity) -> ApiResult<Json<serde_json::Value>> {
    identity.require(Role::Owner)?;
    tracing::info!(by = ?identity.actor(), "app token revealed");
    Ok(Json(json!({ "app_token": ctx.settings().await?.server.app_token })))
}

/// A new app token; the old one stops working at once (clients that use it need the new one).
async fn rotate(State(ctx): State<Ctx>, identity: Identity) -> ApiResult<Json<serde_json::Value>> {
    identity.require(Role::Owner)?;
    let token = op_core::settings::new_token();
    ctx.update_settings(&json!({ "server": { "app_token": token } })).await?;
    tracing::info!(by = ?identity.actor(), "app token replaced");
    Ok(Json(json!({ "app_token": token })))
}
