//! API keys and other secrets, in `secrets.json` (0600) in the data directory.
//! Values are never logged or returned by the API.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use serde::{Deserialize, Serialize};

pub const FILE: &str = "secrets.json";

/// The names the app knows about. Others may be set too.
pub const KNOWN: [&str; 7] =
    ["anthropic_api_key", "openai_api_key", "compatible_api_key", "compatible_base_url", "hosted_api_key", "hosted_url", "firecrawl_api_key"];

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SecretStatus {
    pub name: String,
    pub set: bool,
}

pub struct Secrets {
    path: PathBuf,
    lock: Mutex<()>,
}

impl Secrets {
    pub fn new(data_dir: &Path) -> Self {
        Self { path: data_dir.join(FILE), lock: Mutex::new(()) }
    }

    fn read(&self) -> anyhow::Result<BTreeMap<String, String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) if text.trim().is_empty() => Ok(BTreeMap::new()),
            Ok(text) => serde_json::from_str(&text).with_context(|| format!("{} is not valid JSON", self.path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
            Err(e) => Err(e.into()),
        }
    }

    fn write(&self, map: &BTreeMap<String, String>) -> anyhow::Result<()> {
        write_private(&self.path, serde_json::to_string_pretty(map)?.as_bytes())
    }

    pub fn get(&self, name: &str) -> anyhow::Result<Option<String>> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        Ok(self.read()?.get(name).cloned())
    }

    pub fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        anyhow::ensure!(valid_name(name), "invalid secret name");
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut map = self.read()?;
        map.insert(name.to_owned(), value.to_owned());
        self.write(&map)
    }

    /// Returns whether it was set.
    pub fn delete(&self, name: &str) -> anyhow::Result<bool> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let mut map = self.read()?;
        let had = map.remove(name).is_some();
        if had {
            self.write(&map)?;
        }
        Ok(had)
    }

    /// Names that have a value.
    pub fn list_names(&self) -> anyhow::Result<Vec<String>> {
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        Ok(self.read()?.into_keys().collect())
    }

    /// Every known name plus any other set name, with whether it is set.
    pub fn status(&self) -> anyhow::Result<Vec<SecretStatus>> {
        let set = self.list_names()?;
        let mut out: Vec<SecretStatus> = KNOWN.iter().map(|n| SecretStatus { name: (*n).to_owned(), set: set.iter().any(|s| s == n) }).collect();
        for name in set {
            if !KNOWN.contains(&name.as_str()) {
                out.push(SecretStatus { name, set: true });
            }
        }
        Ok(out)
    }
}

pub fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 64 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}

/// Write a file readable only by the owner, atomically.
pub fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    drop(f);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}
