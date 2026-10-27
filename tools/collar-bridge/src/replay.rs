//! `collar-bridge replay`: a captured session's requests, sent again in
//! order to a (test) server, each with the status it got then and now.
//! Boundaries and acks name command ids from the server they were captured
//! against, so on another server those differ; that is the point of showing both.
//!
//! A replay writes: old fixes, battery and acks land on whatever collar the
//! key belongs to. So nothing is sent until the target server says every
//! collar on it is simulated (read with `--api-key`), or the operator names
//! the farm it should go to anyway (`--into-farm`, matched against the
//! server's own farm name).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, bail};
use serde_json::Value;

use crate::capture::{self, Capture, Record};
use crate::uplink::{Uplink, error_text};

pub struct Options {
    pub capture: PathBuf,
    /// Server and collar key; without them nothing is sent (`--dry-run`).
    pub target: Option<(String, String)>,
    /// A token that reads the target server's collars and farm (any role).
    pub api_key: Option<String>,
    /// Replay into this farm even though it has collars that aren't simulated.
    pub into_farm: Option<String>,
    /// Keep the captured gaps between requests.
    pub pace: bool,
    /// Capture the replay itself.
    pub out: Arc<Capture>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Summary {
    pub requests: usize,
    pub same: usize,
    pub different: usize,
}

/// What the target server said about itself.
#[derive(Debug, PartialEq)]
pub enum Target {
    /// Every collar on it is simulated.
    Simulated { collars: usize },
    /// It has real collars, and the operator named this farm.
    Named { farm: String, real: usize },
}

/// Whether a replay may write to `server`; why not, in words, if not.
pub async fn check_target(server: &str, api_key: Option<&str>, into_farm: Option<&str>) -> anyhow::Result<Target> {
    let Some(token) = api_key else {
        bail!("a replay writes to the collar this key belongs to. Pass --api-key (a token for {server}) so the bridge can check its collars are simulated");
    };
    let http = reqwest::Client::new();
    let base = server.trim_end_matches('/');
    let base = base.strip_suffix("/collar/v1").unwrap_or(base);
    let collars = read(&http, base, "/api/collars", token).await?;
    let list = collars.as_array().context("/api/collars isn't a list")?;
    let real: Vec<&str> = list
        .iter()
        .filter(|c| c.get("simulated").and_then(Value::as_bool) != Some(true))
        .map(|c| c.get("name").and_then(Value::as_str).unwrap_or("?"))
        .collect();
    if real.is_empty() {
        return Ok(Target::Simulated { collars: list.len() });
    }
    let farm = read(&http, base, "/api/farm", token).await?;
    let name = farm.get("name").and_then(Value::as_str).unwrap_or_default().to_owned();
    match into_farm {
        Some(n) if n == name => Ok(Target::Named { farm: name, real: real.len() }),
        Some(n) => bail!("{base} is the farm {name:?}, not {n:?}. Nothing was sent"),
        None => bail!(
            "{base} ({name}) has {} collars that aren't simulated ({}{}). A replay could write old fixes and acks to one. Replay into a simulated farm, or pass --into-farm {name:?} if this is really where it should go",
            real.len(),
            real.iter().take(3).copied().collect::<Vec<_>>().join(", "),
            if real.len() > 3 { ", …" } else { "" }
        ),
    }
}

async fn read(http: &reqwest::Client, base: &str, path: &str, token: &str) -> anyhow::Result<Value> {
    let res = http.get(format!("{base}{path}")).bearer_auth(token).send().await.with_context(|| format!("{base} unreachable"))?;
    let status = res.status().as_u16();
    let text = res.text().await.unwrap_or_default();
    if status != 200 {
        bail!("{base}{path}: {status} {}", error_text(&text));
    }
    serde_json::from_str::<Value>(&text).with_context(|| format!("{base}{path} isn't JSON"))
}

pub async fn run(opts: Options) -> anyhow::Result<Summary> {
    let records = capture::read(&opts.capture)?;
    if let Some((server, _)) = &opts.target {
        match check_target(server, opts.api_key.as_deref(), opts.into_farm.as_deref()).await? {
            Target::Simulated { collars } => println!("{server}: {collars} collars, every one simulated"),
            Target::Named { farm, real } => println!("{server}: replaying into {farm} as named, which has {real} collars that aren't simulated"),
        }
    }
    let uplink = opts.target.as_ref().map(|(server, key)| Uplink::new(server, key, opts.out.clone()));
    let mut sum = Summary::default();
    let mut last_t: Option<chrono::DateTime<chrono::Utc>> = None;
    for rec in records {
        let Record::Http { t, method, path, body, status, reply, error, .. } = rec else { continue };
        sum.requests += 1;
        let then = match (status, &reply, &error) {
            (Some(s), Some(r), _) if s >= 400 => format!("{s} {}", error_text(r)),
            (Some(s), _, _) => s.to_string(),
            (None, _, Some(e)) => format!("unreachable ({e})"),
            (None, _, None) => "unreachable".into(),
        };
        let Some(uplink) = &uplink else {
            println!("{} {method} {path}: {then}", t.format("%H:%M:%S"));
            continue;
        };
        if opts.pace
            && let Some(prev) = last_t
            && let Ok(gap) = (t - prev).to_std()
        {
            tokio::time::sleep(gap.min(Duration::from_secs(3600))).await;
        }
        last_t = Some(t);
        let now = match uplink.resend(&method, &path, body).await {
            Ok((s, r)) => {
                if Some(s) == status {
                    sum.same += 1;
                } else {
                    sum.different += 1;
                }
                if s >= 400 { format!("{s} {}", error_text(&r)) } else { s.to_string() }
            }
            Err(e) => {
                sum.different += 1;
                format!("unreachable ({e})")
            }
        };
        println!("{method} {path}: was {then}, now {now}");
    }
    Ok(sum)
}
