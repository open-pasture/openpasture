//! One server per data dir. [`Ctx::open`](crate::Ctx::open) takes an
//! exclusive OS lock on `<data dir>/openpasture.lock` and holds it for the
//! life of the process; a second process that opens the same data dir gets
//! [`AlreadyRunning`] instead of running every background job twice against
//! one database.
//!
//! The holder writes `<data dir>/server.json` (pid, version, and once it is
//! listening, its URL and port), so the desktop app or a script can find the
//! running server with [`running`] and open it rather than start another.

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const LOCK_FILE: &str = "openpasture.lock";
pub const SERVER_FILE: &str = "server.json";

/// What `server.json` says about the process holding the lock.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunningServer {
    pub pid: u32,
    pub version: String,
    pub started_at: String,
    /// Local URL once the server is listening, e.g. `http://127.0.0.1:7878`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_url: Option<String>,
}

/// Another process (or a live server in this one) has the data dir open.
#[derive(Debug, Clone)]
pub struct AlreadyRunning {
    pub data_dir: PathBuf,
    /// From `server.json`; `None` when the holder hasn't written it.
    pub server: Option<RunningServer>,
}

impl std::fmt::Display for AlreadyRunning {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "openpasture is already running for {}", self.data_dir.display())?;
        match &self.server {
            Some(s) => {
                write!(f, " (pid {}", s.pid)?;
                if let Some(url) = &s.url {
                    write!(f, ", {url}")?;
                }
                write!(f, "). Open that server, or stop it before starting another.")
            }
            None => write!(f, ". Stop the other openpasture process first."),
        }
    }
}

impl std::error::Error for AlreadyRunning {}

/// The held lock file. The OS lock goes when the last handle drops.
#[derive(Debug)]
pub struct DirLock {
    _file: File,
    dir: PathBuf,
}

impl DirLock {
    pub fn data_dir(&self) -> &Path {
        &self.dir
    }
}

/// Live holders in this process: a server that has been shut down hands its
/// lock to the next [`Ctx::open`](crate::Ctx::open) of the same data dir
/// (tests, an in-process restart) while its last tasks wind down.
struct Holder {
    lock: Weak<DirLock>,
    live: Box<dyn Fn() -> bool + Send>,
}

fn holders() -> &'static Mutex<HashMap<PathBuf, Holder>> {
    static HOLDERS: OnceLock<Mutex<HashMap<PathBuf, Holder>>> = OnceLock::new();
    HOLDERS.get_or_init(Default::default)
}

fn key(data_dir: &Path) -> PathBuf {
    data_dir.canonicalize().unwrap_or_else(|_| data_dir.to_path_buf())
}

/// Take the data dir's lock, or fail with [`AlreadyRunning`]. `live` says
/// whether the owner is still serving; once it returns false another open in
/// this process may take the lock over.
pub(crate) fn acquire(data_dir: &Path, live: Box<dyn Fn() -> bool + Send>) -> anyhow::Result<Arc<DirLock>> {
    let k = key(data_dir);
    let mut map = holders().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(h) = map.get(&k)
        && let Some(lock) = h.lock.upgrade()
    {
        if (h.live)() {
            return Err(AlreadyRunning { data_dir: data_dir.to_path_buf(), server: read_server_file(data_dir) }.into());
        }
        map.insert(k, Holder { lock: Arc::downgrade(&lock), live });
        return Ok(lock);
    }
    let lock = Arc::new(lock_file(data_dir)?);
    map.insert(k, Holder { lock: Arc::downgrade(&lock), live });
    Ok(lock)
}

/// Take the lock outside a [`Ctx`](crate::Ctx), e.g. to repair settings
/// while no server runs.
pub fn lock(data_dir: &Path) -> anyhow::Result<DirLock> {
    let k = key(data_dir);
    let map = holders().lock().unwrap_or_else(|e| e.into_inner());
    if map.get(&k).is_some_and(|h| h.lock.strong_count() > 0) {
        return Err(AlreadyRunning { data_dir: data_dir.to_path_buf(), server: read_server_file(data_dir) }.into());
    }
    lock_file(data_dir)
}

fn lock_file(data_dir: &Path) -> anyhow::Result<DirLock> {
    std::fs::create_dir_all(data_dir).with_context(|| format!("creating {}", data_dir.display()))?;
    let path = data_dir.join(LOCK_FILE);
    let file = OpenOptions::new().create(true).truncate(false).read(true).write(true).open(&path).with_context(|| format!("opening {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(DirLock { _file: file, dir: data_dir.to_path_buf() }),
        Err(TryLockError::WouldBlock) => Err(AlreadyRunning { data_dir: data_dir.to_path_buf(), server: read_server_file(data_dir) }.into()),
        Err(TryLockError::Error(e)) => Err(anyhow::Error::from(e).context(format!("locking {}", path.display()))),
    }
}

/// The server running from `data_dir`, if any process holds its lock (in
/// this process: one that hasn't been shut down). Its URL is set once it is
/// listening.
pub fn running(data_dir: &Path) -> Option<RunningServer> {
    if let Some(live) = holders().lock().unwrap_or_else(|e| e.into_inner()).get(&key(data_dir)).filter(|h| h.lock.strong_count() > 0).map(|h| (h.live)()) {
        return live.then(|| read_server_file(data_dir).unwrap_or_else(this_process));
    }
    let file = OpenOptions::new().read(true).write(true).open(data_dir.join(LOCK_FILE)).ok()?;
    match file.try_lock() {
        // Nobody holds it: whatever server.json says is left from a crash.
        Ok(()) => None,
        Err(_) => Some(read_server_file(data_dir).unwrap_or_default()),
    }
}

pub fn read_server_file(data_dir: &Path) -> Option<RunningServer> {
    let text = std::fs::read_to_string(data_dir.join(SERVER_FILE)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Write `server.json` (temp file and rename, so a reader never sees half).
pub fn write_server_file(data_dir: &Path, s: &RunningServer) -> anyhow::Result<()> {
    let path = data_dir.join(SERVER_FILE);
    let tmp = data_dir.join(format!(".{SERVER_FILE}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, serde_json::to_vec_pretty(s)?).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Remove `server.json` if it is this process's.
pub fn remove_server_file(data_dir: &Path) {
    if read_server_file(data_dir).is_some_and(|s| s.pid == std::process::id()) {
        let _ = std::fs::remove_file(data_dir.join(SERVER_FILE));
    }
}

/// `server.json` for this process before it listens.
pub fn this_process() -> RunningServer {
    RunningServer {
        pid: std::process::id(),
        version: env!("CARGO_PKG_VERSION").into(),
        started_at: crate::time::to_db(&crate::time::now()),
        ..Default::default()
    }
}
