//! Paddock files and position history (K-files): `/api/import/*`.
//!
//! - Paddocks: `POST /api/import/paddocks/preview` (multipart file ≤ 20 MB:
//!   GeoJSON, KML, KMZ or a zipped shapefile) gives drafts; `POST
//!   /api/import/paddocks/commit` saves the kept ones as paddocks.
//! - Positions: `POST /api/import/positions/preview` (multipart file ≤ 64 MB:
//!   CSV, GPX or GeoJSON points) gives labels matched to animals; `POST
//!   /api/import/positions/{id}/preview` re-reads it with another mapping or
//!   zone; `POST /api/import/positions/{id}/commit` stores the points in
//!   `imported_fixes` and their daily dwell in `imported_paddock_days`.
//!   `GET /api/import/positions` lists imports, `GET …/{id}` shows one with its
//!   paddock days, `DELETE …/{id}` undoes one,
//!   `GET /api/import/positions/tracks` serves imported tracks.

use axum::Router;
use axum::extract::{DefaultBodyLimit, Multipart};
use axum::http::StatusCode;
use axum::routing::{get, post};
use op_core::{ApiError, ApiResult, Ctx};

mod crs;
mod dwell;
mod geojson;
mod kml;
// @X1 collar data wins over imported history
pub mod overlap;
mod paddocks;
mod pending;
mod points;
mod positions;
mod proj;
mod read;
mod shp;

pub use read::{Draft, read_paddock_file};

/// Import ids (shared with K-animals' animal imports).
pub const IMPORT: &str = "imp";

const PADDOCK_FILE_MAX: usize = 20 * 1024 * 1024;
const POSITION_FILE_MAX: usize = 64 * 1024 * 1024;

pub fn router() -> Router<Ctx> {
    let paddock_files = Router::new().route("/api/import/paddocks/preview", post(paddocks::preview)).layer(DefaultBodyLimit::max(PADDOCK_FILE_MAX + 64 * 1024));
    let position_files =
        Router::new().route("/api/import/positions/preview", post(positions::preview)).layer(DefaultBodyLimit::max(POSITION_FILE_MAX + 64 * 1024));
    Router::new()
        .merge(paddock_files)
        .merge(position_files)
        .route("/api/import/paddocks/commit", post(paddocks::commit))
        .route("/api/import/positions", get(positions::list))
        .route("/api/import/positions/tracks", get(positions::tracks))
        .route("/api/import/positions/{id}", get(positions::get_one).delete(positions::remove))
        .route("/api/import/positions/{id}/preview", post(positions::repreview))
        .route("/api/import/positions/{id}/commit", post(positions::commit))
}

/// An uploaded file: its name, its bytes, and the other form fields.
pub(crate) struct Upload {
    pub name: String,
    pub bytes: Vec<u8>,
    pub fields: Vec<(String, String)>,
}

/// Read a multipart form holding one file (the field with a file name, or
/// the one called `file`) and any text fields.
pub(crate) async fn upload(mut form: Multipart, max: usize) -> ApiResult<Upload> {
    let too_big = || ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, format!("The file is larger than {} MB.", max / (1024 * 1024)));
    let read_err = |e: axum::extract::multipart::MultipartError| {
        if e.status() == StatusCode::PAYLOAD_TOO_LARGE { too_big() } else { ApiError::bad_request(format!("The upload can't be read: {}", e.body_text())) }
    };
    let mut file: Option<(String, Vec<u8>)> = None;
    let mut fields = Vec::new();
    while let Some(mut field) = form.next_field().await.map_err(read_err)? {
        let is_file = field.file_name().is_some() || field.name() == Some("file");
        if is_file && file.is_none() {
            let name = field.file_name().or(field.name()).unwrap_or("file").to_owned();
            let mut bytes = Vec::new();
            while let Some(chunk) = field.chunk().await.map_err(read_err)? {
                if bytes.len() + chunk.len() > max {
                    return Err(too_big());
                }
                bytes.extend_from_slice(&chunk);
            }
            file = Some((name, bytes));
        } else if !is_file {
            let key = field.name().unwrap_or_default().to_owned();
            let text = field.text().await.map_err(read_err)?;
            fields.push((key, text));
        }
    }
    let (name, bytes) = file.ok_or_else(|| ApiError::bad_request("Send the file as multipart form data."))?;
    if bytes.is_empty() {
        return Err(ApiError::bad_request("The file is empty."));
    }
    Ok(Upload { name, bytes, fields })
}
