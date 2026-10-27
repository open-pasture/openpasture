//! Wire log: each collar's last exchanges on `/collar/v1` (reports,
//! downloads, acks, and the configs that ride on report replies), for
//! debugging a real collar on the bench or in the field. A malformed report
//! the server refused shows here with the reason it was given.
//!
//! In memory only, and bounded: [`PER_COLLAR`] entries a collar, at most
//! [`MAX_COLLARS`] collars (the one quiet longest goes first), bodies cut to
//! [`BODY_BYTES`] with keys redacted. Requests with no known collar key are
//! not kept. Collar ids are unique, so one log serves every data dir in the
//! process; reads check the collar exists in the caller's.
//!
//! - `GET /api/collars/:id/wire?limit=` (managers and up): oldest first.
//! - Live event `collar_wire { collar_id, entry }` (managers and up).

use std::collections::{HashMap, VecDeque};
use std::sync::{LazyLock, Mutex};
use std::time::Instant;

use axum::body::{Body, Bytes};
use axum::extract::{Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::{ApiError, ApiResult, Ctx, Event, Identity, Role, keys};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::db;

/// Entries kept a collar: about 30 report and download round trips.
pub const PER_COLLAR: usize = 64;
/// Collars kept at once.
pub const MAX_COLLARS: usize = 4096;
/// Longest body kept, bytes (after redaction).
pub const BODY_BYTES: usize = 512;
/// Largest body read through; bigger ones are refused before any route sees them.
const READ_LIMIT: usize = 16 << 20;
/// JSON keys whose values never go in the log.
const SECRET_KEYS: [&str; 6] = ["key", "k", "token", "app_token", "secret", "password"];

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/collars/{id}/wire", get(list))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    /// Collar to server.
    In,
    /// Server to collar.
    Out,
}

/// One message on the wire.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct WireEntry {
    pub at: DateTime<Utc>,
    pub dir: Dir,
    /// `report`, `boundary`, `ack` or `config` (a config on a report reply).
    pub route: &'static str,
    /// HTTP status, on `out` entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<u16>,
    /// Body size as sent.
    pub bytes: usize,
    /// The body (a download request's query), keys redacted, cut to [`BODY_BYTES`].
    pub body: String,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
    /// What it was, in a few words: counts, versions, or the error given.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Time the server took, on `out` entries.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ms: Option<u64>,
}

#[derive(Default)]
struct Log {
    collars: HashMap<String, Ring>,
    /// Key hash → collar id. A key only ever belongs to one collar.
    keys: HashMap<String, String>,
    tick: u64,
}

struct Ring {
    entries: VecDeque<WireEntry>,
    touched: u64,
}

static LOG: LazyLock<Mutex<Log>> = LazyLock::new(Mutex::default);

fn log() -> std::sync::MutexGuard<'static, Log> {
    LOG.lock().unwrap_or_else(|e| e.into_inner())
}

/// Keep an entry for a collar and send it live.
fn push(ctx: &Ctx, collar_id: &str, entry: WireEntry) {
    let value = serde_json::to_value(&entry).unwrap_or(Value::Null);
    {
        let mut log = log();
        log.tick += 1;
        let tick = log.tick;
        if !log.collars.contains_key(collar_id) && log.collars.len() >= MAX_COLLARS {
            let quiet = log.collars.iter().min_by_key(|(_, r)| r.touched).map(|(id, _)| id.clone());
            if let Some(id) = quiet {
                log.collars.remove(&id);
            }
        }
        let ring = log.collars.entry(collar_id.to_owned()).or_insert_with(|| Ring { entries: VecDeque::with_capacity(PER_COLLAR), touched: 0 });
        if ring.entries.len() == PER_COLLAR {
            ring.entries.pop_front();
        }
        ring.entries.push_back(entry);
        ring.touched = tick;
    }
    ctx.publish(Event::CollarWire { collar_id: collar_id.to_owned(), entry: value });
}

/// A collar's last `limit` entries, oldest first.
pub fn entries(collar_id: &str, limit: usize) -> Vec<WireEntry> {
    let log = log();
    let Some(ring) = log.collars.get(collar_id) else { return Vec::new() };
    let skip = ring.entries.len().saturating_sub(limit);
    ring.entries.iter().skip(skip).cloned().collect()
}

#[derive(Deserialize)]
struct Limit {
    limit: Option<usize>,
}

#[derive(Serialize)]
struct Listed {
    collar_id: String,
    entries: Vec<WireEntry>,
}

async fn list(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>, Query(q): Query<Limit>) -> ApiResult<Json<Listed>> {
    identity.require(Role::Manager)?;
    if db::get_collar(ctx.db(), &id).await?.is_none() {
        return Err(ApiError::not_found("No such collar."));
    }
    let limit = q.limit.unwrap_or(PER_COLLAR).clamp(1, PER_COLLAR);
    Ok(Json(Listed { entries: entries(&id, limit), collar_id: id }))
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    let v = headers.get(axum::http::header::AUTHORIZATION)?.to_str().ok()?;
    let key = v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer "))?.trim();
    (!key.is_empty()).then_some(key)
}

/// The collar a request's key belongs to, remembered after the first lookup.
async fn collar_of(ctx: &Ctx, headers: &HeaderMap) -> Option<String> {
    let hash = keys::hash_key(bearer(headers)?);
    if let Some(id) = log().keys.get(&hash) {
        return Some(id.clone());
    }
    let id: String = sqlx::query_scalar("SELECT id FROM collars WHERE key_hash = ?").bind(&hash).fetch_optional(ctx.db()).await.ok()??;
    let mut log = log();
    if log.keys.len() >= 2 * MAX_COLLARS {
        log.keys.clear();
    }
    log.keys.insert(hash, id.clone());
    Some(id)
}

/// Middleware over the whole app: logs `/collar/v1` exchanges, passes
/// everything else straight through.
pub async fn record(State(ctx): State<Ctx>, req: Request, next: Next) -> Response {
    let route = match req.uri().path() {
        "/collar/v1/report" => "report",
        "/collar/v1/boundary" => "boundary",
        "/collar/v1/ack" => "ack",
        _ => return next.run(req).await,
    };
    let Some(collar_id) = collar_of(&ctx, req.headers()).await else { return next.run(req).await };
    let query = req.uri().query().unwrap_or_default().to_owned();
    let (parts, body) = req.into_parts();
    let Ok(body) = axum::body::to_bytes(body, READ_LIMIT).await else {
        return ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "The request is too large.").into_response();
    };
    let shown = if route == "boundary" { Bytes::from(query.clone()) } else { body.clone() };
    push(&ctx, &collar_id, entry(Dir::In, route, None, &shown, body.len(), note_in(route, &body, &query), None));

    let started = Instant::now();
    let res = next.run(Request::from_parts(parts, Body::from(body))).await;
    let (parts, body) = res.into_parts();
    let body = axum::body::to_bytes(body, READ_LIMIT).await.unwrap_or_default();
    let ms = started.elapsed().as_millis() as u64;
    let status = parts.status;
    let json: Option<Value> = serde_json::from_slice(&body).ok();
    push(&ctx, &collar_id, entry(Dir::Out, route, Some(status.as_u16()), &body, body.len(), note_out(route, status, json.as_ref()), Some(ms)));
    if route == "report"
        && status.is_success()
        && let Some(config) = json.as_ref().and_then(|v| v.get("config")).filter(|c| c.is_object())
    {
        let text = config.to_string();
        let note = config.get("version").map(|v| format!("config v{v}"));
        push(&ctx, &collar_id, entry(Dir::Out, "config", Some(status.as_u16()), text.as_bytes(), text.len(), note, None));
    }
    Response::from_parts(parts, Body::from(body))
}

fn entry(dir: Dir, route: &'static str, status: Option<u16>, shown: &[u8], bytes: usize, note: Option<String>, ms: Option<u64>) -> WireEntry {
    let (body, truncated) = redacted(shown);
    WireEntry { at: Utc::now(), dir, route, status, bytes, body, truncated, note, ms }
}

/// The body as text, secrets replaced, cut at a character boundary.
pub fn redacted(bytes: &[u8]) -> (String, bool) {
    let Ok(text) = std::str::from_utf8(bytes) else { return (format!("({} bytes, not text)", bytes.len()), false) };
    let secret = SECRET_KEYS.iter().any(|k| text.contains(&format!("\"{k}\"")));
    let mut text = match serde_json::from_str::<Value>(text) {
        Ok(mut v) if secret => {
            redact(&mut v);
            v.to_string()
        }
        _ => text.to_owned(),
    };
    if text.len() <= BODY_BYTES {
        return (text, false);
    }
    let mut cut = BODY_BYTES;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    (text, true)
}

fn redact(v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if SECRET_KEYS.contains(&k.as_str()) {
                    *val = Value::String("[redacted]".into());
                } else {
                    redact(val);
                }
            }
        }
        Value::Array(items) => items.iter_mut().for_each(redact),
        _ => {}
    }
}

/// Only counts and the fields a note names are read.
#[derive(Deserialize)]
struct ReportCounts {
    #[serde(default)]
    fixes: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    cues: Vec<serde::de::IgnoredAny>,
    #[serde(default)]
    episodes: Vec<serde::de::IgnoredAny>,
    boundary_version: Option<u32>,
}

#[derive(Deserialize)]
struct AckFields {
    version: Option<u32>,
    status: Option<String>,
    code: Option<String>,
}

fn note_in(route: &str, body: &[u8], query: &str) -> Option<String> {
    match route {
        "report" => {
            let r: ReportCounts = serde_json::from_slice(body).ok()?;
            let v = r.boundary_version.map(|v| format!(", enforcing v{v}")).unwrap_or_default();
            Some(format!("{} fixes, {} cues, {} episodes{v}", r.fixes.len(), r.cues.len(), r.episodes.len()))
        }
        "boundary" => {
            let have = query.split('&').find_map(|p| p.strip_prefix("have=")).unwrap_or("0");
            Some(format!("has v{have}"))
        }
        "ack" => {
            let a: AckFields = serde_json::from_slice(body).ok()?;
            let code = a.code.map(|c| format!(" {c}")).unwrap_or_default();
            Some(format!("v{} {}{code}", a.version?, a.status?))
        }
        _ => None,
    }
}

fn note_out(route: &str, status: StatusCode, json: Option<&Value>) -> Option<String> {
    if !status.is_success() {
        return json.and_then(|v| v.get("error")).and_then(Value::as_str).map(str::to_owned);
    }
    match (route, status) {
        ("boundary", StatusCode::NO_CONTENT) => Some("nothing newer".into()),
        ("boundary", _) => {
            let v = json?;
            Some(format!("v{} {}", v.get("version")?, v.get("command_id")?.as_str()?))
        }
        ("report", _) => {
            let v = json?;
            let latest = v.get("latest_version").filter(|l| !l.is_null()).map_or("none".into(), |l| format!("v{l}"));
            Some(format!("latest {latest}"))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_redacted_and_bodies_cut() {
        let (body, cut) = redacted(br#"{"v":1,"c":"col_1","k":"secret-key","nested":[{"token":"t"}]}"#);
        assert!(!cut);
        assert!(!body.contains("secret-key") && !body.contains("\"t\""), "{body}");
        assert!(body.contains("[redacted]") && body.contains("col_1"));

        // No secret named: left exactly as sent, even when it isn't JSON.
        assert_eq!(redacted(b"have=3&free=12"), ("have=3&free=12".into(), false));
        assert_eq!(redacted(&[0xff, 0xfe]).0, "(2 bytes, not text)");

        let long = format!("{{\"a\":\"{}\"}}", "é".repeat(BODY_BYTES));
        let (body, cut) = redacted(long.as_bytes());
        assert!(cut && body.len() <= BODY_BYTES && body.starts_with("{\"a\":\"é"));
    }

    #[test]
    fn notes_say_what_went_by() {
        let rep = br#"{"boundary_version":4,"fixes":[{},{}],"cues":[{}]}"#;
        assert_eq!(note_in("report", rep, "").as_deref(), Some("2 fixes, 1 cues, 0 episodes, enforcing v4"));
        assert_eq!(note_in("boundary", b"", "have=7&free=3").as_deref(), Some("has v7"));
        assert_eq!(note_in("ack", br#"{"version":5,"status":"rejected","code":"bad_sig"}"#, "").as_deref(), Some("v5 rejected bad_sig"));
        let err = serde_json::json!({"error": "Invalid fix point."});
        assert_eq!(note_out("report", StatusCode::BAD_REQUEST, Some(&err)).as_deref(), Some("Invalid fix point."));
        assert_eq!(note_out("boundary", StatusCode::NO_CONTENT, None).as_deref(), Some("nothing newer"));
        let cmd = serde_json::json!({"version": 9, "command_id": "bnd_1"});
        assert_eq!(note_out("boundary", StatusCode::OK, Some(&cmd)).as_deref(), Some("v9 bnd_1"));
        assert_eq!(note_out("report", StatusCode::OK, Some(&serde_json::json!({"latest_version": null}))).as_deref(), Some("latest none"));
    }

    #[tokio::test]
    async fn rings_are_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let id = "col_wire_ring_test";
        for i in 0..PER_COLLAR + 10 {
            let e = entry(Dir::In, "report", None, b"{}", 2, Some(i.to_string()), None);
            push(&ctx, id, e);
        }
        let all = entries(id, usize::MAX);
        assert_eq!(all.len(), PER_COLLAR);
        assert_eq!(all[0].note.as_deref(), Some("10"));
        assert_eq!(entries(id, 3).iter().map(|e| e.note.clone().unwrap()).collect::<Vec<_>>(), ["71", "72", "73"]);
    }
}
