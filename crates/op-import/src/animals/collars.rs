//! Collars for many animals at once: `tag,collar name` rows (CSV) or JSON
//! create the collars, put each on its animal and return every key once.
//! A new key for one collar (owner only) is the way to print its card again.

use std::collections::{HashMap, HashSet};

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use op_core::store::animal_from_row;
use op_core::{Animal, ApiError, ApiResult, Collar, Ctx, Event, Identity, Role, id};
use op_ingest::NewLinked;
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::import::RowError;
use super::{mapping, table};

/// Batch ids: one per bulk link, the key for printing its cards.
pub const BATCH: &str = "bat";
const MAX_BYTES: usize = 1024 * 1024;
const MAX_ITEMS: usize = 2000;

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/collars/bulk", post(bulk).layer(DefaultBodyLimit::max(MAX_BYTES))).route("/api/collars/{id}/rekey", post(rekey))
}

/// A collar with what setting it up needs. `key` is shown once.
#[derive(Debug, Serialize, Deserialize)]
pub struct Linked {
    pub collar: Collar,
    pub key: String,
    pub endpoint: String,
    pub public_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Batch {
    pub batch_id: String,
    pub collars: Vec<Linked>,
}

#[derive(Deserialize)]
struct BulkQuery {
    herd_id: Option<String>,
}

#[derive(Deserialize)]
struct BulkJson {
    herd_id: String,
    items: Vec<BulkItem>,
}

#[derive(Deserialize)]
struct BulkItem {
    tag: Option<String>,
    name: Option<String>,
}

/// 400 with every row that needs fixing; nothing was created.
struct RowsError(Vec<RowError>);

impl IntoResponse for RowsError {
    fn into_response(self) -> Response {
        let n = self.0.len();
        let first: Vec<String> = self.0.iter().take(3).map(|e| format!("row {}: {}", e.row, e.error)).collect();
        let msg = format!("{n} row{} to fix, nothing linked. {}", if n == 1 { "" } else { "s" }, first.join(" "));
        (StatusCode::BAD_REQUEST, Json(json!({ "error": msg, "errors": self.0 }))).into_response()
    }
}

/// `tag,collar name` rows with or without a header. Returns (row number, tag, name).
fn csv_items(bytes: &[u8]) -> Result<Vec<(usize, Option<String>, Option<String>)>, String> {
    let t = table::read(bytes)?;
    let g = mapping::guess(&t.columns);
    let header = ["tag", "collar", "name"].iter().any(|f| g.contains_key(*f));
    let (tag_col, name_col, first_row, rows): (Option<usize>, Option<usize>, usize, Vec<Vec<String>>) = if header {
        let name = g.get("collar").or_else(|| g.get("name")).and_then(|c| t.col(c));
        (g.get("tag").and_then(|c| t.col(c)), name, 2, t.rows)
    } else {
        // No header: the first line is a row too.
        let mut rows = vec![t.columns.iter().map(|c| if c.starts_with("Column ") { String::new() } else { c.clone() }).collect::<Vec<_>>()];
        rows.extend(t.rows);
        (Some(0), Some(1), 1, rows)
    };
    let cell = |r: &Vec<String>, i: Option<usize>| i.and_then(|i| r.get(i)).map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
    Ok(rows.iter().enumerate().map(|(i, r)| (i + first_row, cell(r, tag_col), cell(r, name_col))).collect())
}

async fn bulk(
    State(ctx): State<Ctx>,
    identity: Identity,
    headers: HeaderMap,
    Query(q): Query<BulkQuery>,
    body: Bytes,
) -> Result<(StatusCode, Json<Batch>), Response> {
    let is_json = headers.get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).is_some_and(|v| v.contains("json"));
    let (herd_id, items) = if is_json {
        let b: BulkJson =
            serde_json::from_slice(&body).map_err(|e| ApiError::bad_request(format!("Expected {{herd_id, items: [{{tag, name}}]}}: {e}.")).into_response())?;
        (b.herd_id, b.items.into_iter().enumerate().map(|(i, it)| (i + 1, it.tag, it.name)).collect::<Vec<_>>())
    } else {
        let herd = q.herd_id.filter(|h| !h.is_empty()).ok_or_else(|| ApiError::bad_request("Say which herd: ?herd_id=").into_response())?;
        (herd, csv_items(&body).map_err(|e| ApiError::bad_request(e).into_response())?)
    };
    if items.is_empty() {
        return Err(ApiError::bad_request("The list is empty.").into_response());
    }
    if items.len() > MAX_ITEMS {
        return Err(ApiError::bad_request(format!("At most {MAX_ITEMS} collars at once.")).into_response());
    }
    let herd = ctx
        .store()
        .get_herd(&herd_id)
        .await
        .map_err(|e| ApiError::from(e).into_response())?
        .ok_or_else(|| ApiError::bad_request("No such herd.").into_response())?;
    let plan = plan(&ctx, &herd.id, &items).await.map_err(IntoResponse::into_response)?.map_err(|e| RowsError(e).into_response())?;
    let made = op_ingest::create_linked_collars(
        &ctx,
        &herd.id,
        &plan.iter().map(|(n, a)| NewLinked { name: n.clone(), animal_id: a.as_ref().map(|a| a.id.clone()) }).collect::<Vec<_>>(),
    )
    .await
    .map_err(IntoResponse::into_response)?;
    let (endpoint, public_key) = (op_ingest::collar_endpoint(&ctx), ctx.public_key_b64());
    let collars: Vec<Linked> = made
        .into_iter()
        .zip(plan)
        .map(|((collar, key), (_, a))| Linked { collar, key, endpoint: endpoint.clone(), public_key: public_key.clone(), tag: a.map(|a| a.tag) })
        .collect();
    let batch_id = id::new_id(BATCH);
    ctx.publish(Event::AnimalsChanged { herd_id: Some(herd.id.clone()) });
    let n = collars.len();
    let mut payload = json!({ "batch_id": batch_id, "count": n });
    if let Some(by) = identity.actor().name {
        payload["by"] = json!(by);
    }
    super::log(
        &ctx,
        "collars.linked",
        format!("Linked {n} collar{} in {}", if n == 1 { "" } else { "s" }, herd.name),
        payload,
        vec![("herd".into(), herd.id.clone())],
    )
    .await;
    Ok((StatusCode::CREATED, Json(Batch { batch_id, collars })))
}

/// Each item's collar name and animal, or every row that needs fixing.
async fn plan(ctx: &Ctx, herd_id: &str, items: &[(usize, Option<String>, Option<String>)]) -> ApiResult<Result<Vec<(String, Option<Animal>)>, Vec<RowError>>> {
    let animals: Vec<Animal> = sqlx::query("SELECT * FROM animals WHERE herd_id = ? AND removed_at IS NULL")
        .bind(herd_id)
        .fetch_all(ctx.db())
        .await?
        .iter()
        .map(animal_from_row)
        .collect::<anyhow::Result<_>>()?;
    let by_tag: HashMap<&str, &Animal> = animals.iter().map(|a| (a.tag.as_str(), a)).collect();
    let collar_names: HashMap<String, String> = sqlx::query_as::<_, (String, String)>("SELECT id, name FROM collars WHERE herd_id = ?")
        .bind(herd_id)
        .fetch_all(ctx.db())
        .await?
        .into_iter()
        .map(|(id, name)| (id, name))
        .collect();
    let mut taken: HashSet<String> = collar_names.values().map(|n| n.to_lowercase()).collect();
    let mut next_n = collar_names.len() + 1;
    let mut seen_tags: HashMap<String, usize> = HashMap::new();
    let mut seen_names: HashMap<String, usize> = HashMap::new();
    let (mut out, mut errors) = (Vec::with_capacity(items.len()), Vec::new());
    for (row, tag, name) in items {
        let mut one = || -> Result<(String, Option<Animal>), String> {
            let animal = match tag {
                Some(t) => {
                    if let Some(prev) = seen_tags.get(t) {
                        return Err(format!("Tag {t} is also on row {prev}."));
                    }
                    let a = by_tag.get(t.as_str()).ok_or_else(|| format!("No animal tagged {t} in this herd."))?;
                    if let Some(c) = &a.collar_id {
                        return Err(format!("{t} already wears {}.", collar_names.get(c).map_or("a collar", String::as_str)));
                    }
                    Some((*a).clone())
                }
                None => None,
            };
            let name = match name.clone().or_else(|| tag.clone()) {
                Some(n) => {
                    if let Some(prev) = seen_names.get(&n.to_lowercase()) {
                        return Err(format!("Collar {n} is also on row {prev}."));
                    }
                    if taken.contains(&n.to_lowercase()) {
                        return Err(format!("A collar named {n} is already in this herd."));
                    }
                    n
                }
                None => loop {
                    let n = format!("Collar {next_n}");
                    if !taken.contains(&n.to_lowercase()) && !seen_names.contains_key(&n.to_lowercase()) {
                        break n;
                    }
                    next_n += 1;
                },
            };
            if name.chars().count() > 200 {
                return Err("The collar name is too long.".into());
            }
            Ok((name, animal))
        };
        match one() {
            Ok((n, a)) => {
                if let Some(t) = tag {
                    seen_tags.insert(t.clone(), *row);
                }
                seen_names.insert(n.to_lowercase(), *row);
                taken.insert(n.to_lowercase());
                out.push((n, a));
            }
            Err(error) => errors.push(RowError { row: *row, error }),
        }
    }
    Ok(if errors.is_empty() { Ok(out) } else { Err(errors) })
}

/// A new key for one collar (owner only). The old key stops working at once.
async fn rekey(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>) -> ApiResult<Json<Linked>> {
    identity.require(Role::Owner)?;
    let (collar, key) = op_ingest::rekey_collar(&ctx, &id).await?;
    let tag = ctx.store().animal_for_collar(&collar.id).await?.map(|a| a.tag);
    let mut payload = json!({ "collar_id": collar.id });
    if let Some(by) = identity.actor().name {
        payload["by"] = json!(by);
    }
    super::log(&ctx, "collar.rekeyed", format!("New key for {}", collar.name), payload, vec![("collar".into(), collar.id.clone())]).await;
    Ok(Json(Linked { endpoint: op_ingest::collar_endpoint(&ctx), public_key: ctx.public_key_b64(), collar, key, tag }))
}
