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
        hand_over(&ctx, &id, current.boundary_version, current.last_seen.is_some()).await?;
        // Its signed config names the new herd, so it accepts that herd's boundaries.
        crate::config::refresh_quietly(&ctx, crate::config::Scope::Collar(&id)).await;
    }
    let next = find(&ctx, &id).await?;
    ctx.publish(Event::Collar { collar: next.clone() });
    Ok(Json(next))
}

/// A collar just moved to another herd still holds its old herd's versions,
/// applied and staged (the old herd's strips would open on it at their
/// times), and asks only for versions above the highest. When the new herd's
/// boundaries sit at or below that, or what it holds isn't known (it has been
/// heard from, but nothing of what it holds reached us), it gets copies of
/// them above everything it holds; the one in effect drops the rest.
async fn hand_over(ctx: &Ctx, collar_id: &str, applied: Option<u32>, heard: bool) -> ApiResult<()> {
    let Some(collar) = db::get_collar(ctx.db(), collar_id).await? else { return Ok(()) };
    let at = op_core::time::now();
    let set = crate::escapes::collar_boundaries(ctx.db(), &collar, at).await?;
    if set.active.is_none() && set.staged.is_empty() {
        return Ok(());
    }
    let mut holds: Vec<u32> = sqlx::query_scalar::<_, i64>("SELECT version FROM collar_slots WHERE collar_id = ? AND status != 'rejected'")
        .bind(collar_id)
        .fetch_all(ctx.db())
        .await?
        .into_iter()
        .map(|v| v as u32)
        .collect();
    holds.extend(applied);
    let unknown = holds.is_empty() && heard;
    // When everything the new herd has sits above what it holds, it asks for those.
    let below = holds.iter().max().is_some_and(|&top| set.active.iter().chain(&set.staged).any(|b| b.version <= top));
    if !(unknown || below) {
        return Ok(());
    }
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let copies = crate::escapes::hand_copies(&mut tx, &collar, at).await?;
    tx.commit().await?;
    tracing::info!(collar = %collar_id, herd = %collar.herd_id, holds = ?holds, copies = ?copies.iter().map(|b| b.version).collect::<Vec<_>>(), "moved collar: its new herd's boundaries as its own copies");
    Ok(())
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

// K-animals: collars on animals, keys and parking. The park and unpark routes
// live here; op-import calls the rest (bulk linking, swap, remove, rekey).

/// `POST /api/collars/{id}/park` and `/unpark`.
pub fn lifecycle_router() -> Router<Ctx> {
    Router::new().route("/api/collars/{id}/park", axum::routing::post(post_park)).route("/api/collars/{id}/unpark", axum::routing::post(post_unpark))
}

#[derive(Deserialize)]
struct ParkBody {
    reason: op_core::ParkReason,
}

async fn post_park(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(body): ApiJson<ParkBody>) -> ApiResult<Json<Collar>> {
    Ok(Json(park_collar(&ctx, &id, body.reason).await?))
}

async fn post_unpark(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<Collar>> {
    Ok(Json(unpark_collar(&ctx, &id).await?))
}

/// Where collars talk to this server: `{base_url}/collar/v1`.
pub fn collar_endpoint(ctx: &Ctx) -> String {
    endpoint(ctx)
}

/// Take a collar off duty: charging, on the shelf, in repair. It raises no
/// alerts, isn't drawn or counted, and its reports keep only battery and
/// health, so its fence state is forgotten and an open escape for it stops.
/// Parking a parked collar changes only the reason.
pub async fn park_collar(ctx: &Ctx, id: &str, reason: op_core::ParkReason) -> ApiResult<Collar> {
    find(ctx, id).await?;
    match crate::escapes::stop_escape(ctx, id).await {
        Ok(_) => {}
        Err(e) if e.status == StatusCode::CONFLICT => {}
        Err(e) => return Err(e),
    }
    sqlx::query("UPDATE collars SET parked_at = COALESCE(parked_at, ?), parked_reason = ?, state = ?, outside_since = NULL WHERE id = ?")
        .bind(op_core::time::to_db(&op_core::time::now()))
        .bind(op_core::DbEnum::as_db(&reason))
        .bind(FenceState::Unknown.as_str())
        .bind(id)
        .execute(ctx.db())
        .await?;
    let c = find(ctx, id).await?;
    tracing::info!(collar = %id, reason = ?reason, "collar parked");
    ctx.publish(Event::Collar { collar: c.clone() });
    Ok(c)
}

/// Back on duty: its next report counts again.
pub async fn unpark_collar(ctx: &Ctx, id: &str) -> ApiResult<Collar> {
    find(ctx, id).await?;
    sqlx::query("UPDATE collars SET parked_at = NULL, parked_reason = NULL WHERE id = ?").bind(id).execute(ctx.db()).await?;
    let c = find(ctx, id).await?;
    ctx.publish(Event::Collar { collar: c.clone() });
    Ok(c)
}

/// Put a collar on an animal of its own herd, or take it off with `None`.
/// `animals.collar_id` and `collars.animal_id` stay in step, and a parked
/// collar put on an animal is back on duty.
pub async fn link_collar(ctx: &Ctx, collar_id: &str, animal_id: Option<&str>) -> ApiResult<Collar> {
    let collar = find(ctx, collar_id).await?;
    if let Some(aid) = animal_id {
        let a = ctx.store().get_animal(aid).await?.ok_or_else(|| ApiError::bad_request("No such animal."))?;
        if a.herd_id != collar.herd_id {
            return Err(ApiError::bad_request(format!("{} is in another herd.", collar.name)));
        }
        if a.removed_at.is_some() {
            return Err(ApiError::bad_request(format!("{} is no longer on the farm.", a.tag)));
        }
    }
    link_animal(ctx, &collar, animal_id).await?;
    if animal_id.is_some() && collar.parked_at.is_some() {
        return unpark_collar(ctx, collar_id).await;
    }
    let c = find(ctx, collar_id).await?;
    ctx.publish(Event::Collar { collar: c.clone() });
    Ok(c)
}

/// A new key for the collar, returned once. The old key stops working at once,
/// so the collar reports again only after it is set up with the new one.
pub async fn rekey_collar(ctx: &Ctx, id: &str) -> ApiResult<(Collar, String)> {
    find(ctx, id).await?;
    let key = keys::new_collar_key();
    sqlx::query("UPDATE collars SET key_hash = ? WHERE id = ?").bind(keys::hash_key(&key)).bind(id).execute(ctx.db()).await?;
    tracing::info!(collar = %id, "collar rekeyed");
    Ok((find(ctx, id).await?, key))
}

/// Whether `key` is the collar's current key.
pub async fn collar_key_matches(ctx: &Ctx, id: &str, key: &str) -> anyhow::Result<bool> {
    let row: Option<(Option<String>,)> = sqlx::query_as("SELECT key_hash FROM collars WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?;
    Ok(matches!(row, Some((Some(h),)) if h == keys::hash_key(key)))
}

/// One collar to create by [`create_linked_collars`].
pub struct NewLinked {
    pub name: String,
    /// An active animal of the herd with no collar yet.
    pub animal_id: Option<String>,
}

/// New collars in a herd, each with a fresh key (returned once, in order) and
/// put on its animal when one is named. All or none: one write transaction.
pub async fn create_linked_collars(ctx: &Ctx, herd_id: &str, items: &[NewLinked]) -> ApiResult<Vec<(Collar, String)>> {
    require_herd(ctx, herd_id).await?;
    let mut made = Vec::with_capacity(items.len());
    for it in items {
        made.push((id::new_id(id::COLLAR), clean_name(&it.name)?, keys::new_collar_key(), it.animal_id.clone()));
    }
    let at = op_core::time::to_db(&op_core::time::now());
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    for (cid, name, key, animal) in &made {
        sqlx::query("INSERT INTO collars (id, name, herd_id, key_hash, state, created_at, animal_id) VALUES (?, ?, ?, ?, 'unknown', ?, ?)")
            .bind(cid)
            .bind(name)
            .bind(herd_id)
            .bind(keys::hash_key(key))
            .bind(&at)
            .bind(animal)
            .execute(&mut *tx)
            .await?;
        if let Some(aid) = animal {
            let n = sqlx::query("UPDATE animals SET collar_id = ? WHERE id = ? AND herd_id = ? AND collar_id IS NULL AND removed_at IS NULL")
                .bind(cid)
                .bind(aid)
                .bind(herd_id)
                .execute(&mut *tx)
                .await?
                .rows_affected();
            if n != 1 {
                return Err(ApiError::conflict("An animal in the list already wears a collar or left the herd. Nothing was linked."));
            }
        }
    }
    tx.commit().await?;
    let mut out = Vec::with_capacity(made.len());
    for (cid, _, key, _) in made {
        let c = find(ctx, &cid).await?;
        ctx.publish(Event::Collar { collar: c.clone() });
        out.push((c, key));
    }
    tracing::info!(herd = %herd_id, count = out.len(), "collars linked in bulk");
    Ok(out)
}
