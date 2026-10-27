//! Factory stations: the PC beside the fixture. A station token (`ops_…`)
//! can register units, fetch test plans, post test runs, provision and draw
//! labels, nothing else. The owner creates and revokes them; only the hash is
//! kept.
//!
//! op-server's guard hands any `ops_…` bearer to [`admit`], which returns
//! the station's [`Identity`] (manager role, the station's id and name) or
//! refuses the request.

use axum::extract::{Path, State};
use axum::http::{Method, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use op_core::time::{now, opt_from_db, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Identity, Role, Via, id};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use crate::who;

/// Every station token starts with this.
pub const TOKEN_PREFIX: &str = "ops_";
const ID_PREFIX: &str = "stn";

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/fleet/stations", get(list).post(create)).route("/api/fleet/stations/{id}/revoke", post(revoke))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Station {
    pub id: String,
    pub name: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
}

fn from_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Station> {
    Ok(Station {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        created_at: op_core::time::from_db(&r.try_get::<String, _>("created_at")?)?,
        last_used_at: opt_from_db(r.try_get("last_used_at")?)?,
        revoked_at: opt_from_db(r.try_get("revoked_at")?)?,
    })
}

/// The station behind a request, if it came with a station token.
pub fn is_station(identity: &Identity) -> bool {
    identity.user_id.as_deref().is_some_and(|u| u.starts_with("stn_"))
}

/// The station's name, for test runs and units it touches.
pub fn station_name(identity: &Identity) -> Option<String> {
    if is_station(identity) { identity.name.clone() } else { None }
}

/// What a station token may do: (method, path pattern), `{x}` one segment.
const ALLOWED: [(&str, &str); 5] = [
    ("POST", "/api/fleet/units"),
    ("GET", "/api/fleet/test-plans/{class}"),
    ("POST", "/api/fleet/units/{serial}/tests"),
    ("POST", "/api/fleet/units/{serial}/provision"),
    ("GET", "/api/fleet/units/{serial}/label.svg"),
];

fn matches(pattern: &str, path: &str) -> bool {
    let (mut want, mut got) = (pattern.split('/'), path.split('/'));
    loop {
        match (want.next(), got.next()) {
            (None, None) => return true,
            (Some(w), Some(g)) if w.starts_with('{') => {
                if g.is_empty() {
                    return false;
                }
            }
            (Some(w), Some(g)) if w == g => {}
            _ => return false,
        }
    }
}

/// Whether a station token may make this request.
pub fn allowed(method: &Method, path: &str) -> bool {
    let method = if method == Method::HEAD { "GET" } else { method.as_str() };
    ALLOWED.iter().any(|(m, p)| *m == method && matches(p, path))
}

fn hash(token: &str) -> String {
    op_core::keys::hash_key(token)
}

/// The identity for a station token: 401 when it is unknown or revoked, 403
/// for a route stations can't use.
pub async fn admit(ctx: &Ctx, token: &str, method: &Method, path: &str) -> Result<Identity, ApiError> {
    let row = sqlx::query("SELECT * FROM fleet_stations WHERE token_hash = ?").bind(hash(token)).fetch_optional(ctx.db()).await?;
    let station = match row.map(|r| from_row(&r)).transpose()? {
        Some(s) if s.revoked_at.is_none() => s,
        _ => return Err(ApiError::unauthorized("This station token no longer works. Ask the owner for a new one.")),
    };
    if !allowed(method, path) {
        return Err(ApiError::forbidden("A station can only register units, fetch test plans, post tests, provision and print labels."));
    }
    // Last use to the minute is enough, and keeps a busy station from writing on every request.
    let at = now();
    if station.last_used_at.is_none_or(|t| at - t > Duration::seconds(60)) {
        sqlx::query("UPDATE fleet_stations SET last_used_at = ? WHERE id = ?").bind(to_db(&at)).bind(&station.id).execute(ctx.db()).await?;
    }
    Ok(Identity { role: Role::Manager, user_id: Some(station.id), name: Some(station.name), via: Via::UserToken })
}

async fn list(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<Station>>> {
    let rows = sqlx::query("SELECT * FROM fleet_stations ORDER BY revoked_at IS NOT NULL, created_at DESC").fetch_all(ctx.db()).await?;
    Ok(Json(rows.iter().map(from_row).collect::<anyhow::Result<_>>()?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewStation {
    name: String,
}

#[derive(Serialize)]
struct Created {
    station: Station,
    /// Shown once.
    token: String,
}

fn new_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    format!("{TOKEN_PREFIX}{}", bytes.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

async fn create(State(ctx): State<Ctx>, identity: Identity, ApiJson(b): ApiJson<NewStation>) -> ApiResult<(StatusCode, Json<Created>)> {
    let name = b.name.trim();
    if name.is_empty() || name.len() > 64 || name.chars().any(char::is_control) {
        return Err(ApiError::bad_request("A station needs a name of 1–64 characters."));
    }
    let taken: Option<String> =
        sqlx::query_scalar("SELECT id FROM fleet_stations WHERE name = ? AND revoked_at IS NULL").bind(name).fetch_optional(ctx.db()).await?;
    if taken.is_some() {
        return Err(ApiError::conflict(format!("There is already a station called {name}.")));
    }
    let id = id::new_id(ID_PREFIX);
    let token = new_token();
    sqlx::query("INSERT INTO fleet_stations (id, name, token_hash, created_at, created_by) VALUES (?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(name)
        .bind(hash(&token))
        .bind(to_db(&now()))
        .bind(who(&identity))
        .execute(ctx.db())
        .await?;
    tracing::info!(station = %id, name, "station token created");
    let station = from_row(&sqlx::query("SELECT * FROM fleet_stations WHERE id = ?").bind(&id).fetch_one(ctx.db()).await?)?;
    Ok((StatusCode::CREATED, Json(Created { station, token })))
}

async fn revoke(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<Station>> {
    let n = sqlx::query("UPDATE fleet_stations SET revoked_at = COALESCE(revoked_at, ?) WHERE id = ?")
        .bind(to_db(&now()))
        .bind(&id)
        .execute(ctx.db())
        .await?
        .rows_affected();
    if n == 0 {
        return Err(ApiError::not_found("No such station."));
    }
    tracing::info!(station = %id, "station token revoked");
    Ok(Json(from_row(&sqlx::query("SELECT * FROM fleet_stations WHERE id = ?").bind(&id).fetch_one(ctx.db()).await?)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stations_reach_only_their_five_routes() {
        let post = Method::POST;
        let get = Method::GET;
        assert!(allowed(&post, "/api/fleet/units"));
        assert!(allowed(&get, "/api/fleet/test-plans/node"));
        assert!(allowed(&Method::HEAD, "/api/fleet/test-plans/node"));
        assert!(allowed(&post, "/api/fleet/units/OC1-1/tests"));
        assert!(allowed(&post, "/api/fleet/units/OC1-1/provision"));
        assert!(allowed(&get, "/api/fleet/units/OC1-1/label.svg"));
        for (m, p) in [
            (&get, "/api/fleet/units"),
            (&get, "/api/fleet/units/OC1-1"),
            (&post, "/api/fleet/units/OC1-1/claim"),
            (&post, "/api/fleet/units/OC1-1/state"),
            (&Method::PUT, "/api/fleet/test-plans/node"),
            (&get, "/api/fleet/stations"),
            (&post, "/api/fleet/units//tests"),
            (&get, "/api/state"),
            (&post, "/api/collars"),
            (&post, "/api/fleet/units/OC1-1/tests/x"),
        ] {
            assert!(!allowed(m, p), "{m} {p}");
        }
    }

    #[test]
    fn tokens_are_prefixed_and_random() {
        let (a, b) = (new_token(), new_token());
        assert!(a.starts_with(TOKEN_PREFIX) && a.len() == TOKEN_PREFIX.len() + 48);
        assert_ne!(a, b);
    }
}
