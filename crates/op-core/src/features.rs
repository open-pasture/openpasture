//! Map features: exclusions, water, gates, shade, hazards, roads, neighbour
//! lines and the farm boundary. Types, the table and the read and insert API
//! live here; CRUD routes and the map are op-core's `features_api` (stream D).
//!
//! Exclusions become holes on sends whose activation time falls inside their
//! active window. Everything else is checked before a send, never enforced by
//! itself.

use chrono::{DateTime, Utc};
use geo::Intersects;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::domain::{DbEnum, LonLat, Polygon};
use crate::error::{ApiError, ApiResult};
use crate::time::{from_db, now, opt_from_db, to_db};
use crate::{Ctx, id};

/// `fea_…`
pub const FEATURE: &str = "fea";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureKind {
    Exclusion,
    Water,
    Gate,
    Shade,
    Hazard,
    Road,
    NeighbourLine,
    FarmBoundary,
}

impl DbEnum for FeatureKind {}

/// GeoJSON geometry: `{ "type": "Point", "coordinates": [lon, lat] }` and so on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "coordinates")]
pub enum FeatureGeometry {
    Point(LonLat),
    LineString(Vec<LonLat>),
    Polygon(Vec<Vec<LonLat>>),
}

impl FeatureGeometry {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Point(_) => "Point",
            Self::LineString(_) => "LineString",
            Self::Polygon(_) => "Polygon",
        }
    }

    /// The polygon, for polygon features.
    pub fn polygon(&self) -> Option<Polygon> {
        match self {
            Self::Polygon(rings) => Some(Polygon { kind: Default::default(), coordinates: rings.clone() }),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MapFeature {
    pub id: String,
    pub kind: FeatureKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub geometry: FeatureGeometry,
    /// `None`: farm-wide.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paddock_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// water: `{source: tank|trough|pond|stream}`; hazard points: `{radius_m}`.
    #[serde(default)]
    pub props: Value,
    /// Temporary features (a wet spot, reseeding, a calving pen).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_from: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl MapFeature {
    /// Active at `at`: `active_from <= at < active_until`, each bound optional.
    pub fn active_at(&self, at: DateTime<Utc>) -> bool {
        self.active_from.is_none_or(|f| f <= at) && self.active_until.is_none_or(|u| at < u)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NewFeature {
    pub kind: FeatureKind,
    #[serde(default)]
    pub name: Option<String>,
    pub geometry: FeatureGeometry,
    #[serde(default)]
    pub paddock_id: Option<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub props: Value,
    #[serde(default)]
    pub active_from: Option<DateTime<Utc>>,
    #[serde(default)]
    pub active_until: Option<DateTime<Utc>>,
}

pub fn feature_from_row(r: &SqliteRow) -> anyhow::Result<MapFeature> {
    Ok(MapFeature {
        id: r.try_get("id")?,
        kind: FeatureKind::from_db(&r.try_get::<String, _>("kind")?)?,
        name: r.try_get("name")?,
        geometry: serde_json::from_str(&r.try_get::<String, _>("geometry")?)?,
        paddock_id: r.try_get("paddock_id")?,
        notes: r.try_get("notes")?,
        props: serde_json::from_str(&r.try_get::<String, _>("props")?)?,
        active_from: opt_from_db(r.try_get("active_from")?)?,
        active_until: opt_from_db(r.try_get("active_until")?)?,
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
        updated_at: from_db(&r.try_get::<String, _>("updated_at")?)?,
    })
}

/// Stored order. `created_at` has millisecond precision and ULIDs are random
/// within one millisecond, so ties go to SQLite's rowid (insertion order).
const ORDER: &str = " ORDER BY created_at, rowid";

const ACTIVE_AT: &str = "(active_from IS NULL OR active_from <= ?) AND (active_until IS NULL OR active_until > ?)";

/// Features in the order they were stored (`created_at`, then insertion order
/// for rows stored in the same millisecond, where ids don't sort by time),
/// optionally of one kind and one paddock. `at`: only those active at that
/// time; `None`: all.
pub async fn list_features(ctx: &Ctx, kind: Option<FeatureKind>, paddock_id: Option<&str>, at: Option<DateTime<Utc>>) -> anyhow::Result<Vec<MapFeature>> {
    let mut sql = "SELECT * FROM features WHERE 1 = 1".to_owned();
    if kind.is_some() {
        sql.push_str(" AND kind = ?");
    }
    if paddock_id.is_some() {
        sql.push_str(" AND paddock_id = ?");
    }
    if at.is_some() {
        sql.push_str(" AND ");
        sql.push_str(ACTIVE_AT);
    }
    sql.push_str(ORDER);
    let mut q = sqlx::query(&sql);
    if let Some(k) = kind {
        q = q.bind(k.as_db());
    }
    if let Some(p) = paddock_id {
        q = q.bind(p);
    }
    if let Some(t) = at {
        let t = to_db(&t);
        q = q.bind(t.clone()).bind(t);
    }
    q.fetch_all(ctx.db()).await?.iter().map(feature_from_row).collect()
}

pub async fn get_feature(ctx: &Ctx, id: &str) -> anyhow::Result<Option<MapFeature>> {
    let row = sqlx::query("SELECT * FROM features WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?;
    row.map(|r| feature_from_row(&r)).transpose()
}

/// Farm-wide exclusions plus those of every paddock `area` touches, active at `at`.
/// Same order as [`list_features`].
pub async fn exclusions_for(ctx: &Ctx, area: &Polygon, at: DateTime<Utc>) -> anyhow::Result<Vec<MapFeature>> {
    let target = to_geo(area);
    let touched: Vec<String> = ctx.store().list_paddocks().await?.into_iter().filter(|p| to_geo(&p.geometry).intersects(&target)).map(|p| p.id).collect();
    let marks = vec!["?"; touched.len()].join(", ");
    let scope = if touched.is_empty() { "paddock_id IS NULL".to_owned() } else { format!("(paddock_id IS NULL OR paddock_id IN ({marks}))") };
    let sql = format!("SELECT * FROM features WHERE kind = 'exclusion' AND {scope} AND {ACTIVE_AT}{ORDER}");
    let mut q = sqlx::query(&sql);
    for p in &touched {
        q = q.bind(p);
    }
    let t = to_db(&at);
    q = q.bind(t.clone()).bind(t);
    q.fetch_all(ctx.db()).await?.iter().map(feature_from_row).collect()
}

fn to_geo(p: &Polygon) -> geo::Polygon<f64> {
    let ring = |r: &Vec<LonLat>| geo::LineString::from(r.iter().map(|c| (c[0], c[1])).collect::<Vec<_>>());
    let mut rings = p.coordinates.iter();
    let outer = rings.next().map(ring).unwrap_or_else(|| geo::LineString::new(vec![]));
    geo::Polygon::new(outer, rings.map(ring).collect())
}

fn check_point(p: LonLat) -> ApiResult<LonLat> {
    if p[0].is_finite() && p[1].is_finite() && p[0].abs() <= 180.0 && p[1].abs() <= 90.0 {
        Ok([op_geo::projection::round7(p[0]), op_geo::projection::round7(p[1])])
    } else {
        Err(ApiError::bad_request("A point is out of range; use [longitude, latitude]."))
    }
}

/// Check the geometry type for the kind and normalise it (coordinates rounded
/// to 7 decimals, polygon rings closed and simple). Exclusion: one-ring
/// Polygon. Water, shade, hazard: Point or Polygon (a hazard point needs
/// `props.radius_m` > 0). Gate: Point. Road, neighbour line: LineString.
/// Farm boundary: Polygon.
pub fn check_geometry(kind: FeatureKind, g: &FeatureGeometry, props: &Value) -> ApiResult<FeatureGeometry> {
    use FeatureKind::*;
    let allowed: &[&str] = match kind {
        Exclusion | FarmBoundary => &["Polygon"],
        Water | Shade | Hazard => &["Point", "Polygon"],
        Gate => &["Point"],
        Road | NeighbourLine => &["LineString"],
    };
    if !allowed.contains(&g.type_name()) {
        let want = allowed.iter().map(|t| t.to_lowercase()).collect::<Vec<_>>().join(" or ");
        return Err(ApiError::bad_request(format!("{} is drawn as a {want}.", kind_label(kind))));
    }
    Ok(match g {
        FeatureGeometry::Point(p) => {
            if kind == Hazard && !props.get("radius_m").and_then(Value::as_f64).is_some_and(|r| r.is_finite() && r > 0.0) {
                return Err(ApiError::bad_request("A hazard point needs radius_m."));
            }
            FeatureGeometry::Point(check_point(*p)?)
        }
        FeatureGeometry::LineString(pts) => {
            if pts.len() < 2 {
                return Err(ApiError::bad_request("A line needs at least 2 points."));
            }
            FeatureGeometry::LineString(pts.iter().map(|p| check_point(*p)).collect::<ApiResult<_>>()?)
        }
        FeatureGeometry::Polygon(rings) => {
            if kind == Exclusion && rings.len() != 1 {
                return Err(ApiError::bad_request("An exclusion is one ring, without holes."));
            }
            let p = Polygon { kind: Default::default(), coordinates: rings.clone() }.validated()?;
            FeatureGeometry::Polygon(p.coordinates)
        }
    })
}

fn kind_label(kind: FeatureKind) -> &'static str {
    match kind {
        FeatureKind::Exclusion => "An exclusion",
        FeatureKind::Water => "Water",
        FeatureKind::Gate => "A gate",
        FeatureKind::Shade => "Shade",
        FeatureKind::Hazard => "A hazard",
        FeatureKind::Road => "A road",
        FeatureKind::NeighbourLine => "A neighbour line",
        FeatureKind::FarmBoundary => "The farm boundary",
    }
}

fn clean_text(s: Option<String>, max: usize, what: &str) -> ApiResult<Option<String>> {
    let s = s.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    if s.as_ref().is_some_and(|s| s.chars().count() > max) {
        return Err(ApiError::bad_request(format!("The {what} is too long.")));
    }
    Ok(s)
}

/// Validate and store a feature: geometry type per kind (400), paddock
/// exists (400), `active_until` after `active_from` (400), one farm boundary
/// (409). Publishing the change is the caller's (the route's) job.
pub async fn insert_feature(ctx: &Ctx, f: NewFeature) -> ApiResult<MapFeature> {
    let props = match f.props {
        Value::Null => Value::Object(Default::default()),
        v @ Value::Object(_) => v,
        _ => return Err(ApiError::bad_request("props must be an object.")),
    };
    let geometry = check_geometry(f.kind, &f.geometry, &props)?;
    let paddock_id = f.paddock_id.filter(|p| !p.is_empty());
    if let Some(p) = &paddock_id
        && ctx.store().get_paddock(p).await?.is_none()
    {
        return Err(ApiError::bad_request("No such paddock."));
    }
    if let (Some(from), Some(until)) = (f.active_from, f.active_until)
        && until <= from
    {
        return Err(ApiError::bad_request("active_until must be after active_from."));
    }
    const TAKEN: &str = "The farm already has a boundary. Edit that one instead.";
    if f.kind == FeatureKind::FarmBoundary && !list_features(ctx, Some(FeatureKind::FarmBoundary), None, None).await?.is_empty() {
        return Err(ApiError::conflict(TAKEN));
    }
    let t = now();
    let feature = MapFeature {
        id: id::new_id(FEATURE),
        kind: f.kind,
        name: clean_text(f.name, 200, "name")?,
        geometry,
        paddock_id,
        notes: clean_text(f.notes, 2000, "note")?,
        props,
        active_from: f.active_from,
        active_until: f.active_until,
        created_at: t,
        updated_at: t,
    };
    let res = sqlx::query(
        "INSERT INTO features (id, kind, name, geometry, paddock_id, notes, props, active_from, active_until, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&feature.id)
    .bind(feature.kind.as_db())
    .bind(&feature.name)
    .bind(serde_json::to_string(&feature.geometry).map_err(anyhow::Error::from)?)
    .bind(&feature.paddock_id)
    .bind(&feature.notes)
    .bind(feature.props.to_string())
    .bind(feature.active_from.as_ref().map(to_db))
    .bind(feature.active_until.as_ref().map(to_db))
    .bind(to_db(&feature.created_at))
    .bind(to_db(&feature.updated_at))
    .execute(ctx.db())
    .await;
    match res {
        Ok(_) => Ok(feature),
        Err(e) if e.as_database_error().is_some_and(|d| d.is_unique_violation()) => Err(ApiError::conflict(TAKEN)),
        Err(e) => Err(e.into()),
    }
}
