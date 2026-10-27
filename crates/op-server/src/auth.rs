//! Who may use the app API and MCP.
//!
//! Every `/api` and `/mcp` request names who it is with a token in
//! `Authorization: Bearer …`. Where a request comes from grants nothing: a
//! reverse proxy on this machine (nginx's default `proxy_pass` rewrites
//! `Host` and adds no forwarding headers) makes every internet request look
//! local, so being local is no credential. The owner on this machine signs
//! in with a one-time code instead (the desktop app's launch, the URL
//! `openpasture serve` prints; [`crate::session`]).
//!
//! The tokens:
//! - the app token (`openpasture token`; revealed only on purpose): the owner, `app_token`;
//! - an owner session (`opo_…`, from a one-time code): the owner, `local`;
//! - a person's own token (`opu_…`, from a sign-in link): that person, with their role;
//! - a brain run's token (`opb_…`): only `/mcp?scope=brain`, read tools only;
//! - a factory station's token (op-fleet): its own routes.
//!
//! Sign-ins run out ([`op_core::people::TOKEN_IDLE_DAYS`] unused,
//! [`op_core::people::TOKEN_MAX_DAYS`] old). A token in the query string
//! (`?token=`) only counts on the two routes a browser or an MCP client
//! can't give headers to: the `/api/live` WebSocket (which also takes it as a
//! `Sec-WebSocket-Protocol` value, see [`crate::live`]) and `/mcp`. Anywhere
//! else it would end up in proxy and access logs.
//!
//! A peer that sends [`AUTH_FAILURES_PER_MINUTE`] wrong tokens in a minute
//! gets 429 until the minute is up.
//!
//! Collar endpoints (`/collar/v1`) use collar keys (op-ingest); `/v1/decide`
//! uses hosted keys (op-brain). Everything outside `/api` and `/mcp` is
//! anonymous here (those endpoints authenticate themselves).
//!
//! The owner acts as the owner person when they added themselves to People.
//! Every identity then passes [`crate::policy`]: a role too low for the
//! request is 403. The few requests that need no sign-in (accepting a
//! sign-in link, trading a one-time code) are limited to a few tries a
//! minute per peer.

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use op_core::people::{self, SessionToken};
use op_core::tools::ToolScope;
use op_core::{ApiError, Ctx, Identity, Via};

use crate::{policy, session};

/// Origins of the Vite dev server, allowed when dev CORS is on.
pub const DEV_ORIGINS: [&str; 2] = ["http://localhost:5173", "http://127.0.0.1:5173"];
/// Wrong tokens one peer may send in a minute.
pub const AUTH_FAILURES_PER_MINUTE: usize = 30;

#[derive(Clone)]
pub struct AuthState {
    pub ctx: Ctx,
    /// Requests that need no sign-in (accepting a sign-in link), per peer.
    pub open: Arc<policy::RateLimit>,
    /// Wrong tokens, per peer.
    pub failed: Arc<policy::RateLimit>,
}

impl AuthState {
    pub fn new(ctx: Ctx) -> Self {
        Self { ctx, open: Arc::default(), failed: Arc::new(policy::RateLimit::new(AUTH_FAILURES_PER_MINUTE)) }
    }
}

fn protected(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/") || path == "/mcp" || path.starts_with("/mcp/")
}

/// The routes a token may come in the query string on.
fn query_token_route(path: &str) -> bool {
    path == "/api/live" || path == "/mcp" || path.starts_with("/mcp/")
}

fn given_token(req: &Request) -> Option<String> {
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let query = || {
        query_token_route(req.uri().path())
            .then(|| req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token="))).map(str::to_owned))
            .flatten()
    };
    bearer.or_else(|| crate::live::protocol_token(req.headers())).or_else(query)
}

pub async fn guard(State(st): State<AuthState>, mut req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if !protected(path) {
        req.extensions_mut().insert(Identity::anonymous());
        return next.run(req).await;
    }
    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    let peer = policy::peer_key(peer, req.headers());
    if policy::is_open(req.method(), path) {
        if !st.open.allow(peer) {
            return too_many();
        }
        req.extensions_mut().insert(Identity::anonymous());
        return next.run(req).await;
    }
    if st.failed.full(peer) {
        return too_many();
    }
    let brain_scope = (path == "/mcp" || path.starts_with("/mcp/")) && req.uri().query().is_some_and(|q| q.split('&').any(|kv| kv == "scope=brain"));

    let Some(given) = given_token(&req) else {
        return ApiError::unauthorized("Sign in first: open the sign-in link from this machine, or use a token.").into_response();
    };
    // Long-lived streams (the live feed) check it again while they run.
    req.extensions_mut().insert(Credential(given.clone()));
    let app_token = match st.ctx.settings().await {
        Ok(s) => s.server.app_token,
        Err(e) => return ApiError::from(e).into_response(),
    };
    if eq(given.as_bytes(), app_token.as_bytes()) {
        let identity = people::with_owner_person(&st.ctx, Identity::owner(Via::AppToken)).await;
        return admit(req, next, identity).await;
    }
    if brain_scope && let Some(tools) = st.ctx.check_brain_token(&given) {
        req.extensions_mut().insert(ToolScope::Only(tools));
        return admit(req, next, Identity::brain()).await;
    }
    if given.starts_with(session::SESSION_PREFIX) {
        return match session::session_for_token(&st.ctx, &given).await {
            Ok(Some(id)) => {
                req.extensions_mut().insert(session::OwnerSession(id));
                let identity = session::owner_identity(&st.ctx).await;
                admit(req, next, identity).await
            }
            Ok(None) => failed(&st, peer, "This sign-in has run out. Open the app again, or run openpasture sign-in."),
            Err(e) => ApiError::from(e).into_response(),
        };
    }
    if given.starts_with(people::TOKEN_PREFIX) {
        return match people::session_for_token(&st.ctx, &given).await {
            Ok(Some(s)) => {
                req.extensions_mut().insert(SessionToken(s.token_id));
                admit(req, next, s.identity).await
            }
            Ok(None) => failed(&st, peer, "This sign-in no longer works. Ask the owner for a new link."),
            Err(e) => ApiError::from(e).into_response(),
        };
    }
    // A factory station's token (op-fleet): its five routes only.
    if given.starts_with(op_fleet::stations::TOKEN_PREFIX) {
        return match op_fleet::stations::admit(&st.ctx, &given, req.method(), req.uri().path()).await {
            Ok(identity) => admit(req, next, identity).await,
            Err(e) => {
                if e.status == StatusCode::UNAUTHORIZED {
                    st.failed.allow(peer);
                }
                e.into_response()
            }
        };
    }
    failed(&st, peer, "Wrong token.")
}

/// The token a request signed in with, for a stream to check again while it runs ([`still_signed_in`]).
#[derive(Clone)]
pub struct Credential(pub String);

/// Whether `token` still signs in as `identity` would now: the app token not
/// rotated, the owner session or person's token not signed out, revoked or
/// run out, the person enabled with the same role, a brain token unexpired.
/// Anything else, a check that fails included, is no.
pub async fn still_signed_in(ctx: &Ctx, token: &str, identity: &Identity) -> bool {
    match signed_in_as(ctx, token).await {
        Ok(Some(now)) => now.role == identity.role && now.via == identity.via && now.user_id == identity.user_id,
        Ok(None) => false,
        Err(e) => {
            tracing::warn!("checking a live sign-in: {e:#}");
            false
        }
    }
}

async fn signed_in_as(ctx: &Ctx, token: &str) -> anyhow::Result<Option<Identity>> {
    if eq(token.as_bytes(), ctx.settings().await?.server.app_token.as_bytes()) {
        return Ok(Some(people::with_owner_person(ctx, Identity::owner(Via::AppToken)).await));
    }
    if ctx.check_brain_token(token).is_some() {
        return Ok(Some(Identity::brain()));
    }
    if token.starts_with(session::SESSION_PREFIX) {
        return Ok(match session::session_for_token(ctx, token).await? {
            Some(_) => Some(session::owner_identity(ctx).await),
            None => None,
        });
    }
    if token.starts_with(people::TOKEN_PREFIX) {
        return Ok(people::session_for_token(ctx, token).await?.map(|s| s.identity));
    }
    Ok(None)
}

fn failed(st: &AuthState, peer: std::net::IpAddr, msg: &str) -> Response {
    st.failed.allow(peer);
    ApiError::unauthorized(msg).into_response()
}

fn too_many() -> Response {
    let mut r = ApiError::new(StatusCode::TOO_MANY_REQUESTS, "Too many tries. Wait a minute and try again.").into_response();
    r.headers_mut().insert(header::RETRY_AFTER, axum::http::HeaderValue::from_static("60"));
    r
}

/// Let the identity in if its role may make this request (403 otherwise).
async fn admit(mut req: Request, next: Next, identity: Identity) -> Response {
    if let Err(e) = policy::check(&identity, req.method(), req.uri().path()) {
        return e.into_response();
    }
    req.extensions_mut().insert(identity);
    next.run(req).await
}

fn eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn is_loopback(bind: &str) -> bool {
    bind == "localhost" || bind.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    fn req(uri: &str, auth: Option<&str>) -> Request {
        let mut b = axum::http::Request::builder().uri(uri);
        if let Some(a) = auth {
            b = b.header("authorization", a);
        }
        b.body(Body::empty()).unwrap()
    }

    #[test]
    fn a_query_token_counts_only_where_headers_cant_go() {
        assert_eq!(given_token(&req("/api/live?token=abc", None)).as_deref(), Some("abc"));
        assert_eq!(given_token(&req("/mcp?scope=brain&token=abc", None)).as_deref(), Some("abc"));
        assert_eq!(given_token(&req("/api/settings?token=abc", None)), None, "it would land in access logs");
        assert_eq!(given_token(&req("/api/settings?token=abc", Some("Bearer xyz"))).as_deref(), Some("xyz"));
        assert_eq!(given_token(&req("/api/state", Some("Bearer "))), None);
    }
}
