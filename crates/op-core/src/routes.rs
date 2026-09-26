//! The op-core routes of `docs/API.md`: state, farm, paddocks, herds,
//! animals, settings, secrets.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, put};
use serde::Deserialize;
use serde_json::Value;

use crate::domain::*;
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::{Ctx, id, patch, secrets, time};

use axum::Json;

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/state", get(get_state))
        .route("/api/farm", get(get_farm).post(create_farm).patch(update_farm))
        .route("/api/paddocks", get(list_paddocks).post(create_paddock))
        .route("/api/paddocks/{id}", get(get_paddock).patch(update_paddock).delete(delete_paddock))
        .route("/api/herds", get(list_herds).post(create_herd))
        .route("/api/herds/{id}", get(get_herd).patch(update_herd).delete(delete_herd))
        .route("/api/animals", get(list_animals).post(create_animal))
        .route("/api/animals/{id}", get(get_animal).patch(update_animal).delete(delete_animal))
        .route("/api/settings", get(get_settings).put(put_settings))
        .route("/api/secrets", get(list_secrets))
        .route("/api/secrets/{name}", put(put_secret).delete(delete_secret))
}

fn clean_name(s: &str, what: &str) -> ApiResult<String> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ApiError::bad_request(format!("{what} needs a name.")));
    }
    if s.len() > 200 {
        return Err(ApiError::bad_request(format!("{what} name is too long.")));
    }
    Ok(s.to_owned())
}

fn check_point(p: LonLat) -> ApiResult<()> {
    if p[0].is_finite() && p[1].is_finite() && p[0].abs() <= 180.0 && p[1].abs() <= 90.0 {
        Ok(())
    } else {
        Err(ApiError::bad_request("center must be [longitude, latitude]."))
    }
}

async fn require_farm(ctx: &Ctx) -> ApiResult<Farm> {
    ctx.store().get_farm().await?.ok_or_else(|| ApiError::bad_request("Create the farm first."))
}

// State

async fn get_state(State(ctx): State<Ctx>) -> ApiResult<Json<AppState>> {
    let store = ctx.store();
    Ok(Json(AppState {
        farm: store.get_farm().await?,
        herds: store.list_herds().await?,
        paddocks: store.list_paddocks().await?,
        settings: ctx.settings().await?,
    }))
}

// Farm

#[derive(Deserialize)]
struct NewFarm {
    name: String,
    /// Used only where the location has no zone.
    #[serde(default)]
    timezone: Option<String>,
    center: LonLat,
}

async fn get_farm(State(ctx): State<Ctx>) -> ApiResult<Json<Farm>> {
    Ok(Json(ctx.store().get_farm().await?.ok_or_else(|| ApiError::not_found("No farm yet."))?))
}

async fn create_farm(State(ctx): State<Ctx>, ApiJson(body): ApiJson<NewFarm>) -> ApiResult<(StatusCode, Json<Farm>)> {
    if ctx.store().get_farm().await?.is_some() {
        return Err(ApiError::conflict("The farm already exists. Use PATCH /api/farm."));
    }
    check_point(body.center)?;
    // The farm's zone comes from where it is, not from whoever set it up.
    let timezone =
        crate::tz::at(body.center).or_else(|| body.timezone.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty())).unwrap_or_else(|| "UTC".to_owned());
    let farm = Farm { id: id::new_id(id::FARM), name: clean_name(&body.name, "The farm")?, timezone, center: body.center, created_at: time::now() };
    ctx.store().insert_farm(&farm).await?;
    Ok((StatusCode::CREATED, Json(farm)))
}

async fn update_farm(State(ctx): State<Ctx>, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<Farm>> {
    let current = ctx.store().get_farm().await?.ok_or_else(|| ApiError::not_found("No farm yet."))?;
    let mut farm: Farm = patch::apply(&current, &body, &["id", "created_at"])?;
    farm.name = clean_name(&farm.name, "The farm")?;
    check_point(farm.center)?;
    if farm.center != current.center
        && let Some(tz) = crate::tz::at(farm.center)
    {
        farm.timezone = tz;
    }
    if farm.timezone.trim().is_empty() {
        return Err(ApiError::bad_request("The farm needs a timezone."));
    }
    ctx.store().update_farm(&farm).await?;
    Ok(Json(farm))
}

// Paddocks

#[derive(Deserialize)]
struct NewPaddock {
    name: String,
    geometry: Polygon,
    #[serde(default)]
    status: PaddockStatus,
    notes: Option<String>,
    grazed_until: Option<chrono::DateTime<chrono::Utc>>,
}

async fn list_paddocks(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<Paddock>>> {
    Ok(Json(ctx.store().list_paddocks().await?))
}

async fn get_paddock(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<Paddock>> {
    Ok(Json(find_paddock(&ctx, &id).await?))
}

async fn find_paddock(ctx: &Ctx, id: &str) -> ApiResult<Paddock> {
    ctx.store().get_paddock(id).await?.ok_or_else(|| ApiError::not_found("No such paddock."))
}

async fn create_paddock(State(ctx): State<Ctx>, ApiJson(body): ApiJson<NewPaddock>) -> ApiResult<(StatusCode, Json<Paddock>)> {
    require_farm(&ctx).await?;
    let geometry = body.geometry.validated()?;
    let paddock = Paddock {
        id: id::new_id(id::PADDOCK),
        name: clean_name(&body.name, "The paddock")?,
        area_ha: round_ha(geometry.area_ha()),
        geometry,
        status: body.status,
        notes: body.notes.filter(|n| !n.trim().is_empty()),
        grazed_until: body.grazed_until,
        created_at: time::now(),
    };
    ctx.store().insert_paddock(&paddock).await?;
    Ok((StatusCode::CREATED, Json(paddock)))
}

async fn update_paddock(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<Paddock>> {
    let current = find_paddock(&ctx, &id).await?;
    let mut p: Paddock = patch::apply(&current, &body, &["id", "created_at", "area_ha"])?;
    p.name = clean_name(&p.name, "The paddock")?;
    if p.geometry != current.geometry {
        p.geometry = p.geometry.validated()?;
        p.area_ha = round_ha(p.geometry.area_ha());
    }
    ctx.store().update_paddock(&p).await?;
    Ok(Json(p))
}

async fn delete_paddock(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    if !ctx.store().delete_paddock(&id).await? {
        return Err(ApiError::not_found("No such paddock."));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn round_ha(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

// Herds

#[derive(Deserialize)]
struct NewHerd {
    name: String,
    species: Species,
    count: u32,
    paddock_id: Option<String>,
    #[serde(default)]
    autonomy: Autonomy,
    timer_minutes: Option<u32>,
}

pub const DEFAULT_TIMER_MINUTES: u32 = 60;

async fn list_herds(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<Herd>>> {
    Ok(Json(ctx.store().list_herds().await?))
}

async fn get_herd(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<Herd>> {
    Ok(Json(find_herd(&ctx, &id).await?))
}

async fn find_herd(ctx: &Ctx, id: &str) -> ApiResult<Herd> {
    ctx.store().get_herd(id).await?.ok_or_else(|| ApiError::not_found("No such herd."))
}

async fn check_herd(ctx: &Ctx, h: &Herd) -> ApiResult<()> {
    if let Some(p) = &h.paddock_id
        && ctx.store().get_paddock(p).await?.is_none()
    {
        return Err(ApiError::bad_request("No such paddock."));
    }
    if h.timer_minutes == 0 || h.timer_minutes > 7 * 24 * 60 {
        return Err(ApiError::bad_request("timer_minutes must be between 1 and 10080."));
    }
    Ok(())
}

async fn create_herd(State(ctx): State<Ctx>, ApiJson(body): ApiJson<NewHerd>) -> ApiResult<(StatusCode, Json<Herd>)> {
    require_farm(&ctx).await?;
    let herd = Herd {
        id: id::new_id(id::HERD),
        name: clean_name(&body.name, "The herd")?,
        species: body.species,
        count: body.count,
        paddock_id: body.paddock_id.filter(|p| !p.is_empty()),
        autonomy: body.autonomy,
        timer_minutes: body.timer_minutes.unwrap_or(DEFAULT_TIMER_MINUTES),
        created_at: time::now(),
    };
    check_herd(&ctx, &herd).await?;
    ctx.store().insert_herd(&herd).await?;
    // A herd placed in a resting paddock is grazing it now.
    if let Some(pid) = &herd.paddock_id
        && let Some(mut p) = ctx.store().get_paddock(pid).await?
        && p.status == PaddockStatus::Resting
    {
        p.status = PaddockStatus::Grazing;
        ctx.store().update_paddock(&p).await?;
    }
    Ok((StatusCode::CREATED, Json(herd)))
}

async fn update_herd(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<Herd>> {
    let current = find_herd(&ctx, &id).await?;
    let mut h: Herd = patch::apply(&current, &body, &["id", "created_at"])?;
    h.name = clean_name(&h.name, "The herd")?;
    check_herd(&ctx, &h).await?;
    ctx.store().update_herd(&h).await?;
    if h.autonomy != current.autonomy || (h.autonomy == Autonomy::Timer && h.timer_minutes != current.timer_minutes) {
        follow_autonomy(&ctx, &h).await?;
    }
    Ok(Json(h))
}

/// An open proposed MOVE follows the herd's new autonomy: timer starts its
/// countdown from now, auto sends it (the engine's scheduler applies it at
/// once, claiming it like any timer decision), propose stops the clock. Only
/// rows still `proposed` change, so a decision being answered or sent is left alone.
async fn follow_autonomy(ctx: &Ctx, h: &Herd) -> ApiResult<()> {
    let apply_at = match h.autonomy {
        Autonomy::Timer => Some(time::now() + chrono::Duration::minutes(h.timer_minutes as i64)),
        Autonomy::Auto => Some(time::now()),
        Autonomy::Propose => None,
    };
    let rows = sqlx::query("UPDATE decisions SET apply_at = ? WHERE herd_id = ? AND status = 'proposed' AND action = 'MOVE' RETURNING *")
        .bind(apply_at.as_ref().map(time::to_db))
        .bind(&h.id)
        .fetch_all(ctx.db())
        .await?;
    for r in &rows {
        ctx.publish(crate::Event::Decision { decision: crate::store::decision_from_row(r)? });
    }
    if h.autonomy == Autonomy::Auto && !rows.is_empty() {
        ctx.wake_scheduler();
    }
    Ok(())
}

async fn delete_herd(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    if !ctx.store().delete_herd(&id).await? {
        return Err(ApiError::not_found("No such herd."));
    }
    Ok(StatusCode::NO_CONTENT)
}

// Animals

#[derive(Deserialize)]
struct NewAnimal {
    tag: String,
    name: Option<String>,
    herd_id: String,
    collar_id: Option<String>,
}

#[derive(Deserialize)]
struct AnimalQuery {
    herd_id: Option<String>,
}

async fn list_animals(State(ctx): State<Ctx>, Query(q): Query<AnimalQuery>) -> ApiResult<Json<Vec<Animal>>> {
    Ok(Json(ctx.store().list_animals(q.herd_id.as_deref()).await?))
}

async fn get_animal(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<Animal>> {
    Ok(Json(find_animal(&ctx, &id).await?))
}

async fn find_animal(ctx: &Ctx, id: &str) -> ApiResult<Animal> {
    ctx.store().get_animal(id).await?.ok_or_else(|| ApiError::not_found("No such animal."))
}

async fn check_animal(ctx: &Ctx, a: &Animal) -> ApiResult<()> {
    if a.tag.trim().is_empty() {
        return Err(ApiError::bad_request("The animal needs a tag."));
    }
    if ctx.store().get_herd(&a.herd_id).await?.is_none() {
        return Err(ApiError::bad_request("No such herd."));
    }
    if let Some(c) = &a.collar_id {
        if !ctx.store().collar_exists(c).await? {
            return Err(ApiError::bad_request("No such collar."));
        }
        if let Some(other) = ctx.store().animal_for_collar(c).await?
            && other.id != a.id
        {
            return Err(ApiError::conflict(format!("That collar is on {}.", other.name.unwrap_or(other.tag))));
        }
    }
    Ok(())
}

async fn create_animal(State(ctx): State<Ctx>, ApiJson(body): ApiJson<NewAnimal>) -> ApiResult<(StatusCode, Json<Animal>)> {
    let animal = Animal {
        id: id::new_id(id::ANIMAL),
        tag: body.tag.trim().to_owned(),
        name: body.name.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()),
        herd_id: body.herd_id,
        collar_id: body.collar_id.filter(|c| !c.is_empty()),
    };
    check_animal(&ctx, &animal).await?;
    ctx.store().insert_animal(&animal).await?;
    Ok((StatusCode::CREATED, Json(animal)))
}

async fn update_animal(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<Animal>> {
    let current = find_animal(&ctx, &id).await?;
    let mut a: Animal = patch::apply(&current, &body, &["id"])?;
    a.tag = a.tag.trim().to_owned();
    check_animal(&ctx, &a).await?;
    ctx.store().update_animal(&a).await?;
    Ok(Json(a))
}

async fn delete_animal(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    if !ctx.store().delete_animal(&id).await? {
        return Err(ApiError::not_found("No such animal."));
    }
    Ok(StatusCode::NO_CONTENT)
}

// Settings

async fn get_settings(State(ctx): State<Ctx>) -> ApiResult<Json<Settings>> {
    Ok(Json(ctx.settings().await?))
}

async fn put_settings(State(ctx): State<Ctx>, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<Settings>> {
    Ok(Json(ctx.update_settings(&body).await?))
}

// Secrets

#[derive(Deserialize)]
struct SecretValue {
    value: String,
}

async fn list_secrets(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<secrets::SecretStatus>>> {
    Ok(Json(ctx.secrets().status()?))
}

async fn put_secret(State(ctx): State<Ctx>, Path(name): Path<String>, ApiJson(body): ApiJson<SecretValue>) -> ApiResult<StatusCode> {
    if !secrets::valid_name(&name) {
        return Err(ApiError::bad_request("Secret names are lowercase letters, digits and underscores."));
    }
    let value = body.value.trim();
    if value.is_empty() {
        return Err(ApiError::bad_request("The value is empty."));
    }
    if value.len() > 8192 {
        return Err(ApiError::bad_request("The value is too long."));
    }
    ctx.secrets().set(&name, value)?;
    tracing::info!(secret = %name, "secret set");
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_secret(State(ctx): State<Ctx>, Path(name): Path<String>) -> ApiResult<StatusCode> {
    ctx.secrets().delete(&name)?;
    Ok(StatusCode::NO_CONTENT)
}
