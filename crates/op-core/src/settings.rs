//! App settings, stored as JSON under the `settings` key of the settings table.

use rand::RngCore;
use serde_json::Value;

use crate::domain::{BrainSetting, ServerSettings, Settings, Units};
use crate::patch;

pub const KEY: &str = "settings";
pub const DEFAULT_BIND: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 7878;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            brain: BrainSetting::default(),
            decision_time: "06:00".into(),
            server: ServerSettings { bind: DEFAULT_BIND.into(), port: DEFAULT_PORT, public_url: None, collar_url: None, app_token: new_token() },
            units: Units::Metric,
        }
    }
}

pub fn new_token() -> String {
    let mut bytes = [0u8; 24];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

pub fn validate(s: &Settings) -> Result<(), String> {
    let t = s.decision_time.as_bytes();
    let ok =
        t.len() == 5 && t[2] == b':' && s.decision_time[..2].parse::<u8>().is_ok_and(|h| h < 24) && s.decision_time[3..].parse::<u8>().is_ok_and(|m| m < 60);
    if !ok {
        return Err("decision_time must be HH:MM.".into());
    }
    if s.server.bind.trim().is_empty() || s.server.bind.parse::<std::net::IpAddr>().is_err() && s.server.bind != "localhost" {
        return Err("server.bind must be an IP address.".into());
    }
    if s.server.app_token.len() < 16 {
        return Err("server.app_token must be at least 16 characters.".into());
    }
    if let Some(url) = &s.server.public_url
        && !(url.starts_with("http://") || url.starts_with("https://"))
    {
        return Err("server.public_url must start with http:// or https://.".into());
    }
    if let Some(url) = &s.server.collar_url
        && let Some(why) = collar_url_problem(url)
    {
        return Err(why);
    }
    Ok(())
}

/// Why a settings write can't set the collar address: only the checked
/// route does ([`crate::Ctx::set_collar_url`]).
pub const COLLAR_URL_ELSEWHERE: &str =
    "The collar address is set in Settings, Server, where it is checked to reach this server first (PUT /api/server/collar-address).";

/// A collar address that passed op-server's reachability check (or `None`,
/// no address). Made only by that check: hold one, and the address is fit to
/// hand to collars.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckedCollarUrl(Option<String>);

impl CheckedCollarUrl {
    /// `url` just passed the full check (form, DNS, https, this server). Only
    /// `op_server::collar_address` calls this.
    pub fn passed(url: String) -> Self {
        Self(Some(url))
    }

    /// No collar address: collars keep the endpoint they have.
    pub fn none() -> Self {
        Self(None)
    }

    pub fn url(&self) -> Option<&str> {
        self.0.as_deref()
    }
}

/// Why a collar address can't go into collars, or `None` when its form is
/// fine (whether it reaches this server is op-server's check). Collars talk
/// only to `https://`, can't reach this machine's loopback, and keep the
/// address for a day when it stops answering, so a name that changes on
/// every run strands them.
pub fn collar_url_problem(url: &str) -> Option<String> {
    let Some(rest) = url.strip_prefix("https://") else {
        return Some("The collar address must start with https://: collars only talk to https.".into());
    };
    let host = host_of(rest);
    if host.is_empty() {
        return Some("The collar address needs a host name.".into());
    }
    if host == "localhost" || host.ends_with(".localhost") || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback() || ip.is_unspecified()) {
        return Some(format!("{host} is this computer: a collar can't reach it. Use a named tunnel or a domain that points here."));
    }
    if host.ends_with(".trycloudflare.com") {
        return Some(
            "Quick-tunnel names change every time the tunnel starts, and collars keep the address they were given. Use a named tunnel or your own domain."
                .into(),
        );
    }
    if format!("{}/collar/v1", url.trim_end_matches('/')).len() > op_protocol::config::MAX_ENDPOINT_BYTES {
        return Some("The collar address is too long for a collar to hold.".into());
    }
    None
}

/// The host of `host[:port][/path]`, lowercased, IPv6 without brackets.
fn host_of(rest: &str) -> String {
    let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h).to_ascii_lowercase();
    if let Some(v6) = authority.strip_prefix('[') {
        return v6.split_once(']').map_or(v6, |(h, _)| h).to_owned();
    }
    authority.split_once(':').map_or(authority.as_str(), |(h, _)| h).to_owned()
}

/// Stored settings that can't be read. Nothing is written when this happens.
#[derive(Debug, Clone)]
pub struct UnreadableSettings {
    pub detail: String,
}

impl std::fmt::Display for UnreadableSettings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the stored settings can't be read ({}). Nothing was changed. Stop openpasture and run `openpasture settings repair` \
             to keep every setting that can be read (the app token among them) and reset the rest; the unreadable copy is kept",
            self.detail
        )
    }
}

impl std::error::Error for UnreadableSettings {}

/// Settings from the stored JSON, fields added since it was written filled
/// from the defaults.
pub fn parse_stored(raw: &str) -> anyhow::Result<Settings> {
    let v: Value = serde_json::from_str(raw).map_err(|e| UnreadableSettings { detail: format!("not JSON: {e}") })?;
    let mut full = serde_json::to_value(Settings::default())?;
    patch::merge(&mut full, &v);
    serde_json::from_value(full).map_err(|e| UnreadableSettings { detail: e.to_string() }.into())
}

/// The stored settings, read without the data dir's lock and without
/// migrating or writing anything, for tools that run beside a server
/// (`openpasture token`). `None` before the first run has stored any.
pub async fn read_stored(data_dir: &std::path::Path) -> anyhow::Result<Option<Settings>> {
    use anyhow::Context;
    let path = data_dir.join(crate::store::DB_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let opts = sqlx::sqlite::SqliteConnectOptions::new().filename(&path).read_only(true).busy_timeout(std::time::Duration::from_secs(10));
    let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1).connect_with(opts).await.with_context(|| format!("opening {}", path.display()))?;
    let raw: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?").bind(KEY).fetch_optional(&pool).await?;
    pool.close().await;
    raw.map(|r| parse_stored(&r)).transpose()
}

/// What [`repair`] did.
#[derive(Debug, Clone, PartialEq)]
pub struct Repaired {
    /// Settings key the unreadable text was copied to.
    pub kept_as: String,
    /// Fields reset to their defaults, e.g. `brain.id`.
    pub reset: Vec<String>,
}

/// Rebuild stored settings that can't be read: each field that reads (and
/// validates) on its own is kept, the app token among them, and the rest
/// go back to their defaults. The stored text is first copied to
/// `settings.unreadable.<time>`. `None` when the settings read fine.
pub async fn repair(pool: &sqlx::SqlitePool) -> anyhow::Result<Option<Repaired>> {
    let mut tx = crate::store::begin_immediate(pool).await?;
    let raw: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?").bind(KEY).fetch_optional(&mut *tx).await?;
    let Some(raw) = raw else { return Ok(None) };
    if parse_stored(&raw).is_ok() {
        return Ok(None);
    }
    let stored: Value = serde_json::from_str(&raw).unwrap_or(Value::Null);
    let mut good = serde_json::to_value(Settings::default())?;
    let mut reset = Vec::new();
    let fits = |v: &Value| serde_json::from_value::<Settings>(v.clone()).is_ok_and(|s| validate(&s).is_ok());
    if let Value::Object(fields) = &stored {
        for (k, v) in fields {
            match (v, good.get(k)) {
                (Value::Object(inner), Some(Value::Object(_))) => {
                    for (k2, v2) in inner {
                        let before = good[k].get(k2).cloned();
                        good[k][k2] = v2.clone();
                        if !fits(&good) {
                            match before {
                                Some(b) => good[k][k2] = b,
                                None => {
                                    good[k].as_object_mut().map(|o| o.remove(k2));
                                }
                            }
                            reset.push(format!("{k}.{k2}"));
                        }
                    }
                }
                _ => {
                    let before = good.get(k).cloned();
                    good[k] = v.clone();
                    if !fits(&good) {
                        match before {
                            Some(b) => good[k] = b,
                            None => {
                                good.as_object_mut().map(|o| o.remove(k));
                            }
                        }
                        reset.push(k.clone());
                    }
                }
            }
        }
    } else {
        reset.push("(everything)".into());
    }
    let kept_as = format!("{KEY}.unreadable.{}", crate::time::now().format("%Y%m%dT%H%M%S%.3fZ"));
    sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?)").bind(&kept_as).bind(&raw).execute(&mut *tx).await?;
    sqlx::query("UPDATE settings SET value = ? WHERE key = ?").bind(good.to_string()).bind(KEY).execute(&mut *tx).await?;
    tx.commit().await?;
    tracing::warn!(kept_as, reset = ?reset, "unreadable settings repaired");
    Ok(Some(Repaired { kept_as, reset }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collar_urls_must_be_https_and_reachable_by_name() {
        assert_eq!(collar_url_problem("https://farm.example.com"), None);
        assert_eq!(collar_url_problem("https://farm.example.com:8443/base/"), None);
        assert_eq!(collar_url_problem("https://192.168.1.20"), None);
        for bad in [
            "http://farm.example.com",
            "https://",
            "https://127.0.0.1:7878",
            "https://localhost",
            "https://LOCALHOST:7878",
            "https://[::1]:7878",
            "https://0.0.0.0",
            "https://abc-def.trycloudflare.com",
        ] {
            assert!(collar_url_problem(bad).is_some(), "{bad}");
        }
        assert!(collar_url_problem(&format!("https://{}.example.com", "a".repeat(250))).is_some());
    }
}
