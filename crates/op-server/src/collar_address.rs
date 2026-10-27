//! The collar address: where collars reach this server. It goes into every
//! collar (links, cards, signed configs), and a collar given a wrong one goes
//! quiet for up to a day before it falls back, so it is checked before it is
//! saved: the form ([`op_core::settings::collar_url_problem`]), that the name
//! resolves, that https answers with a valid certificate, and that what
//! answers is this server. The last step asks
//! `<address>/collar/v1/reach/<nonce>` through the address itself, and only
//! this process knows the nonce.
//!
//! - `POST /api/server/collar-address/check {url}`: the check alone.
//! - `PUT /api/server/collar-address {url}`: check, then save and hand the new
//!   address to every collar that takes configs. `null` clears it. This is
//!   the only way to set it: generic settings writes refuse it (409).
//! - `GET /api/server/collar-address`: the address and where collars stand on
//!   it. A collar keeps its old address until a report through the new one
//!   shows it moved (and goes back to the old one if the new one fails for a
//!   day), so an older address has to keep working while any collar is still
//!   on it: `moving` lists those, `fixed` counts collars that take no configs
//!   and so stay on the address they were set up with.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use op_core::settings::CheckedCollarUrl;
use op_core::{ApiError, ApiJson, ApiResult, Ctx};
use serde::{Deserialize, Serialize};

/// How long a nonce stays answerable, and how long the request may take.
const NONCE_TTL: Duration = Duration::from_secs(60);
const TIMEOUT: Duration = Duration::from_secs(10);

/// Nonces this process is waiting to hear back through a collar address.
static PENDING: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(Mutex::default);

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/collar/v1/reach/{nonce}", get(reach))
        .route("/api/server/collar-address/check", post(post_check))
        .route("/api/server/collar-address", get(get_address).put(put_address))
}

/// 204 when the nonce is one this process is waiting for, else 404.
async fn reach(Path(nonce): Path<String>) -> StatusCode {
    let mut pending = PENDING.lock().unwrap_or_else(|e| e.into_inner());
    let now = Instant::now();
    pending.retain(|_, at| now.duration_since(*at) < NONCE_TTL);
    if pending.remove(&nonce).is_some() { StatusCode::NO_CONTENT } else { StatusCode::NOT_FOUND }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Step {
    /// https, a host collars can reach, short enough.
    Address,
    /// The host name resolves.
    Dns,
    /// https answers with a certificate the collar would accept.
    Https,
    /// What answers is this server.
    Server,
}

#[derive(Debug, Serialize)]
pub struct Check {
    pub ok: bool,
    /// The address as it would be saved (no trailing slash).
    pub url: String,
    /// What collars would be given.
    pub endpoint: String,
    /// The steps that passed, in order.
    pub passed: Vec<Step>,
    /// The step that failed and why, in a sentence.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<Step>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub problem: Option<String>,
}

impl Check {
    fn fail(mut self, step: Step, problem: impl Into<String>) -> Self {
        self.ok = false;
        self.failed = Some(step);
        self.problem = Some(problem.into());
        self
    }
}

#[derive(Deserialize)]
struct CheckBody {
    url: String,
}

async fn post_check(ApiJson(body): ApiJson<CheckBody>) -> Json<Check> {
    Json(check(&body.url).await)
}

#[derive(Deserialize)]
struct PutBody {
    url: Option<String>,
}

#[derive(Serialize)]
struct Saved {
    collar_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    check: Option<Check>,
    #[serde(flatten)]
    status: op_ingest::config::AddressStatus,
}

/// The collar address and where collars stand on it.
async fn get_address(State(ctx): State<Ctx>) -> ApiResult<Json<Saved>> {
    let collar_url = ctx.settings().await?.server.collar_url;
    Ok(Json(Saved { collar_url, check: None, status: op_ingest::config::address_status(&ctx).await? }))
}

/// Save the collar address once it passes the check (422 with the reason
/// otherwise); every collar that takes configs gets it in its next one.
async fn put_address(State(ctx): State<Ctx>, ApiJson(body): ApiJson<PutBody>) -> ApiResult<Json<Saved>> {
    let (url, check) = match body.url.as_deref().map(str::trim).filter(|u| !u.is_empty()) {
        None => (CheckedCollarUrl::none(), None),
        Some(u) => {
            let c = check(u).await;
            if !c.ok {
                return Err(ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, c.problem.unwrap_or_else(|| "The collar address didn't pass the check.".into())));
            }
            (CheckedCollarUrl::passed(c.url.clone()), Some(c))
        }
    };
    let saved = ctx.set_collar_url(&url).await?;
    tracing::info!(collar_url = ?saved.server.collar_url, "collar address saved");
    op_ingest::config::collar_address_changed(&ctx).await;
    Ok(Json(Saved { collar_url: saved.server.collar_url, check, status: op_ingest::config::address_status(&ctx).await? }))
}

/// Check that collars could use `url` to reach this server.
pub async fn check(url: &str) -> Check {
    let url = url.trim().trim_end_matches('/').to_owned();
    let mut c = Check { ok: true, endpoint: format!("{url}/collar/v1"), url: url.clone(), passed: Vec::new(), failed: None, problem: None };
    if let Some(why) = op_core::settings::collar_url_problem(&url) {
        return c.fail(Step::Address, why);
    }
    let Ok(parsed) = reqwest::Url::parse(&url) else {
        return c.fail(Step::Address, "That isn't a web address.");
    };
    let (Some(host), Some(port)) = (parsed.host_str().map(str::to_owned), parsed.port_or_known_default()) else {
        return c.fail(Step::Address, "The collar address needs a host name.");
    };
    c.passed.push(Step::Address);

    let bare = host.trim_start_matches('[').trim_end_matches(']').to_owned();
    match tokio::time::timeout(TIMEOUT, tokio::net::lookup_host((bare.as_str(), port))).await {
        Ok(Ok(addrs)) => {
            let addrs: Vec<_> = addrs.collect();
            if addrs.is_empty() {
                return c.fail(Step::Dns, format!("No address found for {host}. Check the name, or give DNS a few minutes to catch up."));
            }
            // `::ffff:127.0.0.1` is loopback too.
            if addrs.iter().all(|a| a.ip().to_canonical().is_loopback() || a.ip().is_unspecified()) {
                return c.fail(Step::Dns, format!("{host} points at this computer itself, which a collar can't reach."));
            }
        }
        Ok(Err(_)) => return c.fail(Step::Dns, format!("No address found for {host}. Check the name, or give DNS a few minutes to catch up.")),
        Err(_) => return c.fail(Step::Dns, format!("Looking up {host} took too long.")),
    }
    // Hosted: a collar address is on the public internet, never this service's own network.
    if op_core::net::hosted()
        && let Err(why) = op_core::net::check_public(&url).await
    {
        return c.fail(Step::Dns, why);
    }
    c.passed.push(Step::Dns);

    let nonce = op_core::settings::new_token();
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).insert(nonce.clone(), Instant::now());
    let result = ask(&format!("{url}/collar/v1/reach/{nonce}")).await;
    PENDING.lock().unwrap_or_else(|e| e.into_inner()).remove(&nonce);
    match result {
        Err(e) => {
            let msg = chain(&e);
            if e.is_timeout() {
                c.fail(Step::Https, format!("{host} took too long to answer over https."))
            } else if looks_like_tls(&msg) {
                c.fail(Step::Https, format!("{host} answered, but its certificate isn't one a collar would accept ({msg})."))
            } else {
                c.fail(Step::Https, format!("Couldn't reach {host} on port {port} over https."))
            }
        }
        Ok(status) => {
            c.passed.push(Step::Https);
            match status {
                StatusCode::NO_CONTENT => {
                    c.passed.push(Step::Server);
                    c
                }
                s if s.is_redirection() => {
                    c.fail(Step::Server, format!("{host} redirects elsewhere. Collars don't follow redirects; give the address it redirects to."))
                }
                StatusCode::NOT_FOUND => {
                    c.fail(Step::Server, format!("{host} answered, but not as this server. Check the tunnel or proxy points at this computer and port."))
                }
                s => c.fail(Step::Server, format!("{host} answered with {}, not as this server.", s.as_u16())),
            }
        }
    }
}

/// GET without redirects, as a collar would.
async fn ask(url: &str) -> Result<StatusCode, reqwest::Error> {
    // Hosted: names resolve to public addresses only, at connect time (op_core::net).
    let client = op_core::net::client_builder().timeout(TIMEOUT).redirect(reqwest::redirect::Policy::none()).build()?;
    let res = client.get(url).send().await?;
    Ok(StatusCode::from_u16(res.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY))
}

fn chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut parts = Vec::new();
    let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
    while let Some(err) = cur {
        parts.push(err.to_string());
        cur = err.source();
    }
    parts.last().cloned().unwrap_or_default()
}

fn looks_like_tls(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    ["certificate", "tls", "handshake", "ssl", "unknownissuer", "notvalidforname", "expired"].iter().any(|k| m.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn refuses_addresses_collars_cant_use_before_asking_the_network() {
        for (url, step) in
            [("http://farm.example.com", Step::Address), ("https://127.0.0.1:7878", Step::Address), ("https://x.trycloudflare.com", Step::Address)]
        {
            let c = check(url).await;
            assert!(!c.ok, "{url}");
            assert_eq!(c.failed, Some(step), "{url}");
            assert!(c.problem.as_deref().is_some_and(|p| p.ends_with('.')), "a sentence: {:?}", c.problem);
        }
    }

    #[tokio::test]
    async fn a_name_that_doesnt_resolve_fails_at_dns() {
        let c = check("https://collar-address-check.invalid/").await;
        assert_eq!(c.url, "https://collar-address-check.invalid");
        assert_eq!(c.endpoint, "https://collar-address-check.invalid/collar/v1");
        assert_eq!(c.passed, vec![Step::Address]);
        assert_eq!(c.failed, Some(Step::Dns));
    }

    #[tokio::test]
    async fn a_nonce_answers_once_and_only_while_pending() {
        PENDING.lock().unwrap().insert("n1".into(), Instant::now());
        assert_eq!(reach(Path("n1".into())).await, StatusCode::NO_CONTENT);
        assert_eq!(reach(Path("n1".into())).await, StatusCode::NOT_FOUND);
        assert_eq!(reach(Path("never".into())).await, StatusCode::NOT_FOUND);
        PENDING.lock().unwrap().insert("old".into(), Instant::now() - NONCE_TTL - Duration::from_secs(1));
        assert_eq!(reach(Path("old".into())).await, StatusCode::NOT_FOUND);
    }

    #[test]
    fn tls_failures_are_told_apart() {
        assert!(looks_like_tls("invalid peer certificate: UnknownIssuer"));
        assert!(looks_like_tls("received fatal alert: HandshakeFailure"));
        assert!(!looks_like_tls("Connection refused (os error 61)"));
    }
}
