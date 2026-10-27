//! Operations: `GET /healthz`, the request log, disk usage and supervised
//! jobs for `/api/server`, raw telemetry retention, and the stop signals.

use std::net::SocketAddr;
use std::path::Path;
use std::time::Instant;

use axum::Json;
use axum::extract::{ConnectInfo, Request, State};
use axum::http::StatusCode;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use op_analytics::rollup::{RETENTION_KEY, Retention};
use op_core::tasks::{TaskInfo, TaskStatus};
use op_core::{ApiError, ApiJson, ApiResult, Ctx};
use serde::Serialize;

/// `GET /healthz`: no sign-in and nothing private.
///
/// | status | code | meaning |
/// |---|---|---|
/// | `ok` | 200 | the database answers and every job runs on time |
/// | `degraded` | 200 | a job is restarting or late, but no critical one for long |
/// | `down` | 503 | the database doesn't answer, the server is stopping, or a critical job ([`op_core::tasks::CRITICAL`]: fences, alerts) has been failing for [`op_core::tasks::CRITICAL_DOWN_AFTER`]; `failing` names it |
/// | `starting`, `migrating` | 503 | the database is opening or being upgraded (served by [`gated`]); `progress` says how far. Wait: it is working |
///
/// `openpasture health` (Docker's HEALTHCHECK) exits 0 for the first two and
/// for `starting`/`migrating`, so a long upgrade isn't killed half-way.
pub async fn healthz(State(ctx): State<Ctx>) -> Response {
    let db_ok = sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(ctx.db()).await.is_ok();
    let (code, health) = health_of(db_ok, ctx.is_shutting_down(), &ctx.tasks().list(), op_core::time::now());
    (code, Json(health)).into_response()
}

/// `/healthz`'s answer from what it found. Separate so the thresholds can be
/// tested without waiting them out.
pub fn health_of(db_ok: bool, stopping: bool, tasks: &[TaskInfo], at: chrono::DateTime<chrono::Utc>) -> (StatusCode, Health) {
    let failing: Vec<&'static str> = tasks.iter().filter(|t| t.critical_down(at)).map(|t| t.name).collect();
    let status = if !db_ok || stopping || !failing.is_empty() {
        "down"
    } else if tasks.iter().any(|t| t.status != TaskStatus::Running) {
        "degraded"
    } else {
        "ok"
    };
    if !failing.is_empty() {
        tracing::error!(jobs = ?failing, "critical background jobs have been failing for {} minutes", op_core::tasks::CRITICAL_DOWN_AFTER.as_secs() / 60);
    }
    let code = if status == "down" { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
    let jobs =
        tasks.iter().map(|t| JobHealth { name: t.name, status: t.status, restarts: t.restarts, critical: t.critical, down_since: t.down_since }).collect();
    (code, Health { status, version: crate::VERSION, db: if db_ok { "ok" } else { "error" }, failing, jobs })
}

#[derive(Debug, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub version: &'static str,
    pub db: &'static str,
    /// Critical jobs failing for too long.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub failing: Vec<&'static str>,
    pub jobs: Vec<JobHealth>,
}

#[derive(Debug, Serialize)]
pub struct JobHealth {
    pub name: &'static str,
    pub status: TaskStatus,
    pub restarts: u32,
    pub critical: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub down_since: Option<chrono::DateTime<chrono::Utc>>,
}

/// The app once it has started; until then [`gated`] answers for it.
pub struct Gate {
    app: std::sync::OnceLock<axum::Router>,
    progress: op_core::store::Progress,
}

impl Gate {
    pub fn new(progress: op_core::store::Progress) -> Self {
        Self { app: std::sync::OnceLock::new(), progress }
    }

    /// Hand every request to `app` from now on.
    pub fn open(&self, app: axum::Router) {
        let _ = self.app.set(app);
    }
}

/// Seconds a client is told to wait (`Retry-After`) while the server starts.
pub const RETRY_AFTER_S: u64 = 5;

/// Every request: to the app once it is ready, else 503 saying the server
/// is starting or upgrading its database (`/healthz` with its progress).
pub async fn gated(State(gate): State<std::sync::Arc<Gate>>, req: Request) -> Response {
    use tower::ServiceExt;
    if let Some(app) = gate.app.get() {
        return match app.clone().oneshot(req).await {
            Ok(res) => res,
            Err(never) => match never {},
        };
    }
    let progress = gate.progress.lock().unwrap_or_else(|e| e.into_inner()).clone();
    let status = if progress.phase == "snapshot" || progress.phase == "migrating" { "migrating" } else { "starting" };
    let retry = [(axum::http::header::RETRY_AFTER, RETRY_AFTER_S.to_string())];
    if req.uri().path() == "/healthz" {
        let body = serde_json::json!({ "status": status, "version": crate::VERSION, "progress": progress });
        return (StatusCode::SERVICE_UNAVAILABLE, retry, Json(body)).into_response();
    }
    let error = if status == "migrating" {
        "openpasture is upgrading its database. This can take a few minutes on a large farm; try again shortly."
    } else {
        "openpasture is starting. Try again in a few seconds."
    };
    (StatusCode::SERVICE_UNAVAILABLE, retry, Json(serde_json::json!({ "error": error, "status": status }))).into_response()
}

/// The job table for `/api/server`.
pub fn tasks(ctx: &Ctx) -> Vec<TaskInfo> {
    ctx.tasks().list()
}

/// Bytes the data dir uses, for `/api/server`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Disk {
    /// `openpasture.db`
    pub db_bytes: u64,
    /// `openpasture.db-wal` and `-shm`
    pub wal_bytes: u64,
    /// Parquet days under `telemetry/`
    pub telemetry_bytes: u64,
    /// Snapshots under `backups/`
    pub snapshots_bytes: u64,
    /// `logs/`
    pub logs_bytes: u64,
    /// The whole data dir.
    pub total_bytes: u64,
    /// Free space on the disk holding the data dir, where the OS says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub free_bytes: Option<u64>,
}

pub async fn disk(data_dir: &Path) -> Disk {
    let dir = data_dir.to_path_buf();
    tokio::task::spawn_blocking(move || disk_now(&dir)).await.unwrap_or_default()
}

fn disk_now(dir: &Path) -> Disk {
    let file = |name: &str| std::fs::metadata(dir.join(name)).map(|m| m.len()).unwrap_or(0);
    let db = op_core::store::DB_FILE;
    Disk {
        db_bytes: file(db),
        wal_bytes: file(&format!("{db}-wal")) + file(&format!("{db}-shm")),
        telemetry_bytes: tree_bytes(&dir.join("telemetry")),
        snapshots_bytes: tree_bytes(&dir.join(op_core::store::SNAPSHOT_DIR)),
        logs_bytes: tree_bytes(&dir.join("logs")),
        total_bytes: tree_bytes(dir),
        free_bytes: free_bytes(dir),
    }
}

fn tree_bytes(path: &Path) -> u64 {
    let Ok(meta) = std::fs::symlink_metadata(path) else { return 0 };
    if !meta.is_dir() {
        return meta.len();
    }
    std::fs::read_dir(path).map(|entries| entries.filter_map(|e| e.ok()).map(|e| tree_bytes(&e.path())).sum()).unwrap_or(0)
}

fn free_bytes(path: &Path) -> Option<u64> {
    op_core::store::free_bytes(path)
}

/// `GET /api/server/retention`: days of raw fixes, cues and health kept
/// (`null` keeps everything, the default).
pub async fn get_retention(State(ctx): State<Ctx>) -> ApiResult<Json<Retention>> {
    Ok(Json(op_analytics::rollup::retention(&ctx).await?))
}

/// `PUT /api/server/retention` (owner): the whole setting. Applied at the
/// next hourly rollup.
pub async fn put_retention(State(ctx): State<Ctx>, ApiJson(r): ApiJson<Retention>) -> ApiResult<Json<Retention>> {
    r.validate().map_err(ApiError::bad_request)?;
    ctx.store().set_setting(RETENTION_KEY, &r).await?;
    tracing::info!(retention = ?r, "raw telemetry retention changed");
    Ok(Json(r))
}

/// Query parameters whose values never reach the log.
const SECRET_PARAMS: &[&str] = &["token", "key", "code", "secret", "sig", "signature", "password"];

/// The path and query as logged: secret-looking query values replaced. A
/// name is matched as the server reads it, percent-decoded (`%74oken` is
/// `token`), and as often again as it still decodes, so no spelling of a
/// secret's name gets its value into the log.
pub fn redact(path: &str, query: Option<&str>) -> String {
    let Some(q) = query.filter(|q| !q.is_empty()) else { return path.to_owned() };
    let q: Vec<String> = q
        .split('&')
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            if !v.is_empty() && secret_name(k) { format!("{k}=REDACTED") } else { kv.to_owned() }
        })
        .collect();
    format!("{path}?{}", q.join("&"))
}

fn secret_name(raw: &str) -> bool {
    let mut name = raw.to_owned();
    for _ in 0..4 {
        let lower = name.to_lowercase();
        if SECRET_PARAMS.iter().any(|s| lower.contains(s)) {
            return true;
        }
        let decoded = percent_decode(&name);
        if decoded == name {
            return false;
        }
        name = decoded;
    }
    // Still decoding after that many rounds: nobody spells a name so, but a
    // value hidden that way stays out of the log.
    true
}

/// `application/x-www-form-urlencoded` decoding of one name: `%XX` and `+`.
fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match b[i] {
            b'%' if i + 2 < b.len() => match (hex(b[i + 1]), hex(b[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push((h * 16 + l) as u8);
                    i += 3;
                    continue;
                }
                _ => out.push(b'%'),
            },
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// One line per request (target `request`): method, path with secrets
/// redacted, status, milliseconds and the peer. Health probes log at debug.
pub async fn log_request(req: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = req.method().clone();
    let path = redact(req.uri().path(), req.uri().query());
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0.ip().to_string()).unwrap_or_default();
    let res = next.run(req).await;
    let ms = started.elapsed().as_millis() as u64;
    let status = res.status().as_u16();
    if path == "/healthz" {
        tracing::debug!(target: "request", %method, %path, status, ms, %peer);
    } else {
        tracing::info!(target: "request", %method, %path, status, ms, %peer);
    }
    res
}

/// Resolves on Ctrl-C (SIGINT) or SIGTERM (`docker stop`, systemd, Railway).
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(e) => {
                tracing::warn!("can't listen for SIGTERM: {e}");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => tracing::info!("interrupted, shutting down"),
        _ = term => tracing::info!("SIGTERM, shutting down"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_in_the_query_are_redacted() {
        assert_eq!(redact("/api/live", Some("token=abc&x=1")), "/api/live?token=REDACTED&x=1");
        assert_eq!(redact("/join", Some("invite_code=123")), "/join?invite_code=REDACTED");
        assert_eq!(redact("/mcp", Some("scope=brain")), "/mcp?scope=brain");
        assert_eq!(redact("/api/state", None), "/api/state");
        assert_eq!(redact("/x", Some("api_key=")), "/x?api_key=");
    }

    #[test]
    fn encoded_and_mixed_case_secret_names_are_redacted_too() {
        assert_eq!(redact("/api/missing", Some("token=a&%74oken=b")), "/api/missing?token=REDACTED&%74oken=REDACTED");
        assert_eq!(redact("/x", Some("%54%4F%4B%45%4E=b")), "/x?%54%4F%4B%45%4E=REDACTED");
        assert_eq!(redact("/x", Some("TOKEN=b&Api%5FKey=c")), "/x?TOKEN=REDACTED&Api%5FKey=REDACTED");
        // Encoded twice: the server reads `%74oken`, but keep it out anyway.
        assert_eq!(redact("/x", Some("%2574oken=b")), "/x?%2574oken=REDACTED");
        assert_eq!(redact("/x", Some("invite+code=b")), "/x?invite+code=REDACTED");
        // Not secrets, and stray percents, stay.
        assert_eq!(redact("/x", Some("scope=brain&q=50%25&%zz=1&tail%")), "/x?scope=brain&q=50%25&%zz=1&tail%");
    }
}
