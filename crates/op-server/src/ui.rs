//! The built UI, embedded. Unknown paths outside `/api`, `/collar`, `/mcp`,
//! `/v1` and `/hooks` fall back to index.html.

use axum::http::{HeaderValue, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use op_core::ApiError;
use rust_embed::RustEmbed;

const CSP: &str = "frame-ancestors 'none'; object-src 'none'; base-uri 'self'; form-action 'self'";

#[derive(RustEmbed)]
#[folder = "$OP_UI_DIR"]
struct Assets;

fn is_api(path: &str) -> bool {
    ["api", "collar", "mcp", "v1", "hooks"].iter().any(|p| path == *p || path.starts_with(&format!("{p}/")))
}

pub async fn handler(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if is_api(path) {
        return ApiError::new(StatusCode::NOT_FOUND, "Not found.").into_response();
    }
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(res) = file(path) {
        return res;
    }
    // Missing hashed assets are real 404s; everything else is a client route.
    if path.starts_with("assets/") {
        return (StatusCode::NOT_FOUND, "Not found.").into_response();
    }
    file("index.html").unwrap_or_else(|| (StatusCode::NOT_FOUND, "Not found.").into_response())
}

fn file(path: &str) -> Option<Response> {
    let f = Assets::get(path)?;
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let cache = if path.starts_with("assets/") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let mut res = f.data.into_owned().into_response();
    let h = res.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_str(mime.as_ref()).unwrap_or(HeaderValue::from_static("application/octet-stream")));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if mime.essence_str() == "text/html" {
        // Nothing may frame the app or swap its base, and no plugins. Map
        // tiles, place search and imports reach other hosts, so sources stay open.
        h.insert(header::CONTENT_SECURITY_POLICY, HeaderValue::from_static(CSP));
    }
    Some(res)
}
