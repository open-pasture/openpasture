//! Animals from a CSV: a preview (columns, a guessed mapping, the first rows
//! and every row error) kept in memory for 30 minutes, then a commit that
//! creates or updates animals by tag within one herd.
//!
//! Commits are idempotent: the same file and mapping a second time changes
//! nothing. An empty cell leaves the field as it is. A row with an error is
//! skipped and reported with its spreadsheet row number (the header is row 1);
//! the others go in.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::routing::post;
use axum::{Json, Router};
use op_core::animals::{self as rules, normalize_eid, parse_born, parse_sex};
use op_core::store::{animal_from_row, collar_from_row};
use op_core::time::{now, to_db};
use op_core::units::{Units, units_for_timezone};
use op_core::{Animal, ApiError, ApiJson, ApiResult, Collar, Ctx, DbEnum, Event, Identity, id};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::SqliteConnection;

use super::mapping::{self, FIELDS, Mapping};
use super::table::{self, Table};

/// Largest file a preview takes.
pub const MAX_BYTES: usize = 5 * 1024 * 1024;
/// Import ids (shared with K-files' imports).
pub const IMPORT: &str = "imp";
const TTL: Duration = Duration::from_secs(30 * 60);
/// Previews kept at once; the oldest goes first.
const KEEP: usize = 16;
const PREVIEW_ROWS: usize = 20;

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/animals/import/preview", post(preview).layer(DefaultBodyLimit::max(MAX_BYTES)))
        .route("/api/animals/import/{import_id}/commit", post(commit))
}

struct Stored {
    at: Instant,
    table: Arc<Table>,
}

static PREVIEWS: LazyLock<Mutex<HashMap<String, Stored>>> = LazyLock::new(Default::default);

fn keep(table: Table) -> String {
    let id = id::new_id(IMPORT);
    let mut m = PREVIEWS.lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, s| s.at.elapsed() < TTL);
    while m.len() >= KEEP {
        let Some(oldest) = m.iter().min_by_key(|(_, s)| s.at).map(|(k, _)| k.clone()) else { break };
        m.remove(&oldest);
    }
    m.insert(id.clone(), Stored { at: Instant::now(), table: Arc::new(table) });
    id
}

fn stored(id: &str) -> Option<Arc<Table>> {
    let mut m = PREVIEWS.lock().unwrap_or_else(|e| e.into_inner());
    m.retain(|_, s| s.at.elapsed() < TTL);
    m.get(id).map(|s| s.table.clone())
}

/// A row that can't go in, by spreadsheet row number (header = 1).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RowError {
    pub row: usize,
    pub error: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Preview {
    pub import_id: String,
    pub columns: Vec<String>,
    /// field → column: `tag`, `eid`, `name`, `breed`, `sex`, `born`, `collar`, `notes`.
    pub mapping: Mapping,
    /// The first rows as they are in the file, one cell per column.
    pub rows: Vec<Vec<String>>,
    pub total: usize,
    /// Rows that would be skipped with this mapping (and this herd, when given).
    pub errors: Vec<RowError>,
}

#[derive(Deserialize)]
struct PreviewQuery {
    herd_id: Option<String>,
}

async fn preview(State(ctx): State<Ctx>, Query(q): Query<PreviewQuery>, body: Bytes) -> ApiResult<Json<Preview>> {
    let table = table::read(&body).map_err(ApiError::bad_request)?;
    let mapping = mapping::guess(&table.columns);
    let herd = match q.herd_id.filter(|h| !h.is_empty()) {
        Some(h) if ctx.store().get_herd(&h).await?.is_some() => Some(h),
        Some(_) => return Err(ApiError::bad_request("No such herd.")),
        None => None,
    };
    let errors = if mapping.contains_key("tag") {
        let mut conn = ctx.db().acquire().await?;
        plan(&mut conn, &table, &mapping, herd.as_deref(), month_first(&ctx).await?).await?.errors
    } else {
        Vec::new()
    };
    let rows = table.rows.iter().take(PREVIEW_ROWS).cloned().collect();
    let (columns, total) = (table.columns.clone(), table.rows.len());
    let import_id = keep(table);
    Ok(Json(Preview { import_id, columns, mapping, rows, total, errors }))
}

#[derive(Deserialize)]
struct CommitBody {
    mapping: Mapping,
    herd_id: String,
}

#[derive(Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Committed {
    pub created: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub total: usize,
    pub errors: Vec<RowError>,
}

async fn commit(State(ctx): State<Ctx>, identity: Identity, Path(import_id): Path<String>, ApiJson(body): ApiJson<CommitBody>) -> ApiResult<Json<Committed>> {
    let table = stored(&import_id).ok_or_else(|| ApiError::not_found("This preview has expired. Choose the file again."))?;
    let herd = ctx.store().get_herd(&body.herd_id).await?.ok_or_else(|| ApiError::bad_request("No such herd."))?;
    let month_first = month_first(&ctx).await?;
    let started = Instant::now();

    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let plan = plan(&mut tx, &table, &body.mapping, Some(&herd.id), month_first).await?;
    let at = to_db(&now());
    let mut out = Committed { total: table.rows.len(), errors: plan.errors, ..Default::default() };
    let mut linked = Vec::new();
    for p in &plan.rows {
        match p {
            Planned::Same => out.unchanged += 1,
            Planned::Create(a) => {
                insert(&mut tx, a, &at).await?;
                out.created += 1;
            }
            Planned::Update(a, _) => {
                update(&mut tx, a).await?;
                out.updated += 1;
            }
        }
        if let Some((cid, aid)) = p.link() {
            sqlx::query("UPDATE collars SET animal_id = ?, parked_at = NULL, parked_reason = NULL WHERE id = ?").bind(aid).bind(cid).execute(&mut *tx).await?;
            linked.push(cid.to_owned());
        }
    }
    rules::sync_herd_count(&mut *tx, &herd.id).await?;
    tx.commit().await.map_err(conflict)?;

    if out.created + out.updated > 0 {
        ctx.publish(Event::AnimalsChanged { herd_id: Some(herd.id.clone()) });
        for cid in linked {
            if let Some(r) = sqlx::query("SELECT * FROM collars WHERE id = ?").bind(&cid).fetch_optional(ctx.db()).await? {
                ctx.publish(Event::Collar { collar: collar_from_row(&r)? });
            }
        }
        let n = out.created + out.updated;
        let title = format!("Imported {n} animal{} into {}", if n == 1 { "" } else { "s" }, herd.name);
        let mut payload = json!({ "import_id": import_id, "created": out.created, "updated": out.updated });
        if let Some(by) = identity.actor().name {
            payload["by"] = json!(by);
        }
        super::log(&ctx, "animals.imported", title, payload, vec![("herd".into(), herd.id.clone())]).await;
    }
    tracing::info!(herd = %herd.id, created = out.created, updated = out.updated, unchanged = out.unchanged, errors = out.errors.len(), ms = started.elapsed().as_millis() as u64, "animals imported");
    Ok(Json(out))
}

/// A unique index said no: something changed between the plan and the write.
fn conflict(e: sqlx::Error) -> ApiError {
    if e.as_database_error().is_some_and(|d| d.is_unique_violation()) { ApiError::conflict("Animals changed while importing. Import again.") } else { e.into() }
}

/// Dates like 4/1/2022 read month first on a US farm, day first elsewhere.
async fn month_first(ctx: &Ctx) -> anyhow::Result<bool> {
    Ok(ctx.store().get_farm().await?.is_some_and(|f| units_for_timezone(&f.timezone) == Units::Imperial))
}

enum Planned {
    Create(Animal),
    /// The animal as it will be, and whether its collar is put on now.
    Update(Animal, bool),
    Same,
}

impl Planned {
    /// (collar, animal) when this row puts a collar on its animal.
    fn link(&self) -> Option<(&str, &str)> {
        match self {
            Planned::Create(a) | Planned::Update(a, true) => a.collar_id.as_deref().map(|c| (c, a.id.as_str())),
            _ => None,
        }
    }
}

struct Plan {
    rows: Vec<Planned>,
    errors: Vec<RowError>,
}

/// What each row would do. Reads only, so the preview can run it too.
async fn plan(conn: &mut SqliteConnection, t: &Table, m: &Mapping, herd_id: Option<&str>, month_first: bool) -> ApiResult<Plan> {
    let mut idx: HashMap<&str, usize> = HashMap::new();
    for (field, col) in m {
        if !FIELDS.contains(&field.as_str()) {
            return Err(ApiError::bad_request(format!("{field} isn't an animal field.")));
        }
        let i = t.col(col).ok_or_else(|| ApiError::bad_request(format!("The file has no column {col}.")))?;
        idx.insert(field.as_str(), i);
    }
    if !idx.contains_key("tag") {
        return Err(ApiError::bad_request("Choose the column with the tags."));
    }

    let herd_animals: Vec<Animal> = match herd_id {
        Some(h) => sqlx::query("SELECT * FROM animals WHERE herd_id = ?")
            .bind(h)
            .fetch_all(&mut *conn)
            .await?
            .iter()
            .map(animal_from_row)
            .collect::<anyhow::Result<_>>()?,
        None => Vec::new(),
    };
    let by_tag: HashMap<&str, &Animal> = herd_animals.iter().filter(|a| a.removed_at.is_none()).map(|a| (a.tag.as_str(), a)).collect();
    let tag_of: HashMap<&str, &str> = herd_animals.iter().map(|a| (a.id.as_str(), a.tag.as_str())).collect();
    let eids: HashMap<String, (String, String)> = sqlx::query_as::<_, (String, String, String)>("SELECT eid, id, tag FROM animals WHERE eid IS NOT NULL")
        .fetch_all(&mut *conn)
        .await?
        .into_iter()
        .map(|(e, id, tag)| (e, (id, tag)))
        .collect();
    let collars: Vec<Collar> = if idx.contains_key("collar") {
        sqlx::query("SELECT * FROM collars").fetch_all(&mut *conn).await?.iter().map(collar_from_row).collect::<anyhow::Result<_>>()?
    } else {
        Vec::new()
    };

    let cell = |r: &[String], f: &str| idx.get(f).map(|&i| r[i].trim()).filter(|s| !s.is_empty()).map(str::to_owned);
    let mut seen_tags: HashMap<String, usize> = HashMap::new();
    let mut seen_eids: HashMap<String, usize> = HashMap::new();
    let mut seen_collars: HashMap<String, usize> = HashMap::new();
    let mut out = Plan { rows: Vec::with_capacity(t.rows.len()), errors: Vec::new() };

    for (i, r) in t.rows.iter().enumerate() {
        let row = i + 2;
        let one = || -> Result<(Planned, String, Option<String>, Option<String>), String> {
            let tag = rules::clean_tag(&cell(r, "tag").unwrap_or_default())?;
            if let Some(prev) = seen_tags.get(&tag) {
                return Err(format!("Tag {tag} is also on row {prev}."));
            }
            let before = by_tag.get(tag.as_str()).copied();
            let mut a = before.cloned().unwrap_or_else(|| Animal {
                id: id::new_id(id::ANIMAL),
                tag: tag.clone(),
                herd_id: herd_id.unwrap_or_default().to_owned(),
                ..Default::default()
            });
            if let Some(e) = cell(r, "eid") {
                let e = normalize_eid(&e)?;
                if let Some((other, other_tag)) = eids.get(&e)
                    && other != &a.id
                {
                    return Err(format!("EID {e} is on {other_tag}."));
                }
                if let Some(prev) = seen_eids.get(&e) {
                    return Err(format!("EID {e} is also on row {prev}."));
                }
                a.eid = Some(e);
            }
            if let Some(v) = cell(r, "name") {
                a.name = Some(v);
            }
            if let Some(v) = cell(r, "breed") {
                a.breed = Some(v);
            }
            if let Some(v) = cell(r, "notes") {
                a.notes = Some(v);
            }
            if let Some(v) = cell(r, "sex") {
                a.sex = Some(parse_sex(&v).ok_or_else(|| format!("Sex {v} isn't female, male or castrated."))?);
            }
            if let Some(v) = cell(r, "born") {
                a.born = Some(parse_born(&v, month_first)?);
            }
            let mut collar = None;
            if let Some(v) = cell(r, "collar") {
                let in_herd = |c: &&Collar| herd_id.is_none_or(|h| c.herd_id == h);
                let c = collars
                    .iter()
                    .find(|c| c.id == v)
                    .or_else(|| collars.iter().filter(in_herd).find(|c| c.name.eq_ignore_ascii_case(&v)))
                    .or_else(|| collars.iter().find(|c| c.name.eq_ignore_ascii_case(&v)))
                    .ok_or_else(|| format!("No collar named {v}."))?;
                if herd_id.is_some_and(|h| c.herd_id != h) {
                    return Err(format!("Collar {} is in another herd.", c.name));
                }
                if let Some(prev) = seen_collars.get(&c.id) {
                    return Err(format!("Collar {} is also on row {prev}.", c.name));
                }
                if let Some(on) = &c.animal_id
                    && on != &a.id
                {
                    return Err(format!("Collar {} is on {}.", c.name, tag_of.get(on.as_str()).copied().unwrap_or("another animal")));
                }
                if let Some(wears) = &a.collar_id
                    && wears != &c.id
                {
                    let name = collars.iter().find(|x| &x.id == wears).map_or("another collar", |x| x.name.as_str());
                    return Err(format!("{tag} wears {name}. Swap it on {tag}'s page."));
                }
                a.collar_id = Some(c.id.clone());
                collar = Some(c.id.clone());
            }
            rules::tidy(&mut a).map_err(|e| e.message)?;
            let eid = a.eid.clone();
            let planned = match before {
                None => Planned::Create(a),
                Some(b) if *b == a => Planned::Same,
                Some(b) => {
                    let new_collar = b.collar_id != a.collar_id;
                    Planned::Update(a, new_collar)
                }
            };
            Ok((planned, tag, eid, collar))
        };
        match one() {
            Ok((p, tag, eid, collar)) => {
                seen_tags.insert(tag, row);
                if let Some(e) = eid {
                    seen_eids.insert(e, row);
                }
                if let Some(c) = collar {
                    seen_collars.insert(c, row);
                }
                out.rows.push(p);
            }
            Err(error) => out.errors.push(RowError { row, error }),
        }
    }
    Ok(out)
}

async fn insert(tx: &mut SqliteConnection, a: &Animal, at: &str) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO animals (id, tag, name, herd_id, collar_id, created_at, eid, breed, sex, born, notes)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&a.id)
    .bind(&a.tag)
    .bind(&a.name)
    .bind(&a.herd_id)
    .bind(&a.collar_id)
    .bind(at)
    .bind(&a.eid)
    .bind(&a.breed)
    .bind(a.sex.map(|s| s.as_db()))
    .bind(a.born.map(|d| d.format("%Y-%m-%d").to_string()))
    .bind(&a.notes)
    .execute(&mut *tx)
    .await
    .map_err(conflict)?;
    Ok(())
}

async fn update(tx: &mut SqliteConnection, a: &Animal) -> ApiResult<()> {
    sqlx::query("UPDATE animals SET name = ?, collar_id = ?, eid = ?, breed = ?, sex = ?, born = ?, notes = ? WHERE id = ?")
        .bind(&a.name)
        .bind(&a.collar_id)
        .bind(&a.eid)
        .bind(&a.breed)
        .bind(a.sex.map(|s| s.as_db()))
        .bind(a.born.map(|d| d.format("%Y-%m-%d").to_string()))
        .bind(&a.notes)
        .bind(&a.id)
        .execute(&mut *tx)
        .await
        .map_err(conflict)?;
    Ok(())
}
