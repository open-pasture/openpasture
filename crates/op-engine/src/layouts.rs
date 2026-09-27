//! Saved strip layouts per paddock, and copying a paddock (docs/API.md,
//! "Strips and layouts"). A layout keeps how its strips were cut and the
//! strips themselves, so an arrangement that worked can be used again. When
//! the paddock's shape changes, applying the layout cuts it again from the
//! same settings.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::{Actor, ApiError, ApiJson, ApiResult, Ctx, Identity, Paddock, PaddockStatus, Polygon, id, time};
use op_geo::projection::{M_PER_DEG_LAT, Projection, round7};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::strips::{self, Preview, StripParams};

/// `lay_…` ids.
pub const LAYOUT: &str = "lay";
/// Farthest a copy may be offset from its original, in metres each way.
const MAX_OFFSET_M: f64 = 10_000.0;

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/layouts", get(list_route).post(create_route))
        .route("/api/layouts/{id}", get(get_route).patch(rename_route).delete(delete_route))
        .route("/api/layouts/{id}/apply", post(apply_route))
        .route("/api/paddocks/{id}/copy", post(copy_route))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Layout {
    pub id: String,
    pub paddock_id: String,
    pub name: String,
    pub params: StripParams,
    pub strips: Vec<Polygon>,
    pub created_by: Actor,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// A layout laid on its paddock now: the strips (cut again if the paddock
/// changed shape) with today's grazeable ground and days.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Applied {
    pub layout: Layout,
    #[serde(flatten)]
    pub preview: Preview,
}

/// Key of a paddock shape, to tell whether strips still fit it.
fn shape_key(g: &Polygon) -> String {
    let d = Sha256::digest(serde_json::to_string(g).unwrap_or_default().as_bytes());
    d.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

fn layout_from_row(r: &SqliteRow) -> anyhow::Result<Layout> {
    Ok(Layout {
        id: r.try_get("id")?,
        paddock_id: r.try_get("paddock_id")?,
        name: r.try_get("name")?,
        params: serde_json::from_str(&r.try_get::<String, _>("params")?)?,
        strips: serde_json::from_str(&r.try_get::<String, _>("strips")?)?,
        created_by: serde_json::from_str(&r.try_get::<String, _>("created_by")?)?,
        created_at: time::from_db(&r.try_get::<String, _>("created_at")?)?,
        updated_at: time::from_db(&r.try_get::<String, _>("updated_at")?)?,
    })
}

/// Layouts of one paddock (or all), oldest first.
pub async fn list_layouts(ctx: &Ctx, paddock_id: Option<&str>) -> anyhow::Result<Vec<Layout>> {
    let rows = match paddock_id {
        Some(p) => sqlx::query("SELECT * FROM layouts WHERE paddock_id = ? ORDER BY created_at, rowid").bind(p).fetch_all(ctx.db()).await?,
        None => sqlx::query("SELECT * FROM layouts ORDER BY created_at, rowid").fetch_all(ctx.db()).await?,
    };
    rows.iter().map(layout_from_row).collect()
}

pub async fn get_layout(ctx: &Ctx, id: &str) -> anyhow::Result<Option<Layout>> {
    let row = sqlx::query("SELECT * FROM layouts WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?;
    row.as_ref().map(layout_from_row).transpose()
}

async fn find(ctx: &Ctx, id: &str) -> ApiResult<Layout> {
    get_layout(ctx, id).await?.ok_or_else(|| ApiError::not_found("No such layout."))
}

fn clean_name(s: &str) -> ApiResult<String> {
    let s = s.trim();
    if s.is_empty() {
        return Err(ApiError::bad_request("The layout needs a name."));
    }
    if s.chars().count() > 200 {
        return Err(ApiError::bad_request("The layout name is too long."));
    }
    Ok(s.to_owned())
}

/// `base`, or `base 2`, `base 3`… whichever isn't taken.
fn unique(base: &str, taken: &[String]) -> String {
    if !taken.iter().any(|t| t == base) {
        return base.to_owned();
    }
    (2..).map(|n| format!("{base} {n}")).find(|c| !taken.iter().any(|t| t == c)).unwrap_or_else(|| base.to_owned())
}

#[derive(Debug, Deserialize)]
pub struct NewLayout {
    pub paddock_id: String,
    #[serde(default)]
    pub herd_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(flatten)]
    pub params: StripParams,
}

/// Cut the strips for `params` (exactly as a preview would) and keep them.
/// Strips sized by days also keep the width that chose, so the layout stays
/// the same when the forage changes.
pub async fn create_layout(ctx: &Ctx, new: NewLayout, by: Actor) -> ApiResult<Layout> {
    let (paddock, herd) = strips::load(ctx, &new.paddock_id, new.herd_id.as_deref()).await?;
    let preview = strips::preview(ctx, &paddock, herd.as_ref(), &new.params).await?;
    let mut params = new.params;
    if params.days.is_some() {
        params.width_m = Some(preview.width_m);
    }
    let taken: Vec<String> = list_layouts(ctx, Some(&paddock.id)).await?.into_iter().map(|l| l.name).collect();
    let name = match new.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => clean_name(n)?,
        None => unique(&format!("{} strips", preview.strips.len()), &taken),
    };
    let now = time::now();
    let layout = Layout {
        id: id::new_id(LAYOUT),
        paddock_id: paddock.id.clone(),
        name,
        params,
        strips: preview.strips.into_iter().map(|s| s.geometry).collect(),
        created_by: by,
        created_at: now,
        updated_at: now,
    };
    sqlx::query("INSERT INTO layouts (id, paddock_id, name, params, strips, fitted_to, created_by, created_at, updated_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)")
        .bind(&layout.id)
        .bind(&layout.paddock_id)
        .bind(&layout.name)
        .bind(serde_json::to_string(&layout.params).map_err(anyhow::Error::from)?)
        .bind(serde_json::to_string(&layout.strips).map_err(anyhow::Error::from)?)
        .bind(shape_key(&paddock.geometry))
        .bind(serde_json::to_string(&layout.created_by).map_err(anyhow::Error::from)?)
        .bind(time::to_db(&now))
        .bind(time::to_db(&now))
        .execute(ctx.db())
        .await?;
    Ok(layout)
}

#[derive(Debug, Default, Deserialize)]
pub struct ApplyBody {
    #[serde(default)]
    pub herd_id: Option<String>,
    #[serde(default)]
    pub head: Option<u32>,
}

/// The layout on its paddock now. If the paddock changed shape since the
/// strips were cut, they are cut again from the layout's settings (by count,
/// else by the width it kept) and stored. Days are for `head`, else the herd's
/// count, else the head the layout was sized for.
pub async fn apply_layout(ctx: &Ctx, id: &str, body: ApplyBody) -> ApiResult<Applied> {
    let mut layout = find(ctx, id).await?;
    let (paddock, herd) = strips::load(ctx, &layout.paddock_id, body.herd_id.as_deref()).await?;
    let head = body.head.or(herd.as_ref().map(|h| h.count)).or(layout.params.head);
    let fitted: String = sqlx::query("SELECT fitted_to FROM layouts WHERE id = ?").bind(id).fetch_one(ctx.db()).await?.get(0);
    let key = shape_key(&paddock.geometry);
    let preview = if fitted != key {
        let recut = StripParams {
            days: None,
            head,
            count: layout.params.count,
            width_m: layout.params.count.map_or(layout.params.width_m, |_| None),
            ..layout.params.clone()
        };
        let preview = strips::preview(ctx, &paddock, herd.as_ref(), &recut).await?;
        layout.strips = preview.strips.iter().map(|s| s.geometry.clone()).collect();
        layout.updated_at = time::now();
        sqlx::query("UPDATE layouts SET strips = ?, fitted_to = ?, updated_at = ? WHERE id = ?")
            .bind(serde_json::to_string(&layout.strips).map_err(anyhow::Error::from)?)
            .bind(&key)
            .bind(time::to_db(&layout.updated_at))
            .bind(id)
            .execute(ctx.db())
            .await?;
        preview
    } else {
        let forage = strips::paddock_forage(ctx, &paddock).await?;
        let excl = strips::exclusions(ctx, &paddock).await?;
        let (head, au) = strips::feeding(herd.as_ref(), head);
        let depth = op_geo::strip::depth_m(&paddock.geometry, layout.params.orientation_deg);
        let width = layout.params.count.map_or(layout.params.width_m.unwrap_or(depth), |n| depth / n.max(1) as f64);
        Preview {
            paddock_id: paddock.id.clone(),
            strips: strips::facts(layout.strips.clone(), &excl, forage.as_ref(), au),
            width_m: crate::calc::round(width, 2),
            depth_m: crate::calc::round(depth, 2),
            head,
            animal_units: au,
            forage_kg_dm_per_ha: forage.as_ref().map(|f| f.kg_dm_per_ha),
            forage_source: forage.and_then(|f| f.source),
            warn_m: layout.params.warn_m.unwrap_or(op_geo::shape::DEFAULT_WARN_M),
        }
    };
    Ok(Applied { layout, preview })
}

#[derive(Debug, Default, Deserialize)]
pub struct CopyBody {
    #[serde(default)]
    pub name: Option<String>,
    /// Metres east and north to move the copy by.
    #[serde(default)]
    pub offset_m: Option<[f64; 2]>,
}

/// A new paddock with the same shape ("P3 copy", "P3 copy 2", …), moved by
/// `offset_m` when given. Notes, FSA numbers and grazing history stay with
/// the original.
pub async fn copy_paddock(ctx: &Ctx, id: &str, body: CopyBody) -> ApiResult<Paddock> {
    let original = strips::find_paddock(ctx, id).await?;
    let [east, north] = body.offset_m.unwrap_or([0.0, 0.0]);
    if !east.is_finite() || !north.is_finite() || east.abs() > MAX_OFFSET_M || north.abs() > MAX_OFFSET_M {
        return Err(ApiError::bad_request("offset_m must be [east, north] metres, each within 10 km."));
    }
    let geometry = shifted(&original.geometry, east, north).validated()?;
    let paddocks = ctx.store().list_paddocks().await?;
    let taken: Vec<String> = paddocks.into_iter().map(|p| p.name).collect();
    let name = match body.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) if n.chars().count() > 200 => return Err(ApiError::bad_request("The paddock name is too long.")),
        Some(n) => n.to_owned(),
        None => {
            let base: String = original.name.chars().take(190).collect();
            unique(&format!("{base} copy"), &taken)
        }
    };
    let paddock = Paddock {
        id: id::new_id(id::PADDOCK),
        name,
        area_ha: (geometry.area_ha() * 1000.0).round() / 1000.0,
        geometry,
        status: PaddockStatus::default(),
        notes: None,
        grazed_until: None,
        created_at: time::now(),
        props: Default::default(),
    };
    ctx.store().insert_paddock(&paddock).await?;
    Ok(paddock)
}

/// Every vertex moved the same metres east and north (measured at the shape's centre, so it keeps its shape).
fn shifted(g: &Polygon, east: f64, north: f64) -> Polygon {
    if east == 0.0 && north == 0.0 {
        return g.clone();
    }
    let at = g.centroid().or_else(|| g.outer_ring().first().copied()).unwrap_or([0.0, 0.0]);
    let proj = Projection::new(at);
    let (dlon, dlat) = (east / proj.m_per_deg_lon, north / M_PER_DEG_LAT);
    Polygon { kind: g.kind, coordinates: g.coordinates.iter().map(|r| r.iter().map(|p| [round7(p[0] + dlon), round7(p[1] + dlat)]).collect()).collect() }
}

#[derive(Deserialize)]
struct ListQuery {
    paddock_id: Option<String>,
}

async fn list_route(State(ctx): State<Ctx>, Query(q): Query<ListQuery>) -> ApiResult<Json<Vec<Layout>>> {
    Ok(Json(list_layouts(&ctx, q.paddock_id.as_deref().filter(|p| !p.is_empty())).await?))
}

async fn create_route(State(ctx): State<Ctx>, identity: Identity, ApiJson(b): ApiJson<NewLayout>) -> ApiResult<(StatusCode, Json<Layout>)> {
    Ok((StatusCode::CREATED, Json(create_layout(&ctx, b, identity.actor()).await?)))
}

async fn get_route(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<Layout>> {
    Ok(Json(find(&ctx, &id).await?))
}

#[derive(Deserialize)]
struct Rename {
    name: String,
}

async fn rename_route(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(b): ApiJson<Rename>) -> ApiResult<Json<Layout>> {
    let mut layout = find(&ctx, &id).await?;
    layout.name = clean_name(&b.name)?;
    layout.updated_at = time::now();
    sqlx::query("UPDATE layouts SET name = ?, updated_at = ? WHERE id = ?")
        .bind(&layout.name)
        .bind(time::to_db(&layout.updated_at))
        .bind(&id)
        .execute(ctx.db())
        .await?;
    Ok(Json(layout))
}

async fn delete_route(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let done = sqlx::query("DELETE FROM layouts WHERE id = ?").bind(&id).execute(ctx.db()).await?;
    if done.rows_affected() == 0 {
        return Err(ApiError::not_found("No such layout."));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn apply_route(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(b): ApiJson<ApplyBody>) -> ApiResult<Json<Applied>> {
    Ok(Json(apply_layout(&ctx, &id, b).await?))
}

async fn copy_route(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(b): ApiJson<CopyBody>) -> ApiResult<(StatusCode, Json<Paddock>)> {
    Ok((StatusCode::CREATED, Json(copy_paddock(&ctx, &id, b).await?)))
}
