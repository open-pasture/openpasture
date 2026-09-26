//! App settings, stored as JSON under the `settings` key of the settings table.

use rand::RngCore;

use crate::domain::{BrainSetting, ServerSettings, Settings, Units};

pub const KEY: &str = "settings";
pub const DEFAULT_BIND: &str = "127.0.0.1";
pub const DEFAULT_PORT: u16 = 7878;

impl Default for Settings {
    fn default() -> Self {
        Settings {
            brain: BrainSetting::default(),
            decision_time: "06:00".into(),
            server: ServerSettings { bind: DEFAULT_BIND.into(), port: DEFAULT_PORT, public_url: None, app_token: new_token() },
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
    Ok(())
}
