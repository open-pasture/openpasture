//! App routes for collars and latest positions.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use op_core::{Animal, ApiError, ApiJson, ApiResult, Collar, Ctx, Event, FenceState, Position, id, keys, patch};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db;

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/collars", get(list).post(create))
        .route("/api/collars/{id}", get(get_one).patch(update).delete(remove))
        .route("/api/positions", get(positions))
}

#[derive(Deserialize)]
struct HerdQuery {
    herd_id: Option<String>,
}

async fn list(State(ctx): State<Ctx>, Query(q): Query<HerdQuery>) -> ApiResult<Json<Vec<Collar>>> {
    Ok(Json(db::list_collars(ctx.db(), q.herd_id.as_deref().filter(|h| !h.is_empty())).await?))
}

async fn get_one(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<Collar>> {
    Ok(Json(find(&ctx, &id).await?))
}

async fn find(ctx: &Ctx, id: &str) -> ApiResult<Collar> {
    db::get_collar(ctx.db(), id).await?.ok_or_else(|| ApiError::not_found("No such collar."))
}

async fn require_herd(ctx: &Ctx, herd_id: &str) -> ApiResult<()> {
    match ctx.store().get_herd(herd_id).await? {
        Some(_) => Ok(()),
        None => Err(ApiError::bad_request("No such herd.")),
    }
}

#[derive(Deserialize)]
struct NewCollar {
    name: Option<String>,
    herd_id: String,
}

#[derive(Serialize)]
struct Linked {
    collar: Collar,
    /// Shown once. Only its hash is stored.
    key: String,
    endpoint: String,
    public_key: String,
}

/// Where collars talk to this server: `{base_url}/collar/v1`.
fn endpoint(ctx: &Ctx) -> String {
    format!("{}/collar/v1", ctx.base_url())
}

fn clean_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("The collar needs a name."));
    }
    if name.len() > 200 {
        return Err(ApiError::bad_request("The collar name is too long."));
    }
    Ok(name.to_owned())
}

/// Link a collar: a fresh key, returned once with the endpoint and the
/// server's public key for checking boundary signatures.
async fn create(State(ctx): State<Ctx>, ApiJson(body): ApiJson<NewCollar>) -> ApiResult<(StatusCode, Json<Linked>)> {
    require_herd(&ctx, &body.herd_id).await?;
    let name = match body.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => clean_name(n)?,
        None => format!("Collar {}", db::list_collars(ctx.db(), Some(&body.herd_id)).await?.len() + 1),
    };
    let collar_id = id::new_id(id::COLLAR);
    let key = keys::new_collar_key();
    db::insert_collar(ctx.db(), &collar_id, &name, &body.herd_id, &keys::hash_key(&key)).await?;
    let collar = find(&ctx, &collar_id).await?;
    tracing::info!(collar = %collar.id, "collar linked");
    ctx.publish(Event::Collar { collar: collar.clone() });
    Ok((StatusCode::CREATED, Json(Linked { collar, key, endpoint: endpoint(&ctx), public_key: ctx.public_key_b64() })))
}

/// PATCH writes only what was sent (`name`, `herd_id`, `animal_id`), so a
/// report landing at the same time keeps its telemetry.
async fn update(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<Collar>> {
    let current = find(&ctx, &id).await?;
    let immutable = ["id", "last_seen", "battery", "boundary_version", "state", "last_fix", "fw", "caps", "outside_since", "parked_at", "parked_reason"];
    let mut next: Collar = patch::apply(&current, &body, &immutable)?;
    next.name = clean_name(&next.name)?;
    if next.herd_id != current.herd_id {
        require_herd(&ctx, &next.herd_id).await?;
    }
    if next.animal_id != current.animal_id {
        link_animal(&ctx, &current, next.animal_id.as_deref()).await?;
    }
    if next.name != current.name {
        sqlx::query("UPDATE collars SET name = ? WHERE id = ?").bind(&next.name).bind(&id).execute(ctx.db()).await?;
    }
    if next.herd_id != current.herd_id {
        // A new herd means a new fence: the collar starts over.
        sqlx::query("UPDATE collars SET herd_id = ?, boundary_version = NULL, state = ?, outside_since = NULL WHERE id = ?")
            .bind(&next.herd_id)
            .bind(FenceState::Unknown.as_str())
            .bind(&id)
            .execute(ctx.db())
            .await?;
        // The collar still holds its old herd's version; make sure the new
        // herd's boundary is newer than that.
        if let Some(held) = current.boundary_version {
            crate::boundary::reissue_for_moved_collar(&ctx, &next.herd_id, held).await?;
        }
    }
    let next = find(&ctx, &id).await?;
    ctx.publish(Event::Collar { collar: next.clone() });
    Ok(Json(next))
}

/// Put the collar on an animal (or take it off with `None`), through the
/// store so `animals.collar_id` and `collars.animal_id` stay in step.
async fn link_animal(ctx: &Ctx, collar: &Collar, animal_id: Option<&str>) -> ApiResult<()> {
    let store = ctx.store();
    let animal = match animal_id {
        Some(aid) => Some(store.get_animal(aid).await?.ok_or_else(|| ApiError::bad_request("No such animal."))?),
        None => None,
    };
    if let Some(old) = store.animal_for_collar(&collar.id).await? {
        store.update_animal(&Animal { collar_id: None, ..old }).await?;
    }
    if let Some(animal) = animal {
        store.update_animal(&Animal { collar_id: Some(collar.id.clone()), ..animal }).await?;
    }
    Ok(())
}

async fn remove(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    if !db::delete_collar(ctx.db(), &id).await? {
        return Err(ApiError::not_found("No such collar."));
    }
    tracing::info!(collar = %id, "collar removed");
    Ok(StatusCode::NO_CONTENT)
}

async fn positions(State(ctx): State<Ctx>, Query(q): Query<HerdQuery>) -> ApiResult<Json<Vec<Position>>> {
    let collars = db::list_collars(ctx.db(), q.herd_id.as_deref().filter(|h| !h.is_empty())).await?;
    Ok(Json(to_positions(collars)))
}

fn to_positions(collars: Vec<Collar>) -> Vec<Position> {
    collars.into_iter().filter_map(|c| c.last_fix.map(|fix| Position { collar_id: c.id, animal_id: c.animal_id, fix, state: c.state })).collect()
}

/// The latest fix per collar in the herd, with its fence state.
pub async fn latest_positions(ctx: &Ctx, herd_id: &str) -> anyhow::Result<Vec<Position>> {
    Ok(to_positions(db::list_collars(ctx.db(), Some(herd_id)).await?))
}
