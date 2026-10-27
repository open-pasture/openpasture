//! `collar-bridge replay`: a captured session's requests, sent again in
//! order to a (test) server, each with the status it got then and now.
//! Boundaries and acks name command ids from the server they were captured
//! against, so on another server those differ; that is the point of showing both.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::capture::{self, Capture, Record};
use crate::uplink::{Uplink, error_text};

pub struct Options {
    pub capture: PathBuf,
    /// Server and collar key; without them nothing is sent (`--dry-run`).
    pub target: Option<(String, String)>,
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

pub async fn run(opts: Options) -> anyhow::Result<Summary> {
    let records = capture::read(&opts.capture)?;
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
