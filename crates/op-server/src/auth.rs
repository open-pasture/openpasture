//! Who may use the app API and MCP.
//!
//! A request is *local* only when it comes from this machine (loopback peer),
//! names this machine in `Host` (127.0.0.1, localhost or [::1], any port), and
//! carries no proxy forwarding headers. Tunnels (cloudflared, Tailscale Funnel)
//! connect from loopback too, but they keep the public Host and add forwarding
//! headers, so their traffic is never local. Everything that isn't local needs
//! `Authorization: Bearer <app token>` (or `?token=` for the WebSocket). A
//! brain gets a short-lived token that only opens `/mcp?scope=brain`.
//!
//! Local requests carry ambient authority, so a browser page from another
//! origin must not use it: unsafe methods and the WebSocket upgrade are refused
//! when `Origin` is another site. Requests with a valid token don't depend on
//! ambient authority and skip that check.
//!
//! Collar endpoints (`/collar/v1`) use collar keys (op-ingest); `/v1/decide`
//! uses hosted keys (op-brain).

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use op_core::{ApiError, Ctx};

/// Origins of the Vite dev server, allowed when dev CORS is on.
pub const DEV_ORIGINS: [&str; 2] = ["http://localhost:5173", "http://127.0.0.1:5173"];

/// Headers a reverse proxy or tunnel adds. Any of them means "not local".
const FORWARDING_HEADERS: [&str; 6] = ["x-forwarded-for", "x-forwarded-host", "x-forwarded-proto", "forwarded", "cf-connecting-ip", "x-real-ip"];

#[derive(Clone)]
pub struct AuthState {
    pub ctx: Ctx,
    /// Also accept the Vite dev origins.
    pub dev: bool,
}

fn protected(path: &str) -> bool {
    path == "/api" || path.starts_with("/api/") || path == "/mcp" || path.starts_with("/mcp/")
}

/// Host without the port, lowercased. `[::1]:7878` -> `[::1]`.
fn host_name(host: &str) -> String {
    let host = host.trim().to_ascii_lowercase();
    if host.starts_with('[') {
        return host.split_once(']').map(|(h, _)| format!("{h}]")).unwrap_or(host);
    }
    host.rsplit_once(':').filter(|(_, p)| p.chars().all(|c| c.is_ascii_digit())).map(|(h, _)| h.to_owned()).unwrap_or(host)
}

fn loopback_host(host: &str) -> bool {
    matches!(host_name(host).as_str(), "127.0.0.1" | "localhost" | "[::1]")
}

/// The request came from this machine, for this machine, not through a proxy.
pub fn is_local(peer: Option<SocketAddr>, headers: &HeaderMap) -> bool {
    let Some(peer) = peer else { return false };
    let peer_ip = match peer.ip() {
        std::net::IpAddr::V6(v6) => v6.to_ipv4_mapped().map(std::net::IpAddr::V4).unwrap_or(std::net::IpAddr::V6(v6)),
        ip => ip,
    };
    if !peer_ip.is_loopback() {
        return false;
    }
    let Some(host) = headers.get(header::HOST).and_then(|h| h.to_str().ok()) else { return false };
    if !loopback_host(host) {
        return false;
    }
    let proxied = headers.keys().any(|k| {
        let k = k.as_str();
        FORWARDING_HEADERS.contains(&k) || k.starts_with("tailscale-")
    });
    !proxied
}

/// `Origin` is absent (not a browser), or the same host and port as `Host`,
/// or a dev origin when allowed.
pub fn origin_ok(headers: &HeaderMap, dev: bool) -> bool {
    let Some(origin) = headers.get(header::ORIGIN) else { return true };
    let Ok(origin) = origin.to_str() else { return false };
    let origin = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    if dev && DEV_ORIGINS.contains(&origin.as_str()) {
        return true;
    }
    let authority = origin.strip_prefix("http://").or_else(|| origin.strip_prefix("https://"));
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).map(|h| h.trim().to_ascii_lowercase());
    matches!((authority, host), (Some(a), Some(h)) if a == h)
}

fn given_token(req: &Request) -> Option<String> {
    let bearer = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer ").or_else(|| v.strip_prefix("bearer ")))
        .map(|v| v.trim().to_owned());
    bearer.or_else(|| req.uri().query().and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("token="))).map(str::to_owned))
}

pub async fn guard(State(st): State<AuthState>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if !protected(path) {
        return next.run(req).await;
    }
    let brain_scope = (path == "/mcp" || path.starts_with("/mcp/")) && req.uri().query().is_some_and(|q| q.split('&').any(|kv| kv == "scope=brain"));

    if let Some(given) = given_token(&req) {
        let app_token = match st.ctx.settings().await {
            Ok(s) => s.server.app_token,
            Err(e) => return ApiError::from(e).into_response(),
        };
        if eq(given.as_bytes(), app_token.as_bytes()) || (brain_scope && st.ctx.check_brain_token(&given)) {
            return next.run(req).await;
        }
        // A stale token from the browser falls through to the local check.
    }

    let peer = req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0);
    if !is_local(peer, req.headers()) {
        return ApiError::unauthorized("Missing or wrong app token.").into_response();
    }
    let upgrade = req.headers().get(header::UPGRADE).is_some();
    let unsafe_method = !matches!(*req.method(), Method::GET | Method::HEAD | Method::OPTIONS);
    if (upgrade || unsafe_method) && !origin_ok(req.headers(), st.dev) {
        return ApiError::new(StatusCode::FORBIDDEN, "Cross-site request refused.").into_response();
    }
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
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(), HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn local_needs_loopback_peer_host_and_no_proxy() {
        let lo: Option<SocketAddr> = Some("127.0.0.1:5000".parse().unwrap());
        let lo6: Option<SocketAddr> = Some("[::1]:5000".parse().unwrap());
        let lan: Option<SocketAddr> = Some("192.168.1.5:5000".parse().unwrap());
        assert!(is_local(lo, &headers(&[("host", "127.0.0.1:7878")])));
        assert!(is_local(lo, &headers(&[("host", "localhost")])));
        assert!(is_local(lo6, &headers(&[("host", "[::1]:7878")])));
        assert!(!is_local(None, &headers(&[("host", "127.0.0.1:7878")])));
        assert!(!is_local(lan, &headers(&[("host", "127.0.0.1:7878")])));
        assert!(!is_local(lo, &headers(&[])));
        assert!(!is_local(lo, &headers(&[("host", "evil.example:7878")])), "DNS rebinding");
        assert!(!is_local(lo, &headers(&[("host", "farm.example.com")])), "tunnel keeps the public host");
        assert!(!is_local(lo, &headers(&[("host", "127.0.0.1.evil.example")])));
        for h in ["x-forwarded-for", "forwarded", "cf-connecting-ip", "tailscale-user-login", "x-forwarded-host"] {
            assert!(!is_local(lo, &headers(&[("host", "127.0.0.1:7878"), (h, "1.2.3.4")])), "{h}");
        }
    }

    #[test]
    fn origin_must_match_host() {
        assert!(origin_ok(&headers(&[("host", "127.0.0.1:7878")]), false));
        assert!(origin_ok(&headers(&[("host", "127.0.0.1:7878"), ("origin", "http://127.0.0.1:7878")]), false));
        assert!(!origin_ok(&headers(&[("host", "127.0.0.1:7878"), ("origin", "http://evil.example")]), false));
        assert!(!origin_ok(&headers(&[("host", "127.0.0.1:7878"), ("origin", "http://127.0.0.1:9999")]), false));
        assert!(!origin_ok(&headers(&[("host", "127.0.0.1:7878"), ("origin", "null")]), false));
        assert!(!origin_ok(&headers(&[("host", "127.0.0.1:7878"), ("origin", "http://localhost:5173")]), false));
        assert!(origin_ok(&headers(&[("host", "127.0.0.1:7878"), ("origin", "http://localhost:5173")]), true));
    }
}
