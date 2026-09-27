//! Position history: preview (labels matched to animals), commit (points to
//! `imported_fixes`, daily dwell to `imported_paddock_days`), list, undo and
//! tracks.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use axum::Json;
use axum::extract::{Multipart, Path, Query, State};
use axum::http::StatusCode;
use chrono::{DateTime, NaiveDate, Utc};
use chrono_tz::Tz;
use op_core::{Actor, Animal, ApiError, ApiJson, ApiResult, Ctx, Event, Identity};
use serde::{Deserialize, Serialize};
use sqlx::Row;

use super::dwell::{Dwell, PaddockIndex, date_of_day};
use super::pending::{self, Pending};
use super::points::{self, Mapping, Parsed, Pt, Source};
use super::{IMPORT, POSITION_FILE_MAX, upload};

/// Points per label drawn in the preview.
const PREVIEW_TRACK_POINTS: usize = 200;
/// Rows per INSERT statement (9 values each, well under SQLite's variable cap).
const INSERT_BATCH: usize = 500;
/// Rows per page when reading imported fixes back.
const PAGE: usize = 20_000;
/// Points per write transaction.
const CHUNK_ROWS: usize = 5_000;

#[derive(Debug, Serialize)]
pub struct Label {
    pub label: String,
    pub points: usize,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// The animal this label matched by tag, EID or collar name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub animal_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct PreviewTrack {
    pub label: String,
    /// `[lon, lat, t_unix_seconds]`, thinned evenly over the label's points.
    pub points: Vec<[f64; 3]>,
}

#[derive(Debug, Serialize)]
pub struct PositionPreview {
    pub import_id: String,
    pub file: String,
    pub source: Source,
    /// CSV headers or GeoJSON properties; empty for GPX.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub columns: Vec<String>,
    pub mapping: Mapping,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rows: Vec<Vec<String>>,
    pub total: usize,
    pub points: usize,
    pub labels: Vec<Label>,
    pub tracks: Vec<PreviewTrack>,
    /// Some timestamps carry no offset; they were read in `zone`.
    pub needs_zone: bool,
    pub zone: String,
    pub errors: Vec<String>,
}

async fn farm_zone(ctx: &Ctx) -> anyhow::Result<Tz> {
    Ok(ctx.store().get_farm().await?.and_then(|f| f.timezone.parse().ok()).unwrap_or(chrono_tz::UTC))
}

fn parse_zone(z: Option<&str>) -> ApiResult<Option<Tz>> {
    match z.map(str::trim).filter(|z| !z.is_empty()) {
        None => Ok(None),
        Some(z) => z.parse().map(Some).map_err(|_| ApiError::bad_request(format!("\"{z}\" isn't a time zone."))),
    }
}

async fn parse_file(file_name: String, bytes: Arc<Vec<u8>>, mapping: Option<Mapping>, zone: Tz) -> ApiResult<Parsed> {
    tokio::task::spawn_blocking(move || points::parse(&file_name, &bytes, mapping.as_ref(), zone))
        .await
        .map_err(|e| ApiError::internal(format!("reading the file failed: {e}")))?
        .map_err(ApiError::bad_request)
}

// ---------------------------------------------------------------- matching labels

/// Labels to animals: by tag, then EID, then the name of the collar it wears,
/// then a numeric tag without leading zeros. A label two animals could be is
/// left unmatched (an active animal wins over a removed one).
struct Matcher {
    by_tag: HashMap<String, Vec<Animal>>,
    by_eid: HashMap<String, Animal>,
    by_collar: HashMap<String, Animal>,
    by_number: HashMap<String, Vec<Animal>>,
    by_id: HashMap<String, Animal>,
}

fn key(s: &str) -> String {
    s.trim().to_lowercase()
}

impl Matcher {
    async fn load(ctx: &Ctx) -> anyhow::Result<Self> {
        let animals = ctx.store().list_animals(None).await?;
        let mut m = Matcher { by_tag: HashMap::new(), by_eid: HashMap::new(), by_collar: HashMap::new(), by_number: HashMap::new(), by_id: HashMap::new() };
        for a in &animals {
            m.by_tag.entry(key(&a.tag)).or_default().push(a.clone());
            if let Some(e) = a.eid.as_deref().filter(|e| !e.is_empty()) {
                m.by_eid.insert(key(e), a.clone());
            }
            let t = a.tag.trim();
            if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
                m.by_number.entry(t.trim_start_matches('0').to_owned()).or_default().push(a.clone());
            }
            m.by_id.insert(a.id.clone(), a.clone());
        }
        let rows = sqlx::query("SELECT name, animal_id FROM collars WHERE animal_id IS NOT NULL").fetch_all(ctx.db()).await?;
        for r in rows {
            let (name, animal): (String, String) = (r.try_get("name")?, r.try_get("animal_id")?);
            if let Some(a) = m.by_id.get(&animal) {
                m.by_collar.insert(key(&name), a.clone());
            }
        }
        Ok(m)
    }

    fn one(list: Option<&Vec<Animal>>) -> Option<&Animal> {
        let list = list?;
        let active: Vec<&Animal> = list.iter().filter(|a| a.removed_at.is_none()).collect();
        match (active.len(), list.len()) {
            (1, _) => Some(active[0]),
            (0, 1) => Some(&list[0]),
            _ => None,
        }
    }

    fn find(&self, label: &str) -> Option<&Animal> {
        let k = key(label);
        if let Some(a) = Self::one(self.by_tag.get(&k)) {
            return Some(a);
        }
        if let Some(a) = self.by_eid.get(&k).or_else(|| self.by_collar.get(&k)) {
            return Some(a);
        }
        let t = label.trim();
        if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
            return Self::one(self.by_number.get(t.trim_start_matches('0')));
        }
        None
    }
}

// ---------------------------------------------------------------- preview

fn thin(points: &[Pt], n: usize) -> Vec<[f64; 3]> {
    if points.len() <= n {
        return points.iter().map(|p| [p.lon, p.lat, p.t as f64 / 1000.0]).collect();
    }
    (0..n).map(|i| points[i * (points.len() - 1) / (n - 1)]).map(|p| [p.lon, p.lat, p.t as f64 / 1000.0]).collect()
}

fn build_preview(import_id: String, file: String, parsed: Parsed, zone: Tz, matcher: &Matcher) -> PositionPreview {
    let mut by_label: Vec<Vec<Pt>> = vec![Vec::new(); parsed.labels.len()];
    for p in &parsed.points {
        by_label[p.label as usize].push(*p);
    }
    for pts in &mut by_label {
        pts.sort_by_key(|p| p.t);
    }
    let mut labels = Vec::new();
    let mut tracks = Vec::new();
    for (i, pts) in by_label.iter().enumerate() {
        let (Some(first), Some(last)) = (pts.first(), pts.last()) else { continue };
        let name = parsed.labels[i].clone();
        let animal = matcher.find(&name);
        labels.push(Label {
            label: name.clone(),
            points: pts.len(),
            from: op_core::time::from_unix_ms(first.t),
            to: op_core::time::from_unix_ms(last.t),
            animal_id: animal.map(|a| a.id.clone()),
            tag: animal.map(|a| a.tag.clone()),
        });
        tracks.push(PreviewTrack { label: name, points: thin(pts, PREVIEW_TRACK_POINTS) });
    }
    PositionPreview {
        import_id,
        file,
        source: parsed.source.unwrap_or(Source::Csv),
        columns: parsed.columns,
        mapping: parsed.mapping,
        rows: parsed.rows,
        total: parsed.total,
        points: parsed.points.len(),
        labels,
        tracks,
        needs_zone: parsed.needs_zone,
        zone: zone.name().to_owned(),
        errors: parsed.errors,
    }
}

pub async fn preview(State(ctx): State<Ctx>, form: Multipart) -> ApiResult<Json<PositionPreview>> {
    let up = upload(form, POSITION_FILE_MAX).await?;
    let field = |k: &str| up.fields.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let mapping: Option<Mapping> =
        field("mapping").map(|m| serde_json::from_str(&m).map_err(|e| ApiError::bad_request(format!("`mapping` isn't valid: {e}.")))).transpose()?;
    let zone = match parse_zone(field("zone").as_deref())? {
        Some(z) => z,
        None => farm_zone(&ctx).await?,
    };
    let bytes = Arc::new(up.bytes);
    let parsed = parse_file(up.name.clone(), bytes.clone(), mapping, zone).await?;
    let import_id = op_core::id::new_id(IMPORT);
    pending::put(&import_id, Pending::Positions { file_name: up.name.clone(), bytes });
    let matcher = Matcher::load(&ctx).await?;
    Ok(Json(build_preview(import_id, up.name, parsed, zone, &matcher)))
}

#[derive(Debug, Default, Deserialize)]
pub struct Reread {
    #[serde(default)]
    pub mapping: Option<Mapping>,
    #[serde(default)]
    pub zone: Option<String>,
}

fn positions_pending(id: &str) -> ApiResult<(String, Arc<Vec<u8>>)> {
    let item = pending::get(id).ok_or_else(|| ApiError::not_found("That preview has expired. Import the file again."))?;
    match item.as_ref() {
        Pending::Positions { file_name, bytes } => Ok((file_name.clone(), bytes.clone())),
        _ => Err(ApiError::bad_request("That import isn't a position file.")),
    }
}

pub async fn repreview(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(body): ApiJson<Reread>) -> ApiResult<Json<PositionPreview>> {
    let (file_name, bytes) = positions_pending(&id)?;
    let zone = match parse_zone(body.zone.as_deref())? {
        Some(z) => z,
        None => farm_zone(&ctx).await?,
    };
    let parsed = parse_file(file_name.clone(), bytes, body.mapping, zone).await?;
    let matcher = Matcher::load(&ctx).await?;
    Ok(Json(build_preview(id, file_name, parsed, zone, &matcher)))
}

// ---------------------------------------------------------------- commit

#[derive(Debug, Default, Deserialize)]
pub struct Commit {
    #[serde(default)]
    pub mapping: Option<Mapping>,
    #[serde(default)]
    pub zone: Option<String>,
    /// Label → animal id, overriding the match; `null` leaves the label out.
    #[serde(default)]
    pub animals: HashMap<String, Option<String>>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PositionImport {
    pub id: String,
    pub file_name: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub zone: Option<String>,
    pub fixes: i64,
    pub animals: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
    pub created_by: Actor,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Serialize)]
pub struct Committed {
    pub import: PositionImport,
    /// Points not stored because that animal already has one at that instant
    /// (an earlier import, or the file repeats itself).
    pub duplicates: usize,
    /// Labels left out: no animal matched or chosen.
    pub skipped: Vec<String>,
    pub errors: Vec<String>,
}

pub async fn commit(
    State(ctx): State<Ctx>,
    identity: Identity,
    Path(id): Path<String>,
    ApiJson(body): ApiJson<Commit>,
) -> ApiResult<(StatusCode, Json<Committed>)> {
    let (file_name, bytes) = positions_pending(&id)?;
    let zone_given = parse_zone(body.zone.as_deref())?;
    let zone = match zone_given {
        Some(z) => z,
        None => farm_zone(&ctx).await?,
    };
    let parsed = parse_file(file_name.clone(), bytes, body.mapping.clone(), zone).await?;
    let source = parsed.source.unwrap_or(Source::Csv);

    // Label → animal.
    let matcher = Matcher::load(&ctx).await?;
    let mut chosen: Vec<Option<Animal>> = Vec::with_capacity(parsed.labels.len());
    let mut skipped = Vec::new();
    for label in &parsed.labels {
        let animal = match body.animals.get(label) {
            Some(Some(animal_id)) => Some(matcher.by_id.get(animal_id).cloned().ok_or_else(|| ApiError::bad_request(format!("No such animal for {label}.")))?),
            Some(None) => None,
            None => matcher.find(label).cloned(),
        };
        if animal.is_none() {
            skipped.push(label.clone());
        }
        chosen.push(animal);
    }
    if chosen.iter().all(Option::is_none) {
        return Err(ApiError::bad_request("No track matches an animal. Choose an animal for at least one."));
    }

    // Store the points. The preview is taken first, so a second commit of it can't run alongside.
    let mut rows: Vec<(&Animal, Pt)> = parsed.points.iter().filter_map(|p| Some((chosen[p.label as usize].as_ref()?, *p))).collect();
    rows.sort_by(|a, b| a.0.id.cmp(&b.0.id).then(a.1.t.cmp(&b.1.t)));
    let held = pending::take(&id).ok_or_else(|| ApiError::not_found("That preview has expired. Import the file again."))?;
    let meta = Meta { file_name, source, zone: parsed.needs_zone.then(|| zone.name().to_owned()), by: identity.actor() };
    let stored = match store_import(&ctx, &id, &rows, &meta).await {
        Ok(s) => s,
        Err(e) => {
            // Leave nothing half-stored, and the preview ready for another try.
            if let Err(c) = forget(&ctx, &id).await {
                tracing::error!(import_id = %id, "cleaning up a failed import: {c:#}");
            }
            pending::restore(&id, held);
            return Err(e.into());
        }
    };
    let (import, herds) = (stored.import, stored.herds);
    if import.fixes == 0 {
        // Nothing new: no empty import in the list.
        forget(&ctx, &id).await?;
        return Err(ApiError::conflict("Every point in this file is already imported."));
    }
    for h in &herds {
        ctx.publish(Event::AnimalsChanged { herd_id: Some(h.clone()) });
    }
    tracing::info!(import_id = %id, fixes = import.fixes, animals = import.animals, "imported position history");
    let duplicates = rows.len() - import.fixes as usize;
    Ok((StatusCode::CREATED, Json(Committed { import, duplicates, skipped, errors: parsed.errors })))
}

struct Meta {
    file_name: String,
    source: Source,
    zone: Option<String>,
    by: Actor,
}

struct Stored {
    import: PositionImport,
    herds: BTreeSet<String>,
}

/// Points in transactions of [`CHUNK_ROWS`] (a season for 250 animals never
/// holds the write lock long enough to stall collar reports), then the daily
/// dwell and the import record in one last transaction.
async fn store_import(ctx: &Ctx, id: &str, rows: &[(&Animal, Pt)], meta: &Meta) -> anyhow::Result<Stored> {
    let mut stored = 0usize;
    for chunk in rows.chunks(CHUNK_ROWS) {
        let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
        for batch in chunk.chunks(INSERT_BATCH) {
            let mut sql = String::from("INSERT OR IGNORE INTO imported_fixes (import_id, animal_id, herd_id, t, at, lon, lat, accuracy_m, source) VALUES ");
            sql.push_str(&vec!["(?, ?, ?, ?, ?, ?, ?, ?, ?)"; batch.len()].join(", "));
            let mut q = sqlx::query(&sql);
            for (a, p) in batch {
                q = q
                    .bind(id)
                    .bind(&a.id)
                    .bind(&a.herd_id)
                    .bind(p.t)
                    .bind(op_core::time::to_db(&op_core::time::from_unix_ms(p.t)))
                    .bind(op_geo::projection::round7(p.lon))
                    .bind(op_geo::projection::round7(p.lat))
                    .bind(p.accuracy_m)
                    .bind(meta.source.as_str());
            }
            stored += q.execute(&mut *tx).await?.rows_affected() as usize;
        }
        tx.commit().await?;
        tokio::task::yield_now().await;
    }

    // Daily dwell from what this import stored, against today's paddocks, read back in
    // pages of (animal, t) so memory stays bounded at any size.
    let paddocks = PaddockIndex::new(&ctx.store().list_paddocks().await?);
    let mut dwell = Dwell::default();
    let mut animals = BTreeSet::new();
    let mut herds = BTreeSet::new();
    let (mut from_t, mut to_t) = (i64::MAX, i64::MIN);
    let mut cursor: (String, i64) = (String::new(), i64::MIN);
    loop {
        let page = sqlx::query(
            "SELECT animal_id, herd_id, t, lon, lat FROM imported_fixes WHERE import_id = ? AND (animal_id, t) > (?, ?) ORDER BY animal_id, t LIMIT ?",
        )
        .bind(id)
        .bind(&cursor.0)
        .bind(cursor.1)
        .bind(PAGE as i64)
        .fetch_all(ctx.db())
        .await?;
        for r in &page {
            let (animal, herd, t, lon, lat): (String, String, i64, f64, f64) = (r.try_get(0)?, r.try_get(1)?, r.try_get(2)?, r.try_get(3)?, r.try_get(4)?);
            dwell.push(&animal, &herd, paddocks.locate([lon, lat]), t);
            from_t = from_t.min(t);
            to_t = to_t.max(t);
            herds.insert(herd);
            cursor = (animal.clone(), t);
            animals.insert(animal);
        }
        if page.len() < PAGE {
            break;
        }
    }
    let days: Vec<_> = dwell.finish().into_iter().collect();

    let import = PositionImport {
        id: id.to_owned(),
        file_name: meta.file_name.clone(),
        source: meta.source.as_str().to_owned(),
        zone: meta.zone.clone(),
        fixes: stored as i64,
        animals: animals.len() as i64,
        from: (stored > 0).then(|| op_core::time::from_unix_ms(from_t)),
        to: (stored > 0).then(|| op_core::time::from_unix_ms(to_t)),
        created_by: meta.by.clone(),
        created_at: op_core::time::now(),
    };
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    for batch in days.chunks(INSERT_BATCH) {
        let mut sql = String::from("INSERT INTO imported_paddock_days (date, herd_id, collar_id, paddock_id, fixes, dwell_s, last_t, import_id) VALUES ");
        sql.push_str(&vec!["(?, ?, ?, ?, ?, ?, ?, ?)"; batch.len()].join(", "));
        let mut q = sqlx::query(&sql);
        for ((day, herd, animal, paddock), agg) in batch {
            q = q.bind(date_of_day(*day)).bind(herd).bind(animal).bind(paddock).bind(agg.fixes).bind(agg.dwell_ms as f64 / 1000.0).bind(agg.last_t).bind(id);
        }
        q.execute(&mut *tx).await?;
    }
    sqlx::query(
        "INSERT INTO position_imports (id, file_name, source, zone, fixes, animals, from_t, to_t, created_by, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&import.id)
    .bind(&import.file_name)
    .bind(&import.source)
    .bind(&import.zone)
    .bind(import.fixes)
    .bind(import.animals)
    .bind((stored > 0).then_some(from_t))
    .bind((stored > 0).then_some(to_t))
    .bind(serde_json::to_string(&import.created_by)?)
    .bind(op_core::time::to_db(&import.created_at))
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Stored { import, herds })
}

/// Everything an import stored: its points in transactions of [`CHUNK_ROWS`], then its
/// days and record together (a crash midway leaves the record, so it can be deleted again).
async fn forget(ctx: &Ctx, id: &str) -> anyhow::Result<()> {
    loop {
        let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
        let n = sqlx::query("DELETE FROM imported_fixes WHERE (animal_id, t) IN (SELECT animal_id, t FROM imported_fixes WHERE import_id = ? LIMIT ?)")
            .bind(id)
            .bind(CHUNK_ROWS as i64)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        tx.commit().await?;
        if n == 0 {
            break;
        }
        tokio::task::yield_now().await;
    }
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    sqlx::query("DELETE FROM imported_paddock_days WHERE import_id = ?").bind(id).execute(&mut *tx).await?;
    sqlx::query("DELETE FROM position_imports WHERE id = ?").bind(id).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

// ---------------------------------------------------------------- list, undo

fn import_from_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<PositionImport> {
    let by: String = r.try_get("created_by")?;
    let from: Option<i64> = r.try_get("from_t")?;
    let to: Option<i64> = r.try_get("to_t")?;
    Ok(PositionImport {
        id: r.try_get("id")?,
        file_name: r.try_get("file_name")?,
        source: r.try_get("source")?,
        zone: r.try_get("zone")?,
        fixes: r.try_get("fixes")?,
        animals: r.try_get("animals")?,
        from: from.map(op_core::time::from_unix_ms),
        to: to.map(op_core::time::from_unix_ms),
        created_by: serde_json::from_str(&by)?,
        created_at: op_core::time::from_db(&r.try_get::<String, _>("created_at")?)?,
    })
}

pub async fn list(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<PositionImport>>> {
    let rows = sqlx::query("SELECT * FROM position_imports ORDER BY created_at DESC, id DESC").fetch_all(ctx.db()).await?;
    Ok(Json(rows.iter().map(import_from_row).collect::<anyhow::Result<_>>()?))
}

/// One imported day: an animal's fixes and dwell in a paddock ("" = outside every paddock).
#[derive(Debug, Serialize, PartialEq)]
pub struct ImportedDay {
    pub date: String,
    pub animal_id: String,
    pub paddock_id: String,
    pub fixes: i64,
    pub dwell_s: f64,
}

#[derive(Debug, Serialize)]
pub struct ImportDetail {
    #[serde(flatten)]
    pub import: PositionImport,
    pub days: Vec<ImportedDay>,
}

pub async fn get_one(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<ImportDetail>> {
    let row = sqlx::query("SELECT * FROM position_imports WHERE id = ?")
        .bind(&id)
        .fetch_optional(ctx.db())
        .await?
        .ok_or_else(|| ApiError::not_found("No such import."))?;
    let import = import_from_row(&row)?;
    let rows =
        sqlx::query("SELECT date, collar_id, paddock_id, fixes, dwell_s FROM imported_paddock_days WHERE import_id = ? ORDER BY date, collar_id, paddock_id")
            .bind(&id)
            .fetch_all(ctx.db())
            .await?;
    let days = rows
        .iter()
        .map(|r| Ok(ImportedDay { date: r.try_get(0)?, animal_id: r.try_get(1)?, paddock_id: r.try_get(2)?, fixes: r.try_get(3)?, dwell_s: r.try_get(4)? }))
        .collect::<Result<_, sqlx::Error>>()?;
    Ok(Json(ImportDetail { import, days }))
}

pub async fn remove(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    if sqlx::query("SELECT 1 FROM position_imports WHERE id = ?").bind(&id).fetch_optional(ctx.db()).await?.is_none() {
        return Err(ApiError::not_found("No such import."));
    }
    let herds: Vec<String> = sqlx::query_scalar("SELECT DISTINCT herd_id FROM imported_paddock_days WHERE import_id = ?").bind(&id).fetch_all(ctx.db()).await?;
    forget(&ctx, &id).await?;
    for h in herds {
        ctx.publish(Event::AnimalsChanged { herd_id: Some(h) });
    }
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------- tracks

#[derive(Debug, Default, Deserialize)]
pub struct TrackQuery {
    pub animal_id: Option<String>,
    pub import_id: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub max_points: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Track {
    pub animal_id: String,
    /// `[lon, lat, t_unix_seconds]`: the first point in each of `max_points`
    /// equal time buckets, plus the last.
    pub points: Vec<[f64; 3]>,
}

type SqliteQuery<'q> = sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments<'q>>;

/// `sql` with the time range and the optional animal and import bound, in that order.
fn filtered<'q>(sql: &'q str, lo: i64, hi: i64, animal: Option<&str>, import: Option<&str>) -> SqliteQuery<'q> {
    let mut q = sqlx::query(sql).bind(lo).bind(hi);
    if let Some(a) = animal {
        q = q.bind(a.to_owned());
    }
    if let Some(i) = import {
        q = q.bind(i.to_owned());
    }
    q
}

fn parse_bound(s: Option<&str>, name: &str) -> ApiResult<Option<i64>> {
    let Some(s) = s.map(str::trim).filter(|s| !s.is_empty()) else { return Ok(None) };
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(Some(t.timestamp_millis()));
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(Some(d.and_hms_opt(0, 0, 0).expect("midnight").and_utc().timestamp_millis()));
    }
    Err(ApiError::bad_request(format!("`{name}` must be RFC 3339 or YYYY-MM-DD.")))
}

pub async fn tracks(State(ctx): State<Ctx>, Query(q): Query<TrackQuery>) -> ApiResult<Json<Vec<Track>>> {
    let from = parse_bound(q.from.as_deref(), "from")?;
    let to = parse_bound(q.to.as_deref(), "to")?;
    let max_points: usize = match q.max_points.as_deref().filter(|s| !s.is_empty()) {
        None => 2000,
        Some(s) => s.parse::<usize>().map_err(|_| ApiError::bad_request("`max_points` must be a number."))?.clamp(2, 50_000),
    };
    let animal = q.animal_id.as_deref().filter(|s| !s.is_empty()).map(str::to_owned);
    let import = q.import_id.as_deref().filter(|s| !s.is_empty()).map(str::to_owned);
    let mut filter = String::from(" WHERE t >= ? AND t < ?");
    if animal.is_some() {
        filter.push_str(" AND animal_id = ?");
    }
    if import.is_some() {
        filter.push_str(" AND import_id = ?");
    }
    let (lo, hi) = (from.unwrap_or(i64::MIN), to.unwrap_or(i64::MAX));
    let bind = |sql| filtered(sql, lo, hi, animal.as_deref(), import.as_deref());
    // The span the buckets cover: the asked range, else the data's own.
    let span_sql = format!("SELECT MIN(t), MAX(t) FROM imported_fixes{filter}");
    let span = bind(&span_sql).fetch_one(ctx.db()).await?;
    let (Some(min_t), Some(max_t)) = (span.try_get::<Option<i64>, _>(0)?, span.try_get::<Option<i64>, _>(1)?) else { return Ok(Json(Vec::new())) };
    let (start, end) = (from.unwrap_or(min_t), to.unwrap_or(max_t + 1));
    let width = ((end - start) / max_points as i64).max(1);

    struct Acc {
        points: Vec<[f64; 3]>,
        bucket: Option<i64>,
        last: Option<(i64, [f64; 3])>,
        emitted: i64,
    }
    let mut accs: BTreeMap<String, Acc> = BTreeMap::new();
    let page_sql = format!("SELECT animal_id, t, lon, lat FROM imported_fixes{filter} AND (animal_id, t) > (?, ?) ORDER BY animal_id, t LIMIT ?");
    let mut cursor: (String, i64) = (String::new(), i64::MIN);
    loop {
        let page = bind(&page_sql).bind(cursor.0.clone()).bind(cursor.1).bind(PAGE as i64).fetch_all(ctx.db()).await?;
        for r in &page {
            let (animal_id, t, lon, lat): (String, i64, f64, f64) = (r.try_get(0)?, r.try_get(1)?, r.try_get(2)?, r.try_get(3)?);
            let pt = [lon, lat, t as f64 / 1000.0];
            let bucket = (t - start).div_euclid(width);
            cursor = (animal_id.clone(), t);
            let a = accs.entry(animal_id).or_insert(Acc { points: Vec::new(), bucket: None, last: None, emitted: i64::MIN });
            if a.bucket != Some(bucket) {
                a.points.push(pt);
                a.bucket = Some(bucket);
                a.emitted = t;
            }
            a.last = Some((t, pt));
        }
        if page.len() < PAGE {
            break;
        }
    }
    Ok(Json(
        accs.into_iter()
            .map(|(animal_id, mut a)| {
                if let Some((t, pt)) = a.last
                    && t != a.emitted
                {
                    a.points.push(pt);
                }
                Track { animal_id, points: a.points }
            })
            .collect(),
    ))
}
