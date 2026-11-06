//! App routes for collars and latest positions.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::time::{from_db, now, to_db};
use op_core::{
    ActivityEvent, Animal, ApiError, ApiJson, ApiResult, Collar, Ctx, Event, FenceState, Identity, Position, UNDO_WINDOW_HOURS, id, keys, patch, undo_window,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::db;

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/collars", get(list).post(create))
        .route("/api/collars/{id}", get(get_one).patch(update).delete(remove))
        .route("/api/collars/{id}/transfer", post(transfer))
        .route("/api/collars/{id}/undo", post(undo))
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
    /// Linked by the simulator (`simulation/collar-sim`). Only a simulated
    /// herd takes it, and a simulated herd takes nothing else (`op_core::sim`).
    #[serde(default)]
    simulated: bool,
}

#[derive(Serialize)]
struct Linked {
    collar: Collar,
    /// Shown once. Only its hash is stored.
    key: String,
    /// The collar address; absent until one is set, with `no_endpoint` saying why.
    #[serde(skip_serializing_if = "Option::is_none")]
    endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    no_endpoint: Option<&'static str>,
    public_key: String,
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
    if op_core::sim::herd_is_simulated(ctx.db(), &body.herd_id).await? != body.simulated {
        return Err(ApiError::conflict(op_core::sim::WRONG_SIDE));
    }
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
    let endpoint = crate::config::collar_endpoint(&ctx).await?;
    let no_endpoint = endpoint.is_none().then_some(crate::config::NO_COLLAR_ADDRESS);
    Ok((StatusCode::CREATED, Json(Linked { collar, key, endpoint, no_endpoint, public_key: ctx.public_key_b64() })))
}

/// Optional identity: present when op-server / `with_identity` layered it in,
/// absent on bare feature-router tests. Never 500s with "Identity missing".
struct OptIdentity(Option<Identity>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for OptIdentity {
    type Rejection = std::convert::Infallible;
    async fn from_request_parts(parts: &mut axum::http::request::Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.extensions.get::<Identity>().cloned()))
    }
}

/// PATCH writes only what was sent (`name`, `herd_id`, `animal_id`), so a
/// report landing at the same time keeps its telemetry.
async fn update(State(ctx): State<Ctx>, Path(id): Path<String>, OptIdentity(identity): OptIdentity, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<Collar>> {
    let current = find(&ctx, &id).await?;
    let immutable =
        ["id", "last_seen", "battery", "boundary_version", "state", "last_fix", "fw", "caps", "outside_since", "parked_at", "parked_reason", "simulated"];
    let mut next: Collar = patch::apply(&current, &body, &immutable)?;
    next.name = clean_name(&next.name)?;
    if next.animal_id != current.animal_id {
        link_animal(&ctx, &current, next.animal_id.as_deref()).await?;
    }
    if next.name != current.name {
        sqlx::query("UPDATE collars SET name = ? WHERE id = ?").bind(&next.name).bind(&id).execute(ctx.db()).await?;
    }
    if next.herd_id != current.herd_id {
        relocate_collar(&ctx, identity.as_ref(), &current, &next.herd_id, None, false).await?;
    }
    let next = find(&ctx, &id).await?;
    ctx.publish(Event::Collar { collar: next.clone() });
    Ok(Json(next))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TransferBody {
    to_herd_id: String,
    animal_id: Option<String>,
}

/// `POST /api/collars/{id}/transfer`: same-farm herd move. Preserves collar
/// id, key and history. No approval; managers/owners get a notice with undo
/// for [`UNDO_WINDOW_HOURS`] (default pending owner OK).
async fn transfer(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>, ApiJson(b): ApiJson<TransferBody>) -> ApiResult<Json<Collar>> {
    let current = find(&ctx, &id).await?;
    if b.to_herd_id == current.herd_id && b.animal_id == current.animal_id {
        return Ok(Json(current));
    }
    // Outside / cue: stop an open escape so it does not follow the collar.
    match crate::escapes::stop_escape(&ctx, &id).await {
        Ok(_) => {}
        Err(e) if e.status == StatusCode::CONFLICT => {}
        Err(e) => return Err(e),
    }
    if b.to_herd_id != current.herd_id {
        relocate_collar(&ctx, Some(&identity), &current, &b.to_herd_id, b.animal_id.as_deref(), true).await?;
    } else if b.animal_id != current.animal_id {
        let after = find(&ctx, &id).await?;
        link_animal(&ctx, &after, b.animal_id.as_deref()).await?;
    }
    let next = find(&ctx, &id).await?;
    ctx.publish(Event::Collar { collar: next.clone() });
    Ok(Json(next))
}

/// Move a collar to another herd on this farm: clear fence state, hand over
/// boundary copies, refresh config. Preserves collar id, key and history.
/// Shared by PATCH `herd_id` and POST `/transfer`.
async fn relocate_collar(
    ctx: &Ctx,
    identity: Option<&Identity>,
    current: &Collar,
    to_herd_id: &str,
    animal_id: Option<&str>,
    detach_animal: bool,
) -> ApiResult<()> {
    require_herd(ctx, to_herd_id).await?;
    if op_core::sim::herd_is_simulated(ctx.db(), to_herd_id).await? != current.simulated {
        return Err(ApiError::conflict(op_core::sim::WRONG_SIDE));
    }
    if let Some(aid) = animal_id {
        let a = ctx.store().get_animal(aid).await?.ok_or_else(|| ApiError::bad_request("No such animal."))?;
        if a.herd_id != to_herd_id {
            return Err(ApiError::bad_request(format!("{} is in another herd.", a.tag)));
        }
        if a.removed_at.is_some() {
            return Err(ApiError::bad_request(format!("{} is no longer on the farm.", a.tag)));
        }
        if a.collar_id.as_deref().is_some_and(|c| c != current.id) {
            return Err(ApiError::conflict(format!("{} already wears a collar.", a.tag)));
        }
    }
    let from_herd = current.herd_id.clone();
    let from_animal = current.animal_id.clone();
    let from_boundary = current.boundary_version;
    let from_state = current.state;
    let from_outside = current.outside_since;

    if detach_animal || to_herd_id != current.herd_id {
        if current.animal_id.is_some() {
            link_animal(ctx, current, None).await?;
        }
    }

    // A new herd means a new fence: the collar starts over.
    sqlx::query("UPDATE collars SET herd_id = ?, animal_id = NULL, boundary_version = NULL, state = ?, outside_since = NULL WHERE id = ?")
        .bind(to_herd_id)
        .bind(FenceState::Unknown.as_str())
        .bind(&current.id)
        .execute(ctx.db())
        .await?;
    hand_over(ctx, &current.id, current.boundary_version, current.last_seen.is_some()).await?;
    // Its signed config names the new herd, so it accepts that herd's boundaries.
    crate::config::refresh_quietly(ctx, crate::config::Scope::Collar(&current.id)).await;

    if let Some(aid) = animal_id {
        let after = find(ctx, &current.id).await?;
        link_animal(ctx, &after, Some(aid)).await?;
    }

    let undo_id =
        record_move_undo(ctx, identity, current, &from_herd, to_herd_id, from_animal.as_deref(), animal_id, from_boundary, from_state, from_outside).await;
    notice_moved(ctx, identity, &current.id, &from_herd, to_herd_id, animal_id, undo_id).await;
    Ok(())
}

/// A collar just moved to another herd still holds its old herd's versions,
/// applied and staged (the old herd's strips would open on it at their
/// times), and asks only for versions above the highest. When the new herd's
/// boundaries sit at or below that, or what it holds isn't known (it has been
/// heard from, but nothing of what it holds reached us), it gets copies of
/// them above everything it holds; the one in effect drops the rest.
async fn hand_over(ctx: &Ctx, collar_id: &str, applied: Option<u32>, heard: bool) -> ApiResult<()> {
    let Some(collar) = db::get_collar(ctx.db(), collar_id).await? else { return Ok(()) };
    let at = now();
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
    let copies = crate::escapes::hand_copies(&mut tx, ctx, &collar, at).await?;
    tx.commit().await?;
    tracing::info!(collar = %collar_id, herd = %collar.herd_id, holds = ?holds, copies = ?copies.iter().map(|b| b.version).collect::<Vec<_>>(), "moved collar: its new herd's boundaries as its own copies");
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn record_move_undo(
    ctx: &Ctx,
    identity: Option<&Identity>,
    collar: &Collar,
    from_herd: &str,
    to_herd: &str,
    from_animal: Option<&str>,
    to_animal: Option<&str>,
    from_boundary: Option<u32>,
    from_state: FenceState,
    from_outside: Option<DateTime<Utc>>,
) -> Option<String> {
    let id = id::new_id("undo");
    let at = now();
    let expires = at + undo_window();
    let by = identity.and_then(|i| i.user_id.clone().or_else(|| i.name.clone()));
    let ok = sqlx::query(
        "INSERT INTO collar_undos (id, collar_id, kind, from_herd_id, to_herd_id, from_animal_id, to_animal_id,
            from_boundary_version, from_state, from_outside_since, by_user, created_at, expires_at)
         VALUES (?, ?, 'move', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&collar.id)
    .bind(from_herd)
    .bind(to_herd)
    .bind(from_animal)
    .bind(to_animal)
    .bind(from_boundary.map(|v| v as i64))
    .bind(from_state.as_str())
    .bind(from_outside.as_ref().map(to_db))
    .bind(by)
    .bind(to_db(&at))
    .bind(to_db(&expires))
    .execute(ctx.db())
    .await;
    match ok {
        Ok(_) => Some(id),
        Err(e) => {
            tracing::warn!("move undo row: {e:#}");
            None
        }
    }
}

async fn notice_moved(
    ctx: &Ctx,
    identity: Option<&Identity>,
    collar_id: &str,
    from_herd: &str,
    to_herd: &str,
    animal_id: Option<&str>,
    undo_id: Option<String>,
) {
    let at = now();
    let undo_until = undo_id.as_ref().map(|_| at + undo_window());
    let e = ActivityEvent {
        id: id::new_id(id::EVENT),
        kind: "collar.moved".into(),
        source: "farmer".into(),
        occurred_at: at,
        recorded_at: at,
        title: "Collar moved between herds".into(),
        body: None,
        payload: json!({
            "collar_id": collar_id,
            "from_herd_id": from_herd,
            "to_herd_id": to_herd,
            "animal_id": animal_id,
            "undo_id": undo_id,
            "undo_until": undo_until,
            "by": identity.map(|i| i.actor()),
            "fence_restored_exactly": false,
        }),
        targets: vec![("collar".into(), collar_id.to_owned()), ("herd".into(), to_herd.to_owned()), ("herd".into(), from_herd.to_owned())],
    };
    if let Err(err) = ctx.store().record_event(&e).await {
        tracing::warn!("move audit: {err:#}");
    }
    ctx.publish(Event::CollarMoved {
        collar_id: collar_id.to_owned(),
        from_herd_id: from_herd.to_owned(),
        to_herd_id: to_herd.to_owned(),
        animal_id: animal_id.map(str::to_owned),
        undo_id,
        undo_until,
        fence_restored_exactly: false,
    });
}

#[derive(Serialize)]
#[serde(untagged)]
enum UndoResult {
    Moved(Collar),
    ClaimUndone {
        undone: &'static str,
        serial: String,
        collar_id: String,
        /// Fresh claim code so the unit can be claimed again (shown once).
        claim_code: String,
    },
}

/// `POST /api/collars/{id}/undo`: reverse the latest claim or move within
/// [`UNDO_WINDOW_HOURS`] (default pending owner OK).
/// Move undo restores the previous herd and re-copies that herd's current
/// fence (not a byte-for-byte restore of the prior applied version).
async fn undo(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>) -> ApiResult<Json<UndoResult>> {
    let at = now();
    let row: Option<(
        String,
        String,
        Option<String>,
        Option<String>,
        String,
        Option<String>,
        Option<String>,
        Option<i64>,
        Option<String>,
        Option<String>,
        String,
    )> = sqlx::query_as(
        "SELECT id, kind, serial, from_herd_id, to_herd_id, from_animal_id, to_animal_id,
                    from_boundary_version, from_state, from_outside_since, expires_at
             FROM collar_undos WHERE collar_id = ? AND undone_at IS NULL ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&id)
    .fetch_optional(ctx.db())
    .await?;
    let Some((undo_id, kind, serial, from_herd, _to_herd, from_animal, _to_animal, _from_bv, _from_state, _from_outside, expires)) = row else {
        return Err(ApiError::conflict("Nothing to undo for this collar."));
    };
    let expires_at = from_db(&expires)?;
    if expires_at < at {
        return Err(ApiError::conflict(format!("The {UNDO_WINDOW_HOURS}-hour undo window has ended.")));
    }
    let collar = find(&ctx, &id).await?;
    match kind.as_str() {
        "claim" => {
            let serial = serial.ok_or_else(|| ApiError::internal("claim undo missing serial"))?;
            if collar.animal_id.is_some() {
                link_animal(&ctx, &collar, None).await?;
            }
            let fresh = op_protocol::v2::identity::new_claim_code();
            let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
            // USB-prebound units keep v1_collar_id / v1_key_hash / v1_herd_id and
            // restore the printed card code from v1_card_code across undo.
            let v1_card: Option<String> = sqlx::query_scalar("SELECT v1_card_code FROM fleet_units WHERE serial = ? AND v1_key_hash IS NOT NULL")
                .bind(&serial)
                .fetch_optional(&mut *tx)
                .await?
                .flatten();
            let code = v1_card.unwrap_or(fresh);
            sqlx::query("UPDATE animals SET collar_id = NULL WHERE collar_id = ?").bind(&id).execute(&mut *tx).await?;
            let n = sqlx::query(
                "UPDATE fleet_units SET collar_id = NULL, state = 'provisioned', claim_code = ?, claimed_at = NULL, updated_at = ?
                 WHERE serial = ? AND collar_id = ?",
            )
            .bind(&code)
            .bind(to_db(&at))
            .bind(&serial)
            .bind(&id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
            if n != 1 {
                return Err(ApiError::conflict("The unit changed meanwhile."));
            }
            let n = sqlx::query("DELETE FROM collars WHERE id = ?").bind(&id).execute(&mut *tx).await?.rows_affected();
            if n != 1 {
                return Err(ApiError::conflict("The collar changed meanwhile."));
            }
            sqlx::query("UPDATE collar_undos SET undone_at = ? WHERE id = ?").bind(to_db(&at)).bind(&undo_id).execute(&mut *tx).await?;
            tx.commit().await?;
            let e = ActivityEvent {
                id: id::new_id(id::EVENT),
                kind: "collar.claim_undone".into(),
                source: "farmer".into(),
                occurred_at: at,
                recorded_at: at,
                title: "Claim undone".into(),
                body: None,
                payload: json!({ "collar_id": id, "serial": serial, "undo_id": undo_id, "by": identity.actor() }),
                targets: vec![("unit".into(), serial.clone())],
            };
            let _ = ctx.store().record_event(&e).await;
            Ok(Json(UndoResult::ClaimUndone { undone: "claim", serial, collar_id: id, claim_code: code }))
        }
        "move" => {
            let from_herd = from_herd.ok_or_else(|| ApiError::internal("move undo missing from_herd"))?;
            match crate::escapes::stop_escape(&ctx, &id).await {
                Ok(_) => {}
                Err(e) if e.status == StatusCode::CONFLICT => {}
                Err(e) => return Err(e),
            }
            if collar.animal_id.is_some() {
                link_animal(&ctx, &collar, None).await?;
            }
            // Restore herd + re-copy that herd's current fence (not exact prior version).
            sqlx::query("UPDATE collars SET herd_id = ?, animal_id = NULL, boundary_version = NULL, state = ?, outside_since = NULL WHERE id = ?")
                .bind(&from_herd)
                .bind(FenceState::Unknown.as_str())
                .bind(&id)
                .execute(ctx.db())
                .await?;
            hand_over(&ctx, &id, None, collar.last_seen.is_some()).await?;
            crate::config::refresh_quietly(&ctx, crate::config::Scope::Collar(&id)).await;
            if let Some(aid) = from_animal.as_deref() {
                if let Ok(Some(a)) = ctx.store().get_animal(aid).await {
                    if a.herd_id == from_herd && a.collar_id.is_none() && a.removed_at.is_none() {
                        let after = find(&ctx, &id).await?;
                        link_animal(&ctx, &after, Some(aid)).await?;
                    }
                }
            }
            sqlx::query("UPDATE collar_undos SET undone_at = ? WHERE id = ?").bind(to_db(&at)).bind(&undo_id).execute(ctx.db()).await?;
            let next = find(&ctx, &id).await?;
            let e = ActivityEvent {
                id: id::new_id(id::EVENT),
                kind: "collar.move_undone".into(),
                source: "farmer".into(),
                occurred_at: at,
                recorded_at: at,
                title: "Herd move undone".into(),
                body: Some("Fence re-copied from the restored herd's current boundary, not the prior applied version.".into()),
                payload: json!({
                    "collar_id": id,
                    "herd_id": from_herd,
                    "undo_id": undo_id,
                    "fence_restored_exactly": false,
                    "by": identity.actor(),
                }),
                targets: vec![("collar".into(), id.clone()), ("herd".into(), from_herd)],
            };
            let _ = ctx.store().record_event(&e).await;
            ctx.publish(Event::Collar { collar: next.clone() });
            Ok(Json(UndoResult::Moved(next)))
        }
        _ => Err(ApiError::internal("unknown undo kind")),
    }
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
    // These are real collars; a simulated herd takes only the simulator's.
    if op_core::sim::herd_is_simulated(ctx.db(), herd_id).await? {
        return Err(ApiError::conflict(op_core::sim::WRONG_SIDE));
    }
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
