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

/// `GET /healthz`: no sign-in and nothing private. `ok` when the database
/// answers and every job runs on time, `degraded` when a job is restarting
/// or late (still 200: the server answers), `down` (503) when the database
/// doesn't answer or the server is stopping.
pub async fn healthz(State(ctx): State<Ctx>) -> Response {
    let db_ok = sqlx::query_scalar::<_, i64>("SELECT 1").fetch_one(ctx.db()).await.is_ok();
    let jobs: Vec<JobHealth> = ctx.tasks().list().into_iter().map(|t| JobHealth { name: t.name, status: t.status, restarts: t.restarts }).collect();
    let status = if !db_ok || ctx.is_shutting_down() {
        "down"
    } else if jobs.iter().any(|j| j.status != TaskStatus::Running) {
        "degraded"
    } else {
        "ok"
    };
    let code = if status == "down" { StatusCode::SERVICE_UNAVAILABLE } else { StatusCode::OK };
    (code, Json(Health { status, version: crate::VERSION, db: if db_ok { "ok" } else { "error" }, jobs })).into_response()
}

#[derive(Serialize)]
struct Health {
    status: &'static str,
    version: &'static str,
    db: &'static str,
    jobs: Vec<JobHealth>,
}

#[derive(Serialize)]
struct JobHealth {
    name: &'static str,
    status: TaskStatus,
    restarts: u32,
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

#[cfg(unix)]
fn free_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a writable statvfs.
    let rc = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    (rc == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

#[cfg(not(unix))]
fn free_bytes(_path: &Path) -> Option<u64> {
    None
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

/// The path and query as logged: secret-looking query values replaced.
pub fn redact(path: &str, query: Option<&str>) -> String {
    let Some(q) = query.filter(|q| !q.is_empty()) else { return path.to_owned() };
    let q: Vec<String> = q
        .split('&')
        .map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            let lower = k.to_ascii_lowercase();
            if !v.is_empty() && SECRET_PARAMS.iter().any(|s| lower.contains(s)) { format!("{k}=REDACTED") } else { kv.to_owned() }
        })
        .collect();
    format!("{path}?{}", q.join("&"))
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
}
