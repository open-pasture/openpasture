//! Which brains can run here: CLI binaries and sign-in, keys, model lists.
//! Cached so `GET /api/brains` answers at once; refreshed in the background.

use std::collections::HashMap;
use std::ffi::OsString;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use op_core::{BrainId, BrainInfo, Ctx};
use serde_json::Value;

use crate::cli;

const STATUS_TIMEOUT: Duration = Duration::from_secs(4);
const MODELS_TIMEOUT: Duration = Duration::from_secs(4);
const TTL: Duration = Duration::from_secs(30);

/// Where to look for CLI binaries.
#[derive(Debug, Clone)]
pub struct Locator {
    pub path: Option<OsString>,
    pub home: Option<PathBuf>,
    /// Also look in /opt/homebrew/bin and /usr/local/bin.
    pub system: bool,
    pub codex_home: Option<PathBuf>,
}

impl Locator {
    pub fn from_env() -> Self {
        let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
        let codex_home = std::env::var_os("CODEX_HOME").filter(|h| !h.is_empty()).map(PathBuf::from).or_else(|| home.as_ref().map(|h| h.join(".codex")));
        Self { path: std::env::var_os("PATH"), home, system: true, codex_home }
    }

    /// For tests: only `path` and dirs under `home`.
    pub fn isolated(path: &Path, home: &Path) -> Self {
        Self { path: Some(path.into()), home: Some(home.into()), system: false, codex_home: Some(home.join(".codex")) }
    }

    /// PATH entries, then the usual install locations.
    pub fn dirs(&self) -> Vec<PathBuf> {
        let mut dirs: Vec<PathBuf> = self.path.as_ref().map(|p| std::env::split_paths(p).collect()).unwrap_or_default();
        if let Some(h) = &self.home {
            for d in [".local/bin", ".bun/bin", ".npm-global/bin", ".npm/bin", ".volta/bin", ".claude/local", "Library/pnpm", ".yarn/bin"] {
                dirs.push(h.join(d));
            }
            // nvm: newest node first.
            if let Ok(rd) = std::fs::read_dir(h.join(".nvm/versions/node")) {
                let mut v: Vec<PathBuf> = rd.flatten().map(|e| e.path().join("bin")).collect();
                v.sort();
                dirs.extend(v.into_iter().rev());
            }
        }
        if self.system {
            if let Some(prefix) = std::env::var_os("NPM_CONFIG_PREFIX").filter(|p| !p.is_empty()) {
                dirs.push(PathBuf::from(prefix).join("bin"));
            }
            dirs.push("/opt/homebrew/bin".into());
            dirs.push("/usr/local/bin".into());
        }
        let mut seen = std::collections::HashSet::new();
        dirs.retain(|d| !d.as_os_str().is_empty() && seen.insert(d.clone()));
        dirs
    }

    pub fn find(&self, name: &str) -> Option<PathBuf> {
        let names: Vec<String> = if cfg!(windows) { vec![format!("{name}.exe"), format!("{name}.cmd"), name.to_owned()] } else { vec![name.to_owned()] };
        self.dirs().into_iter().flat_map(|d| names.iter().map(move |n| d.join(n))).find(|p| is_executable(p))
    }

    /// PATH for a child process: the binary's own dir first, then every dir we search.
    pub fn child_path(&self, bin: &Path) -> OsString {
        let mut dirs = Vec::new();
        if let Some(parent) = bin.parent() {
            dirs.push(parent.to_path_buf());
        }
        dirs.extend(self.dirs());
        std::env::join_paths(dirs).unwrap_or_default()
    }
}

fn is_executable(p: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(p) else { return false };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

#[derive(Debug, Clone, Default)]
pub struct CliStatus {
    pub path: Option<PathBuf>,
    pub signed_in: bool,
    pub detail: Option<String>,
    pub models: Vec<String>,
}

pub async fn detect_codex(loc: &Locator) -> CliStatus {
    let Some(path) = loc.find("codex") else {
        return CliStatus { detail: Some("Not installed".into()), ..Default::default() };
    };
    let models = loc.codex_home.as_deref().map(codex_models).unwrap_or_default();
    let (signed_in, detail) = match cli::run_quick(&path, &["login", "status"], STATUS_TIMEOUT).await {
        Ok(out) => {
            let text = format!("{}\n{}", out.stdout, out.stderr).to_lowercase();
            let yes = out.success && text.contains("logged in") && !text.contains("not logged in");
            // Never echo the status line: API key logins print part of the key.
            let detail = if !yes {
                "Sign in with codex login"
            } else if text.contains("chatgpt") {
                "ChatGPT sign-in"
            } else if text.contains("api key") {
                "API key sign-in"
            } else {
                "Signed in"
            };
            (yes, detail.to_owned())
        }
        Err(e) => (false, short_err(&e)),
    };
    CliStatus { path: Some(path), signed_in, detail: Some(detail), models }
}

/// Models the Codex CLI offers, from its own cache (`models_cache.json`).
pub fn codex_models(codex_home: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(codex_home.join("models_cache.json")) else { return Vec::new() };
    let Ok(v) = serde_json::from_str::<Value>(&text) else { return Vec::new() };
    v.get("models")
        .and_then(Value::as_array)
        .map(|ms| {
            ms.iter()
                .filter(|m| m.get("visibility").and_then(Value::as_str).is_none_or(|vis| vis == "list"))
                .filter_map(|m| m.get("slug").and_then(Value::as_str).map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

pub const CLAUDE_MODELS: [&str; 4] = ["opus", "sonnet", "haiku", "fable"];

pub async fn detect_claude(loc: &Locator) -> CliStatus {
    let models = CLAUDE_MODELS.iter().map(|s| s.to_string()).collect();
    let Some(path) = loc.find("claude") else {
        return CliStatus { detail: Some("Not installed".into()), ..Default::default() };
    };
    let (signed_in, detail) = match cli::run_quick(&path, &["auth", "status"], STATUS_TIMEOUT).await {
        Ok(out) => match serde_json::from_str::<Value>(out.stdout.trim()) {
            Ok(v) if v.get("loggedIn").and_then(Value::as_bool) == Some(true) => {
                let plan = v.get("subscriptionType").and_then(Value::as_str).filter(|s| !s.is_empty());
                let method = v.get("authMethod").and_then(Value::as_str).unwrap_or_default();
                let detail = match plan {
                    Some(p) => format!("Claude {p}"),
                    None if method.contains("key") || method.contains("api") => "API key sign-in".into(),
                    None => "Signed in".into(),
                };
                (true, detail)
            }
            Ok(_) => (false, "Sign in with claude auth login".into()),
            // Older versions print text; trust the exit code.
            Err(_) if out.success => (true, "Signed in".into()),
            Err(_) => (false, "Sign in with claude auth login".into()),
        },
        Err(e) => (false, short_err(&e)),
    };
    CliStatus { path: Some(path), signed_in, detail: Some(detail), models }
}

fn short_err(e: &anyhow::Error) -> String {
    let s = e.to_string();
    if s.contains("timed out") { "Not responding".into() } else { "Can't run".into() }
}

// ---- cache ----

type Remote = Result<Vec<String>, String>;

struct Cache {
    at: Instant,
    codex: CliStatus,
    claude: CliStatus,
    /// Model lists keyed by brain, with a fingerprint of the secrets used.
    remote: HashMap<BrainId, (u64, Remote)>,
    refreshing: bool,
}

fn cache() -> &'static Mutex<Option<Cache>> {
    static C: OnceLock<Mutex<Option<Cache>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(None))
}

fn lock() -> std::sync::MutexGuard<'static, Option<Cache>> {
    cache().lock().unwrap_or_else(|e| e.into_inner())
}

/// Mark the cache stale so the next list refreshes (e.g. after a test run).
pub fn invalidate() {
    if let Some(c) = lock().as_mut() {
        c.at = Instant::now() - TTL * 2;
    }
}

struct Keys {
    anthropic: Option<String>,
    openai: Option<String>,
    compatible_base: Option<String>,
    compatible_key: Option<String>,
    hosted: Option<String>,
}

fn keys(ctx: &Ctx) -> Keys {
    let get = |n: &str| ctx.secrets().get(n).ok().flatten().map(|v| v.trim().to_owned()).filter(|v| !v.is_empty());
    Keys {
        anthropic: get("anthropic_api_key"),
        openai: get("openai_api_key"),
        compatible_base: get("compatible_base_url"),
        compatible_key: get("compatible_api_key"),
        hosted: get("hosted_api_key"),
    }
}

fn fingerprint(parts: &[&Option<String>]) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    parts.hash(&mut h);
    h.finish()
}

async fn fetch_remote(k: &Keys) -> HashMap<BrainId, (u64, Remote)> {
    let openai = async {
        match &k.openai {
            Some(key) => Some(crate::api::openai_models(key).await.map_err(|e| e.to_string())),
            None => None,
        }
    };
    let compatible = async {
        match &k.compatible_base {
            Some(base) => Some(crate::api::compatible_models(base, k.compatible_key.as_deref()).await.map_err(|e| e.to_string())),
            None => None,
        }
    };
    let (o, c) = tokio::time::timeout(MODELS_TIMEOUT + Duration::from_secs(1), async { tokio::join!(openai, compatible) })
        .await
        .unwrap_or((Some(Err("timed out".into())), Some(Err("timed out".into()))));
    let mut m = HashMap::new();
    if let Some(r) = o {
        m.insert(BrainId::Openai, (fingerprint(&[&k.openai]), r));
    }
    if let Some(r) = c {
        m.insert(BrainId::Compatible, (fingerprint(&[&k.compatible_base, &k.compatible_key]), r));
    }
    m
}

/// Detect everything now and store it.
pub async fn refresh(ctx: &Ctx) {
    let loc = Locator::from_env();
    let k = keys(ctx);
    let (codex, claude, remote) = tokio::join!(detect_codex(&loc), detect_claude(&loc), fetch_remote(&k));
    *lock() = Some(Cache { at: Instant::now(), codex, claude, remote, refreshing: false });
}

/// Every brain with its status. Fast after the first call: stale data is
/// returned while a refresh runs in the background. Model lists refetch at
/// once when their key changes.
pub async fn list(ctx: &Ctx, force: bool) -> Vec<BrainInfo> {
    let k = keys(ctx);
    let fp_openai = fingerprint(&[&k.openai]);
    let fp_compat = fingerprint(&[&k.compatible_base, &k.compatible_key]);
    let (empty, keys_changed, stale) = {
        let g = lock();
        match g.as_ref() {
            None => (true, false, false),
            Some(c) => {
                let changed = |id: BrainId, set: bool, fp: u64| match c.remote.get(&id) {
                    Some((f, _)) => !set || *f != fp,
                    None => set,
                };
                let keys_changed =
                    changed(BrainId::Openai, k.openai.is_some(), fp_openai) || changed(BrainId::Compatible, k.compatible_base.is_some(), fp_compat);
                (false, keys_changed, c.at.elapsed() > TTL && !c.refreshing)
            }
        }
    };
    if empty || force {
        refresh(ctx).await;
    } else if keys_changed {
        let remote = fetch_remote(&k).await;
        if let Some(c) = lock().as_mut() {
            c.remote = remote;
        }
    } else if stale {
        if let Some(c) = lock().as_mut() {
            c.refreshing = true;
        }
        let ctx = ctx.clone();
        tokio::spawn(async move { refresh(&ctx).await });
    }

    let g = lock();
    let Some(c) = g.as_ref() else { return Vec::new() };
    build(c, &k)
}

fn build(c: &Cache, k: &Keys) -> Vec<BrainInfo> {
    let remote = |id: BrainId| c.remote.get(&id).map(|(_, r)| r.clone());
    BrainId::ALL
        .iter()
        .map(|&id| {
            let needs = |n: &[&str]| n.iter().map(|s| s.to_string()).collect::<Vec<_>>();
            let key_brain = |name: &str, key: &Option<String>, secret: &str, models: Vec<String>| BrainInfo {
                id,
                name: name.into(),
                available: key.is_some(),
                signed_in: key.is_some(),
                needs: needs(&[secret]),
                models,
                detail: key.is_none().then(|| "Add API key".to_owned()),
            };
            match id {
                BrainId::Codex => cli_info(id, "Codex", &c.codex),
                BrainId::Claude => cli_info(id, "Claude Code", &c.claude),
                BrainId::Anthropic => {
                    key_brain("Anthropic", &k.anthropic, "anthropic_api_key", crate::api::ANTHROPIC_MODELS.iter().map(|s| s.to_string()).collect())
                }
                BrainId::Openai => {
                    let mut b = key_brain("OpenAI", &k.openai, "openai_api_key", vec![]);
                    match remote(id) {
                        Some(Ok(m)) => b.models = m,
                        Some(Err(_)) => b.detail = Some("Key not accepted".into()),
                        None => {}
                    }
                    if b.models.is_empty() && k.openai.is_some() {
                        b.models = vec![crate::api::OPENAI_DEFAULT_MODEL.into()];
                    }
                    b
                }
                BrainId::Compatible => {
                    let set = k.compatible_base.is_some();
                    let mut b = BrainInfo {
                        id,
                        name: "Compatible".into(),
                        available: set,
                        signed_in: set,
                        needs: needs(&["compatible_base_url", "compatible_api_key"]),
                        models: vec![],
                        detail: (!set).then(|| "Add base URL".to_owned()),
                    };
                    match remote(id) {
                        Some(Ok(m)) => b.models = m,
                        Some(Err(_)) => b.detail = Some("Not reachable".into()),
                        None => {}
                    }
                    b
                }
                BrainId::Hosted => {
                    let mut b = key_brain("openpasture", &k.hosted, "hosted_api_key", vec![]);
                    // hosted_url is optional (default https://api.openpasture.dev).
                    b.needs.push("hosted_url".into());
                    b
                }
                BrainId::Heuristic => {
                    BrainInfo { id, name: "Heuristic".into(), available: true, signed_in: true, needs: vec![], models: vec![], detail: Some("No model".into()) }
                }
            }
        })
        .collect()
}

fn cli_info(id: BrainId, name: &str, s: &CliStatus) -> BrainInfo {
    BrainInfo { id, name: name.into(), available: s.path.is_some(), signed_in: s.signed_in, needs: vec![], models: s.models.clone(), detail: s.detail.clone() }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    pub(crate) fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    #[test]
    fn finds_in_path_then_home_dirs() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let loc = Locator::isolated(bin.path(), home.path());
        assert!(loc.find("codex").is_none());
        // Not executable: ignored.
        std::fs::write(bin.path().join("codex"), "x").unwrap();
        assert!(loc.find("codex").is_none());
        // ~/.local/bin.
        std::fs::create_dir_all(home.path().join(".local/bin")).unwrap();
        let local = script(&home.path().join(".local/bin"), "codex", "exit 0");
        assert_eq!(loc.find("codex"), Some(local));
        // PATH wins.
        let on_path = script(bin.path(), "codex", "exit 0");
        assert_eq!(loc.find("codex"), Some(on_path.clone()));
        let child = loc.child_path(&on_path);
        assert!(std::env::split_paths(&child).next().unwrap() == bin.path());
    }

    #[tokio::test]
    async fn codex_signed_in_and_out() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let loc = Locator::isolated(bin.path(), home.path());
        assert_eq!(detect_codex(&loc).await.detail.as_deref(), Some("Not installed"));

        script(bin.path(), "codex", r#"[ "$1 $2" = "login status" ] && { echo "Logged in using ChatGPT" >&2; exit 0; }; exit 2"#);
        std::fs::create_dir_all(home.path().join(".codex")).unwrap();
        std::fs::write(
            home.path().join(".codex/models_cache.json"),
            r#"{"models":[{"slug":"gpt-6-astra","visibility":"list"},{"slug":"secret","visibility":"hide"},{"slug":"gpt-5.5","visibility":"list"}]}"#,
        )
        .unwrap();
        let s = detect_codex(&loc).await;
        assert!(s.path.is_some() && s.signed_in);
        assert_eq!(s.detail.as_deref(), Some("ChatGPT sign-in"));
        assert_eq!(s.models, vec!["gpt-6-astra", "gpt-5.5"]);

        script(bin.path(), "codex", r#"echo "Not logged in"; exit 1"#);
        let s = detect_codex(&loc).await;
        assert!(s.path.is_some() && !s.signed_in);
        assert_eq!(s.detail.as_deref(), Some("Sign in with codex login"));
    }

    #[tokio::test]
    async fn codex_api_key_status_is_not_echoed() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        script(bin.path(), "codex", r#"echo "Logged in using an API key - sk-proj-***ABCD""#);
        let s = detect_codex(&Locator::isolated(bin.path(), home.path())).await;
        assert!(s.signed_in);
        assert_eq!(s.detail.as_deref(), Some("API key sign-in"));
    }

    #[tokio::test]
    async fn claude_status_json() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let loc = Locator::isolated(bin.path(), home.path());
        script(
            bin.path(),
            "claude",
            r#"[ "$1 $2" = "auth status" ] && echo '{"loggedIn":true,"authMethod":"claude.ai","subscriptionType":"max","email":"x@y"}'"#,
        );
        let s = detect_claude(&loc).await;
        assert!(s.signed_in);
        assert_eq!(s.detail.as_deref(), Some("Claude max"));
        assert!(s.models.contains(&"sonnet".to_string()));

        script(bin.path(), "claude", r#"echo '{"loggedIn":false}'; exit 1"#);
        let s = detect_claude(&loc).await;
        assert!(!s.signed_in);
        assert_eq!(s.detail.as_deref(), Some("Sign in with claude auth login"));
    }

    #[tokio::test]
    async fn slow_status_times_out() {
        let bin = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        script(bin.path(), "claude", "sleep 30");
        let t = Instant::now();
        let s = detect_claude(&Locator::isolated(bin.path(), home.path())).await;
        assert!(t.elapsed() < Duration::from_secs(10));
        assert!(!s.signed_in);
        assert_eq!(s.detail.as_deref(), Some("Not responding"));
    }
}

#[cfg(all(test, unix))]
pub(crate) use tests::script;
