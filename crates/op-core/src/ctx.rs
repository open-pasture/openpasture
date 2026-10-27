use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use anyhow::Context;
use op_protocol::{SigningKey, VerifyingKey};
use serde_json::Value;
use tokio::sync::{broadcast, watch};

use crate::brief::BriefRegistry;
use crate::domain::Settings;
use crate::event::Event;
use crate::identity::Identity;
use crate::instance::{self, DirLock};
use crate::secrets::Secrets;
use crate::store::Store;
use crate::tasks::Tasks;
use crate::tools::{RegistryRunner, ToolRegistry, ToolRunner};
use crate::{keys, settings};

/// Everything a crate needs, cheap to clone. Handlers get it as
/// `State(ctx): State<Ctx>`.
#[derive(Clone)]
pub struct Ctx(Arc<Inner>);

struct Inner {
    store: Store,
    events: broadcast::Sender<Event>,
    secrets: Secrets,
    data_dir: PathBuf,
    /// (local url, public url override)
    urls: RwLock<(String, Option<String>)>,
    signing_key: SigningKey,
    shutdown: watch::Sender<bool>,
    /// Short-lived tokens that only open `/mcp?scope=brain`, one per brain
    /// run: expiry and the tools the token may list and call.
    brain_tokens: Mutex<HashMap<String, (Instant, Vec<String>)>>,
    /// Pokes op-engine's scheduler to apply due decisions now.
    scheduler_wake: tokio::sync::Notify,
    /// Every crate's tools (MCP, brains, text questions).
    tools: ToolRegistry,
    /// Lines other crates add to the morning brief.
    brief_lines: BriefRegistry,
    /// Supervised background jobs.
    tasks: Tasks,
    /// The data dir's lock, held while this `Ctx` (or a later one in this
    /// process that took it over) lives.
    _lock: Arc<DirLock>,
}

/// Room for bursts of collar reports before a slow `/api/live` client lags.
const EVENT_BUS_CAPACITY: usize = 8192;

/// A brain-scoped MCP token. Revoked when dropped (the run is over).
pub struct BrainToken {
    ctx: Ctx,
    token: String,
}

impl BrainToken {
    pub fn as_str(&self) -> &str {
        &self.token
    }
}

impl Drop for BrainToken {
    fn drop(&mut self) {
        self.ctx.0.brain_tokens.lock().unwrap_or_else(|e| e.into_inner()).remove(&self.token);
    }
}

/// A data dir whose lock is held and whose database is about to open (see
/// [`Ctx::begin_open`]).
pub struct Opening {
    data_dir: PathBuf,
    lock: Arc<DirLock>,
    shutdown: watch::Sender<bool>,
    progress: crate::store::Progress,
}

impl Opening {
    pub fn data_dir(&self) -> &std::path::Path {
        &self.data_dir
    }

    /// Where opening the database is (snapshot, migrations), while
    /// [`Opening::finish`] runs.
    pub fn progress(&self) -> crate::store::Progress {
        self.progress.clone()
    }

    /// Open the database (snapshot and migrations when upgrading), secrets,
    /// server key and settings.
    pub async fn finish(self) -> anyhow::Result<Ctx> {
        let Opening { data_dir, lock, shutdown, progress } = self;
        let store = Store::open_with(&data_dir.join(crate::store::DB_FILE), Some(&progress)).await?;
        // Paddock areas stored before op-geo measured rings whichever way they wind.
        crate::area_repair::repair_paddock_areas(store.pool()).await?;
        let signing_key = keys::load_or_create(&data_dir)?;
        let secrets = Secrets::new(&data_dir);
        let (events, _) = broadcast::channel(EVENT_BUS_CAPACITY);
        let ctx = Ctx(Arc::new(Inner {
            store,
            events,
            secrets,
            data_dir,
            urls: RwLock::new((String::new(), None)),
            signing_key,
            shutdown,
            brain_tokens: Mutex::new(HashMap::new()),
            scheduler_wake: tokio::sync::Notify::new(),
            tools: ToolRegistry::default(),
            brief_lines: BriefRegistry::default(),
            tasks: Tasks::default(),
            _lock: lock,
        }));
        let s = ctx.settings().await?;
        ctx.set_local_url(format!("http://127.0.0.1:{}", s.server.port));
        ctx.set_public_url(s.server.public_url.clone());
        Ok(ctx)
    }
}

/// `OPENPASTURE_DATA_DIR`, else the platform data dir
/// (`~/Library/Application Support/openpasture` on macOS).
pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("OPENPASTURE_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    dirs::data_dir().map(|d| d.join("openpasture")).unwrap_or_else(|| PathBuf::from("openpasture-data"))
}

impl Ctx {
    /// Open the data directory: take its lock (one server per data dir, see
    /// [`instance`]), then the database (with migrations), secrets, server
    /// key and settings (created with defaults on first run).
    pub async fn open(data_dir: impl Into<PathBuf>) -> anyhow::Result<Ctx> {
        Ctx::begin_open(data_dir)?.finish().await
    }

    /// The first half of [`Ctx::open`]: the data dir is created owner-only,
    /// its lock taken and `server.json` written; the database isn't opened
    /// yet. A server binds its port in between, so that during a long
    /// upgrade it can say so ([`Opening::progress`]) instead of not
    /// answering.
    pub fn begin_open(data_dir: impl Into<PathBuf>) -> anyhow::Result<Opening> {
        let data_dir = data_dir.into();
        crate::perms::private_dir(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
        let (shutdown, shutdown_rx) = watch::channel(false);
        let lock = instance::acquire(&data_dir, Box::new(move || !*shutdown_rx.borrow()))?;
        instance::write_server_file(&data_dir, &instance::this_process())?;
        Ok(Opening { data_dir, lock, shutdown, progress: Default::default() })
    }

    pub fn store(&self) -> &Store {
        &self.0.store
    }

    pub fn db(&self) -> &sqlx::SqlitePool {
        self.0.store.pool()
    }

    /// Send an event to `/api/live` subscribers. Never fails; with no
    /// subscribers the event is dropped.
    pub fn publish(&self, event: Event) {
        let _ = self.0.events.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Event> {
        self.0.events.subscribe()
    }

    pub fn events(&self) -> &broadcast::Sender<Event> {
        &self.0.events
    }

    pub fn secrets(&self) -> &Secrets {
        &self.0.secrets
    }

    pub fn data_dir(&self) -> &Path {
        &self.0.data_dir
    }

    /// Where collars and outside agents reach this server: the public URL
    /// (settings or a tunnel) if set, else the local URL. No trailing slash.
    pub fn base_url(&self) -> String {
        let urls = self.0.urls.read().unwrap_or_else(|e| e.into_inner());
        urls.1.clone().unwrap_or_else(|| urls.0.clone())
    }

    /// The URL the server is bound to, e.g. `http://127.0.0.1:7878`. Set by op-server.
    pub fn local_url(&self) -> String {
        self.0.urls.read().unwrap_or_else(|e| e.into_inner()).0.clone()
    }

    pub fn set_local_url(&self, url: impl Into<String>) {
        self.0.urls.write().unwrap_or_else(|e| e.into_inner()).0 = url.into().trim_end_matches('/').to_owned();
    }

    /// Override the base URL, e.g. when a tunnel comes up. `None` clears it.
    pub fn set_public_url(&self, url: Option<String>) {
        self.0.urls.write().unwrap_or_else(|e| e.into_inner()).1 = url.map(|u| u.trim_end_matches('/').to_owned());
    }

    /// The server's Ed25519 key for signing boundaries.
    pub fn signing_key(&self) -> &SigningKey {
        &self.0.signing_key
    }

    pub fn public_key(&self) -> VerifyingKey {
        self.0.signing_key.verifying_key()
    }

    /// Base64 public key, given to collars at link time.
    pub fn public_key_b64(&self) -> String {
        op_protocol::encode_public_key(&self.public_key())
    }

    /// Current settings. Created with defaults (and a fresh app token) the
    /// first time. Stored settings that can't be read are an error, never
    /// replaced: replacing them would mint a new app token and lock out every
    /// phone and integration (see [`settings::repair`]).
    pub async fn settings(&self) -> anyhow::Result<Settings> {
        let read = || sqlx::query_scalar::<_, String>("SELECT value FROM settings WHERE key = ?").bind(settings::KEY).fetch_optional(self.db());
        if let Some(raw) = read().await? {
            return settings::parse_stored(&raw);
        }
        // Two first reads at once keep the same token: the second insert does nothing.
        sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO NOTHING")
            .bind(settings::KEY)
            .bind(serde_json::to_string(&Settings::default())?)
            .execute(self.db())
            .await?;
        let raw = read().await?.context("settings row missing after insert")?;
        settings::parse_stored(&raw)
    }

    /// Save whole settings; the collar address must be the one stored
    /// ([`Ctx::set_collar_url`] changes it).
    pub async fn save_settings(&self, s: &Settings) -> anyhow::Result<()> {
        settings::validate(s).map_err(anyhow::Error::msg)?;
        anyhow::ensure!(s.server.collar_url == self.settings().await?.server.collar_url, settings::COLLAR_URL_ELSEWHERE);
        self.store().set_setting(settings::KEY, s).await
    }

    /// Merge a partial settings object and save. Errors are farmer-readable.
    /// Read, merge and write happen in one write transaction, so two updates
    /// at once can't lose each other's fields. The collar address can't
    /// change here (409): it goes into collars, so only
    /// [`Ctx::set_collar_url`], after the reachability check, sets it.
    pub async fn update_settings(&self, partial: &Value) -> Result<Settings, crate::ApiError> {
        self.write_settings(partial, None).await
    }

    /// Set the collar address, which passed op-server's check.
    pub async fn set_collar_url(&self, checked: &settings::CheckedCollarUrl) -> Result<Settings, crate::ApiError> {
        self.write_settings(&serde_json::json!({ "server": { "collar_url": checked.url() } }), Some(checked)).await
    }

    async fn write_settings(&self, partial: &Value, collar_url: Option<&settings::CheckedCollarUrl>) -> Result<Settings, crate::ApiError> {
        let _ = self.settings().await?; // creates the row with defaults the first time
        let mut tx = crate::store::begin_immediate(self.db()).await?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM settings WHERE key = ?").bind(settings::KEY).fetch_optional(&mut *tx).await.map_err(anyhow::Error::from)?;
        let current = match row {
            Some((v,)) => settings::parse_stored(&v)?,
            None => Settings::default(),
        };
        let next: Settings = crate::patch::apply(&current, partial, &[])?;
        settings::validate(&next).map_err(crate::ApiError::bad_request)?;
        if next.server.collar_url != current.server.collar_url && collar_url.is_none_or(|c| c.url() != next.server.collar_url.as_deref()) {
            return Err(crate::ApiError::conflict(settings::COLLAR_URL_ELSEWHERE));
        }
        sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(settings::KEY)
            .bind(serde_json::to_string(&next).map_err(anyhow::Error::from)?)
            .execute(&mut *tx)
            .await
            .map_err(anyhow::Error::from)?;
        tx.commit().await.map_err(anyhow::Error::from)?;
        if next.server.public_url != current.server.public_url {
            self.set_public_url(next.server.public_url.clone());
        }
        Ok(next)
    }

    /// A token for one brain run: it opens `/mcp?scope=brain` only, lists
    /// and calls only `tools` (read tools among them), lives in memory, and
    /// expires after `ttl` or when the guard is dropped.
    pub fn mint_brain_token(&self, ttl: Duration, tools: Vec<String>) -> BrainToken {
        let token = format!("opb_{}", settings::new_token());
        let mut map = self.0.brain_tokens.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        map.retain(|_, (exp, _)| *exp > now);
        map.insert(token.clone(), (now + ttl, tools));
        BrainToken { ctx: self.clone(), token }
    }

    /// The tools a live brain token allows; `None` for an unknown or expired token.
    pub fn check_brain_token(&self, token: &str) -> Option<Vec<String>> {
        let map = self.0.brain_tokens.lock().unwrap_or_else(|e| e.into_inner());
        map.get(token).filter(|(exp, _)| *exp > Instant::now()).map(|(_, tools)| tools.clone())
    }

    /// The tool registry.
    pub fn tools(&self) -> &ToolRegistry {
        &self.0.tools
    }

    /// Read tools only, minus `exclude`, called as `identity`: what a text
    /// question or `Brain::ask` may use.
    pub fn tool_runner(&self, identity: Identity, exclude: &[&str]) -> Arc<dyn ToolRunner> {
        Arc::new(RegistryRunner::new(self, identity, exclude))
    }

    /// Lines other crates add to the morning brief.
    pub fn brief_lines(&self) -> &BriefRegistry {
        &self.0.brief_lines
    }

    /// Ask the decision scheduler to apply due decisions now instead of at its
    /// next tick (e.g. a herd switched to auto with a proposal open).
    pub fn wake_scheduler(&self) {
        self.0.scheduler_wake.notify_one();
    }

    /// Resolves when [`Ctx::wake_scheduler`] is called. For the scheduler loop.
    pub async fn scheduler_woken(&self) {
        self.0.scheduler_wake.notified().await;
    }

    /// The supervised background jobs (see [`Ctx::supervise`]).
    pub fn tasks(&self) -> &Tasks {
        &self.0.tasks
    }

    /// Ask background tasks to stop. Called by the server on shutdown.
    pub fn shutdown(&self) {
        self.0.shutdown.send_replace(true);
    }

    pub fn is_shutting_down(&self) -> bool {
        *self.0.shutdown.borrow()
    }

    /// Resolves once [`Ctx::shutdown`] is called. Use in background loops:
    /// `tokio::select! { _ = ctx.on_shutdown() => break, _ = tick.tick() => … }`.
    pub async fn on_shutdown(&self) {
        let mut rx = self.0.shutdown.subscribe();
        let _ = rx.wait_for(|v| *v).await;
    }
}
