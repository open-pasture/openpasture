//! The openpasture server: one axum app with every crate's routes, the
//! `/api/live` WebSocket, and the embedded UI. `openpasture serve` and the
//! desktop app both call [`serve`].

mod auth;
mod collar_address;
mod limits;
pub mod session;
pub mod live;
pub mod logging;
pub mod ops;
mod ui;
pub mod work;
// @HUB
// @HUB-UI
// @E-lib
// @E-srv
// @J
mod policy;
// @A-engine
// @A-notify
// @D
// @K-animals
// @K-files
// @I
// @B
// @G
// @P
pub mod coalesce;
// @Q
// @C
// @F
// @S
// @A3
// @H
// @L
// @M
// @Z

use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use axum::Router;
use axum::extract::State;
use axum::routing::get;
use op_core::{ApiResult, Ctx};
use serde::Serialize;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tower_http::cors::CorsLayer;

pub use auth::{AUTH_FAILURES_PER_MINUTE, is_loopback};

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// Default: `OPENPASTURE_DATA_DIR`, else the platform data dir.
    pub data_dir: Option<PathBuf>,
    /// Default: `settings.server.bind` (127.0.0.1).
    pub bind: Option<String>,
    /// Default: `settings.server.port` (7878).
    pub port: Option<u16>,
    /// Let the OS pick a free port (the desktop app). Overrides `port`.
    pub free_port: bool,
    /// CORS and Origin checks also accept the Vite dev server
    /// (localhost:5173). Default: on in debug builds.
    pub cors: Option<bool>,
}

/// Static facts for `/api/server`.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub data_dir: PathBuf,
    pub bind: String,
    pub port: u16,
    pub lan_url: Option<String>,
}

pub struct ServerHandle {
    url: String,
    addr: SocketAddr,
    lan_url: Option<String>,
    ctx: Ctx,
    shutdown_tx: Option<oneshot::Sender<()>>,
    join: JoinHandle<anyhow::Result<()>>,
}

impl ServerHandle {
    /// Local URL for this machine, e.g. `http://127.0.0.1:7878`.
    pub fn url(&self) -> &str {
        &self.url
    }
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }
    /// The LAN URL when bound beyond localhost.
    pub fn lan_url(&self) -> Option<&str> {
        self.lan_url.as_deref()
    }
    pub fn ctx(&self) -> &Ctx {
        &self.ctx
    }

    /// A one-time sign-in URL for this machine's owner (see [`session`]):
    /// the desktop app opens its window on it; `openpasture serve` prints it.
    pub async fn sign_in_url(&self, label: &str) -> anyhow::Result<String> {
        session::sign_in_url(&self.ctx, &self.url, label).await
    }

    /// Stop background tasks and the server: waits up to 5 s for open
    /// connections and 10 s for supervised jobs to finish, then closes the
    /// database (checkpointing the WAL) and removes `server.json`.
    pub async fn shutdown(mut self) -> anyhow::Result<()> {
        self.ctx.shutdown();
        if let Some(tx) = self.shutdown_tx.take() {
            let _ = tx.send(());
        }
        let served = match tokio::time::timeout(Duration::from_secs(5), &mut self.join).await {
            Ok(res) => res.context("server task panicked").and_then(|r| r),
            Err(_) => {
                self.join.abort();
                Ok(())
            }
        };
        if !self.ctx.tasks().join(Duration::from_secs(10)).await {
            tracing::warn!("background jobs still running after 10 s; stopped");
        }
        self.ctx.db().close().await;
        op_core::instance::remove_server_file(self.ctx.data_dir());
        tracing::info!("openpasture stopped");
        served
    }

    /// Run until the server stops on its own (normally never).
    pub async fn wait(self) -> anyhow::Result<()> {
        self.join.await.context("server task panicked")?
    }
}

/// Open the data dir, run migrations, start every crate's background tasks,
/// and serve. Returns once the listener is bound.
pub async fn serve(opts: ServeOptions) -> anyhow::Result<ServerHandle> {
    let data_dir = opts.data_dir.clone().unwrap_or_else(op_core::default_data_dir);
    let ctx = Ctx::open(&data_dir).await?;
    let settings = ctx.settings().await?;
    let bind = opts.bind.clone().unwrap_or(settings.server.bind.clone());
    let port = if opts.free_port { 0 } else { opts.port.unwrap_or(settings.server.port) };

    let listener = TcpListener::bind((bind.as_str(), port)).await.with_context(|| format!("binding {bind}:{port}"))?;
    let addr = listener.local_addr()?;
    let local_ip = if addr.ip().is_unspecified() { IpAddr::from([127, 0, 0, 1]) } else { addr.ip() };
    let url = format!("http://{}", SocketAddr::new(local_ip, addr.port()));
    let lan_url = if addr.ip().is_loopback() {
        None
    } else if addr.ip().is_unspecified() {
        lan_ip().map(|ip| format!("http://{}", SocketAddr::new(ip, addr.port())))
    } else {
        Some(url.clone())
    };
    // Collars reach the server at the LAN address unless a public URL is set.
    ctx.set_local_url(lan_url.clone().unwrap_or_else(|| url.clone()));

    let info = ServerInfo { data_dir: data_dir.clone(), bind: bind.clone(), port: addr.port(), lan_url: lan_url.clone() };
    let app = build_app(ctx.clone(), info, opts.cors.unwrap_or(cfg!(debug_assertions)));

    let running = op_core::instance::RunningServer { url: Some(url.clone()), port: Some(addr.port()), lan_url: lan_url.clone(), ..op_core::instance::this_process() };
    op_core::instance::write_server_file(&data_dir, &running)?;

    // Tools list in registration order: the engine's farm tools first.
    op_engine::register_tools(&ctx);
    op_core::register_tools(&ctx);
    // A crate that fails to start stops the jobs the ones before it started.
    let stop_on_error = StopOnDrop(Some(ctx.clone()));
    op_ingest::start(ctx.clone()).await.context("starting op-ingest")?;
    op_analytics::start(ctx.clone()).await.context("starting op-analytics")?;
    op_brain::start(ctx.clone()).await.context("starting op-brain")?;
    op_engine::start(ctx.clone()).await.context("starting op-engine")?;
    // @HUB
    op_alerts::start(ctx.clone()).await.context("starting op-alerts")?;
    op_import::start(ctx.clone()).await.context("starting op-import")?;
    op_reports::start(ctx.clone()).await.context("starting op-reports")?;
    op_fleet::start(ctx.clone()).await.context("starting op-fleet")?;
    // @HUB-UI
    // @E-lib
    // @E-srv
    // @J
    // @A-engine
    // @A-notify
    // @D
    // @K-animals
    // @K-files
    // @I
    // @B
    // @G
    // @P
    // @Q
    // @C
    // @F
    // @S
    // @A3
    // @H
    // @L
    // @M
    // @Z
    stop_on_error.disarm();

    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let join = tokio::spawn(async move {
        // The peer address feeds the per-peer limits; connections are capped and idle ones closed.
        let listener = axum::serve::ListenerExt::tap_io(limits::capped(listener), |_| {});
        axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
            .with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
            })
            .await
            .map_err(anyhow::Error::from)
    });
    tracing::info!(%url, data_dir = %data_dir.display(), "openpasture serving");
    Ok(ServerHandle { url, addr, lan_url, ctx, shutdown_tx: Some(shutdown_tx), join })
}

/// Stops a half-started server's jobs when `serve` returns early.
struct StopOnDrop(Option<Ctx>);

impl StopOnDrop {
    fn disarm(mut self) {
        self.0 = None;
    }
}

impl Drop for StopOnDrop {
    fn drop(&mut self) {
        if let Some(ctx) = self.0.take() {
            ctx.shutdown();
        }
    }
}

/// The full app: op-core and feature routers, `/api/server`, `/api/live`, UI.
/// `/api` and `/mcp` need a token (see `auth`); public routes have limits
/// (`limits`). `dev` also lets the Vite dev server (localhost:5173) in.
pub fn build_app(ctx: Ctx, info: ServerInfo, dev: bool) -> Router {
    let info = Arc::new(info);
    let mut routes = Router::new();
    for part in [
        op_core::router(),
        op_ingest::router(),
        op_analytics::router(),
        op_brain::router(),
        op_engine::router(),
        // @HUB
        op_alerts::router(),
        op_import::router(),
        op_reports::router(),
        op_fleet::router(),
        // @HUB-UI
        // @E-lib
        // @E-srv
        // @J
        // @A-engine
        // @A-notify
        // @D
        // @K-animals
        // @K-files
        // @I
        // @B
        // @G
        // @P
        // @Q
        // @C
        // @F
        // @S
        // @A3
        // @H
        // @L
        // @M
        // @Z
    ] {
        routes = routes.merge(part);
    }
    let mut app = routes
        .merge(collar_address::router())
        .merge(session::router())
        .route("/api/server", get(move |state| server_info(state, info.clone())))
        .route("/api/server/retention", get(ops::get_retention).put(ops::put_retention))
        .route("/healthz", get(ops::healthz))
        .route("/api/live", get(live::handler))
        .fallback(ui::handler)
        .with_state(ctx.clone())
        .layer(axum::middleware::from_fn_with_state(ctx.clone(), op_ingest::wire_log::record))
        .layer(axum::middleware::from_fn_with_state(auth::AuthState::new(ctx), auth::guard))
        .layer(axum::middleware::from_fn_with_state(Arc::new(limits::Public::default()), limits::limit_public))
        .layer(axum::middleware::from_fn(limits::headers));
    if dev {
        use axum::http::{HeaderValue, Method, header};
        let origins: Vec<HeaderValue> = auth::DEV_ORIGINS.iter().filter_map(|o| HeaderValue::from_str(o).ok()).collect();
        app = app.layer(
            CorsLayer::new()
                .allow_origin(origins)
                .allow_methods([Method::GET, Method::POST, Method::PUT, Method::PATCH, Method::DELETE])
                .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE]),
        );
    }
    app.layer(axum::middleware::from_fn(ops::log_request))
}

#[derive(Serialize)]
struct ServerInfoBody {
    version: &'static str,
    data_dir: String,
    bind: String,
    port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    lan_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    public_url: Option<String>,
    /// Where collars reach this server (Settings, Server), once checked.
    #[serde(skip_serializing_if = "Option::is_none")]
    collar_url: Option<String>,
    /// What this server can do, for clients to ask instead of probing.
    capabilities: &'static [&'static str],
    work: work::Work,
    /// Supervised background jobs: status, heartbeats, restarts, last error.
    tasks: Vec<op_core::tasks::TaskInfo>,
    /// What the data dir holds and the disk's free space.
    disk: ops::Disk,
}

/// See [`ServerInfoBody::capabilities`]. A feature that may be missing from a
/// build or a farm lists itself here.
pub const CAPABILITIES: &[&str] = &["analytics", "sql", "export", "alerts", "texting", "push", "imports", "reports", "brains", "collar_address_check", "live_ping", "fleet"];

async fn server_info(State(ctx): State<Ctx>, info: Arc<ServerInfo>) -> ApiResult<axum::Json<ServerInfoBody>> {
    let settings = ctx.settings().await?;
    Ok(axum::Json(ServerInfoBody {
        version: VERSION,
        data_dir: info.data_dir.display().to_string(),
        bind: info.bind.clone(),
        port: info.port,
        lan_url: info.lan_url.clone(),
        public_url: settings.server.public_url,
        collar_url: settings.server.collar_url,
        capabilities: CAPABILITIES,
        work: work::current(&ctx).await?,
        tasks: ops::tasks(&ctx),
        disk: ops::disk(ctx.data_dir()).await,
    }))
}

/// This machine's address on the LAN. Connecting a UDP socket sends nothing.
fn lan_ip() -> Option<IpAddr> {
    let sock = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    sock.connect("192.0.2.1:80").ok()?;
    let ip = sock.local_addr().ok()?.ip();
    (!ip.is_unspecified() && !ip.is_loopback()).then_some(ip)
}
