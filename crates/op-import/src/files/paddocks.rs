//! `POST /api/import/paddocks/preview` and `/commit`.

use axum::Json;
use axum::extract::{Multipart, State};
use axum::http::StatusCode;
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Paddock, PaddockStatus};
use serde::{Deserialize, Serialize};

use super::pending::{self, Pending};
use super::read::{Draft, read_paddock_file};
use super::{IMPORT, PADDOCK_FILE_MAX, upload};

#[derive(Debug, Serialize)]
pub struct PaddockPreview {
    pub import_id: String,
    pub file: String,
    pub drafts: Vec<Draft>,
    /// Polygons that couldn't become drafts, one sentence each.
    pub errors: Vec<String>,
}

pub async fn preview(State(_ctx): State<Ctx>, form: Multipart) -> ApiResult<Json<PaddockPreview>> {
    let up = upload(form, PADDOCK_FILE_MAX).await?;
    let name = up.name.clone();
    let read = tokio::task::spawn_blocking(move || read_paddock_file(&up.name, &up.bytes))
        .await
        .map_err(|e| ApiError::internal(format!("reading the file failed: {e}")))?
        .map_err(ApiError::bad_request)?;
    let import_id = op_core::id::new_id(IMPORT);
    pending::put(&import_id, Pending::Paddocks { drafts: read.drafts.clone() });
    Ok(Json(PaddockPreview { import_id, file: name, drafts: read.drafts, errors: read.errors }))
}

#[derive(Debug, Deserialize)]
pub struct Commit {
    pub import_id: String,
    /// Indexes into the preview's drafts to save.
    pub keep: Vec<usize>,
    /// New names for the kept drafts, in `keep` order; empty or absent keeps the file's names.
    #[serde(default)]
    pub names: Vec<Option<String>>,
}

#[derive(Debug, Serialize)]
pub struct Committed {
    pub paddocks: Vec<Paddock>,
}

pub async fn commit(State(ctx): State<Ctx>, ApiJson(body): ApiJson<Commit>) -> ApiResult<(StatusCode, Json<Committed>)> {
    if ctx.store().get_farm().await?.is_none() {
        return Err(ApiError::bad_request("Create the farm first."));
    }
    let item = pending::get(&body.import_id).ok_or_else(|| ApiError::not_found("That preview has expired. Import the file again."))?;
    let Pending::Paddocks { drafts } = item.as_ref() else {
        return Err(ApiError::bad_request("That import isn't a paddock file."));
    };
    if body.keep.is_empty() {
        return Err(ApiError::bad_request("Keep at least one paddock."));
    }
    if !body.names.is_empty() && body.names.len() != body.keep.len() {
        return Err(ApiError::bad_request("Give one name per kept paddock."));
    }
    let mut seen = std::collections::HashSet::new();
    let mut paddocks = Vec::with_capacity(body.keep.len());
    let now = op_core::time::now();
    for (i, &k) in body.keep.iter().enumerate() {
        let d = drafts.get(k).ok_or_else(|| ApiError::bad_request(format!("There is no draft {k}.")))?;
        if !seen.insert(k) {
            return Err(ApiError::bad_request(format!("Draft {k} is listed twice.")));
        }
        let name = body.names.get(i).cloned().flatten().map(|n| n.trim().to_owned()).filter(|n| !n.is_empty()).unwrap_or_else(|| d.name.clone());
        if name.len() > 200 {
            return Err(ApiError::bad_request("The paddock name is too long."));
        }
        paddocks.push(Paddock {
            id: op_core::id::new_id(op_core::id::PADDOCK),
            name,
            geometry: d.geometry.clone(),
            area_ha: d.area_ha,
            status: PaddockStatus::Resting,
            notes: None,
            grazed_until: None,
            created_at: now,
            props: d.props.clone(),
        });
    }
    // Taken just before saving, so a second commit of the same preview finds nothing.
    let held = pending::take(&body.import_id).ok_or_else(|| ApiError::not_found("That preview has expired. Import the file again."))?;
    for (i, p) in paddocks.iter().enumerate() {
        if let Err(e) = ctx.store().insert_paddock(p).await {
            for done in &paddocks[..i] {
                let _ = ctx.store().delete_paddock(&done.id).await;
            }
            pending::restore(&body.import_id, held);
            return Err(e.into());
        }
    }
    tracing::info!(import_id = %body.import_id, paddocks = paddocks.len(), "imported paddocks");
    Ok((StatusCode::CREATED, Json(Committed { paddocks })))
}
