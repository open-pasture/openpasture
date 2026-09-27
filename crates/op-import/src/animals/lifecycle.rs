//! An animal leaving the farm (sold, died, culled, moved off) and a collar
//! swapped for another. Both keep the record: a removed animal keeps its row
//! and its fixes; a swapped-off collar goes on the shelf.

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Duration, SubsecRound, Utc};
use op_core::store::collar_from_row;
use op_core::time::now;
use op_core::{Animal, ApiError, ApiJson, ApiResult, Collar, Ctx, Event, Identity, ParkReason, RemovedReason};
use serde::Deserialize;
use serde_json::json;

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/animals/{id}/remove", post(remove)).route("/api/animals/{id}/swap", post(swap))
}

async fn find_animal(ctx: &Ctx, id: &str) -> ApiResult<Animal> {
    ctx.store().get_animal(id).await?.ok_or_else(|| ApiError::not_found("No such animal."))
}

async fn find_collar(ctx: &Ctx, id: &str) -> ApiResult<Collar> {
    let row = sqlx::query("SELECT * FROM collars WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?;
    Ok(collar_from_row(&row.ok_or_else(|| ApiError::not_found("No such collar."))?)?)
}

fn words(r: RemovedReason) -> &'static str {
    match r {
        RemovedReason::Sold => "sold",
        RemovedReason::Died => "died",
        RemovedReason::Culled => "culled",
        RemovedReason::MovedOff => "moved off",
    }
}

#[derive(Deserialize)]
struct RemoveBody {
    reason: RemovedReason,
    /// When it left. Absent: now.
    at: Option<DateTime<Utc>>,
}

/// The animal leaves the farm: it keeps its record and fixes, drops off the
/// map and the head count, and its collar comes off and goes on the shelf.
async fn remove(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>, ApiJson(b): ApiJson<RemoveBody>) -> ApiResult<Json<Animal>> {
    let a = find_animal(&ctx, &id).await?;
    if a.removed_at.is_some() {
        return Err(ApiError::conflict(format!("{} was already removed.", a.tag)));
    }
    let at = b.at.unwrap_or_else(now).trunc_subsecs(3);
    if at > now() + Duration::minutes(5) {
        return Err(ApiError::bad_request("That date is in the future."));
    }
    let collar = a.collar_id.clone();
    let next = Animal { removed_at: Some(at), removed_reason: Some(b.reason), collar_id: None, ..a };
    ctx.store().update_animal(&next).await?;
    if let Some(c) = &collar {
        op_ingest::park_collar(&ctx, c, ParkReason::Shelf).await?;
    }
    op_core::animals::sync_herd_count(ctx.db(), &next.herd_id).await?;
    ctx.publish(Event::AnimalsChanged { herd_id: Some(next.herd_id.clone()) });
    let mut payload = json!({ "animal_id": next.id, "reason": b.reason, "at": at });
    if let Some(c) = &collar {
        payload["collar_id"] = json!(c);
    }
    if let Some(by) = identity.actor().name {
        payload["by"] = json!(by);
    }
    let mut targets = vec![("animal".to_owned(), next.id.clone()), ("herd".to_owned(), next.herd_id.clone())];
    targets.extend(collar.map(|c| ("collar".to_owned(), c)));
    super::log(&ctx, "animal.removed", format!("{} {}", next.tag, words(b.reason)), payload, targets).await;
    Ok(Json(next))
}

#[derive(Deserialize)]
struct SwapBody {
    collar_id: String,
}

/// Another collar of the herd on the animal. The old one comes off and goes on
/// the shelf; the fixes both took stay the animal's.
async fn swap(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>, ApiJson(b): ApiJson<SwapBody>) -> ApiResult<Json<Animal>> {
    let a = find_animal(&ctx, &id).await?;
    if a.removed_at.is_some() {
        return Err(ApiError::conflict(format!("{} is no longer on the farm.", a.tag)));
    }
    let new = find_collar(&ctx, &b.collar_id).await?;
    if a.collar_id.as_deref() == Some(new.id.as_str()) {
        return Err(ApiError::bad_request(format!("{} already wears {}.", a.tag, new.name)));
    }
    if new.herd_id != a.herd_id {
        return Err(ApiError::bad_request(format!("{} is in another herd.", new.name)));
    }
    if let Some(other) = ctx.store().animal_for_collar(&new.id).await?
        && other.id != a.id
    {
        return Err(ApiError::conflict(format!("{} is on {}.", new.name, other.tag)));
    }
    if let Some(old) = &a.collar_id {
        op_ingest::link_collar(&ctx, old, None).await?;
        op_ingest::park_collar(&ctx, old, ParkReason::Shelf).await?;
    }
    op_ingest::link_collar(&ctx, &new.id, Some(&a.id)).await?;
    let next = find_animal(&ctx, &a.id).await?;
    op_core::animals::sync_herd_count(ctx.db(), &next.herd_id).await?;
    ctx.publish(Event::AnimalsChanged { herd_id: Some(next.herd_id.clone()) });
    let mut payload = json!({ "animal_id": next.id, "collar_id": new.id });
    if let Some(old) = &a.collar_id {
        payload["old_collar_id"] = json!(old);
    }
    if let Some(by) = identity.actor().name {
        payload["by"] = json!(by);
    }
    let mut targets = vec![("animal".to_owned(), next.id.clone()), ("collar".to_owned(), new.id.clone())];
    targets.extend(a.collar_id.clone().map(|c| ("collar".to_owned(), c)));
    super::log(&ctx, "collar.swapped", format!("{} now wears {}", next.tag, new.name), payload, targets).await;
    Ok(Json(next))
}
