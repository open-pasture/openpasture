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
use crate::secrets::Secrets;
use crate::store::Store;
use crate::tools::{RegistryRunner, ToolRegistry, ToolRunner};
use crate::{keys, patch, settings};

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

/// `OPENPASTURE_DATA_DIR`, else the platform data dir
/// (`~/Library/Application Support/openpasture` on macOS).
pub fn default_data_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("OPENPASTURE_DATA_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    dirs::data_dir().map(|d| d.join("openpasture")).unwrap_or_else(|| PathBuf::from("openpasture-data"))
}

impl Ctx {
    /// Open the data directory: database (with migrations), secrets, server
    /// key and settings (created with defaults on first run).
    pub async fn open(data_dir: impl Into<PathBuf>) -> anyhow::Result<Ctx> {
        let data_dir = data_dir.into();
        std::fs::create_dir_all(&data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
        let store = Store::open(&data_dir).await?;
        let signing_key = keys::load_or_create(&data_dir)?;
        let secrets = Secrets::new(&data_dir);
        let (events, _) = broadcast::channel(EVENT_BUS_CAPACITY);
        let (shutdown, _) = watch::channel(false);
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
        }));
        let s = ctx.settings().await?;
        ctx.set_local_url(format!("http://127.0.0.1:{}", s.server.port));
        ctx.set_public_url(s.server.public_url.clone());
        Ok(ctx)
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
    /// first time.
    pub async fn settings(&self) -> anyhow::Result<Settings> {
        let store = self.store();
        if let Some(v) = store.get_setting_json(settings::KEY).await? {
            // Fill fields added after the row was written from the defaults.
            let mut full = serde_json::to_value(Settings::default())?;
            patch::merge(&mut full, &v);
            if let Ok(s) = serde_json::from_value::<Settings>(full) {
                return Ok(s);
            }
            tracing::warn!("stored settings are unreadable; resetting to defaults");
        }
        let s = Settings::default();
        store.set_setting(settings::KEY, &s).await?;
        Ok(s)
    }

    pub async fn save_settings(&self, s: &Settings) -> anyhow::Result<()> {
        settings::validate(s).map_err(anyhow::Error::msg)?;
        self.store().set_setting(settings::KEY, s).await
    }

    /// Merge a partial settings object and save. Errors are farmer-readable.
    /// Read, merge and write happen in one write transaction, so two updates
    /// at once can't lose each other's fields.
    pub async fn update_settings(&self, partial: &Value) -> Result<Settings, crate::ApiError> {
        let _ = self.settings().await?; // creates the row with defaults the first time
        let mut tx = crate::store::begin_immediate(self.db()).await?;
        let row: Option<(String,)> =
            sqlx::query_as("SELECT value FROM settings WHERE key = ?").bind(settings::KEY).fetch_optional(&mut *tx).await.map_err(anyhow::Error::from)?;
        let current = match row {
            Some((v,)) => {
                let mut full = serde_json::to_value(Settings::default()).map_err(anyhow::Error::from)?;
                patch::merge(&mut full, &serde_json::from_str(&v).map_err(anyhow::Error::from)?);
                serde_json::from_value::<Settings>(full).map_err(anyhow::Error::from)?
            }
            None => Settings::default(),
        };
        let next: Settings = patch::apply(&current, partial, &[])?;
        settings::validate(&next).map_err(crate::ApiError::bad_request)?;
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
