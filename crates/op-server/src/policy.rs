//! Who may call what (field-ready §2.2), checked by the guard for every
//! `/api` and `/mcp` request once it knows the [`Identity`]. Coarse on
//! purpose, so feature handlers stay simple:
//!
//! - **viewer**: `GET`/`HEAD` on `/api/*` except the owner's and managers'
//!   reads below; `/api/live`; `/mcp` (MCP lists and calls tools by role).
//!   Every role may also use [`ANY_ROLE`]: its own profile and token, its
//!   own push subscription.
//! - **hand**: a viewer plus [`HAND`].
//! - **manager**: every other `/api/*` request except [`OWNER`].
//! - **owner**: everything.
//!
//! [`OPEN`] needs no sign-in at all; the guard rate-limits it per peer.
//!
//! Patterns: `{x}` is one path segment, a trailing `*` any rest (`/api/users*`
//! is `/api/users` and everything under it). Method `*` is any method. A
//! stream adding a hand-level or owner-only endpoint adds a line under its
//! anchor here.

use std::collections::{HashMap, VecDeque};
use std::net::{IpAddr, SocketAddr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::{HeaderMap, Method};
use op_core::{ApiError, Identity, Role};

/// (method, path pattern)
type Rule = (&'static str, &'static str);

/// No sign-in: the code in the body is the credential.
pub const OPEN: &[Rule] = &[
    // @J
    ("POST", "/api/invites/accept"),
    // @A-engine
    // @A-notify
    // @D
    // @K-animals
    // @K-files
    // @I
    // @B
    // @G
    // @P
    // @Q
    // @C
    // @F
    // @S
    // @A3
    // @H
    // @L
    // @M
    // @Z
];

/// Every signed-in role, any method listed.
pub const ANY_ROLE: &[Rule] = &[
    // @J
    ("*", "/api/me"),
    ("*", "/api/me/*"),
    ("POST", "/api/push/subscriptions*"),
    ("DELETE", "/api/push/subscriptions*"),
    // @A-engine
    // @A-notify
    // @D
    // @K-animals
    // @K-files
    // @I
    // @B
    // @G
    // @P
    // @Q
    // @C
    // @F
    // @S
    // @A3
    // @H
    // @L
    // @M
    // @Z
];

/// What a hand may do beyond reading.
pub const HAND: &[Rule] = &[
    // @J
    ("POST", "/api/alerts/{id}/ack"),
    ("POST", "/api/alerts/{id}/resolve"),
    ("PUT", "/api/alerts/prefs/me"),
    ("POST", "/api/herds/{id}/move/stop"),
    ("POST", "/api/collars/{id}/escape/stop"),
    ("POST", "/api/collars/{id}/park"),
    ("POST", "/api/collars/{id}/unpark"),
    ("POST", "/api/fleet/{id}/fit-checks"),
    ("POST", "/api/fleet/fit-checks"),
    ("POST", "/api/paddocks/{id}/heights"),
    ("POST", "/api/feed-log"),
    ("POST", "/api/herds/{id}/check"),
    // @A-engine
    // @A-notify
    // @D
    // @K-animals
    // @K-files
    // @I
    // @B
    // @G
    // @P
    // @Q
    // @C
    // @F
    // @S
    // @A3
    // @H
    // @L
    // @M
    // @Z
];

/// Reads only managers and up may make (phone numbers, everyone's alert prefs).
pub const MANAGER_READS: &[Rule] = &[
    // @J
    ("GET", "/api/messages"),
    ("GET", "/api/alerts/prefs"),
    // @A-engine
    // @A-notify
    // @D
    // @K-animals
    // @K-files
    // @I
    // @B
    // @G
    // @P
    // @Q
    // @C
    // @F
    // @S
    // @A3
    // @H
    // @L
    // @M
    // @Z
];

/// The owner's alone, reads included unless a method is given.
pub const OWNER: &[Rule] = &[
    // @J
    ("PUT", "/api/settings"),
    ("*", "/api/secrets*"),
    ("*", "/api/users*"),
    ("*", "/api/invites*"),
    ("*", "/api/tokens*"),
    ("*", "/api/brains/hosted/keys*"),
    ("*", "/api/notify/*"),
    ("*", "/api/texting*"),
    ("PUT", "/api/push/settings"),
    ("POST", "/api/collars/{id}/rekey"),
    // @A-engine
    // @A-notify
    // @D
    // @K-animals
    // @K-files
    // @I
    // @B
    // @G
    // @P
    // @Q
    // @C
    // @F
    // @S
    // @A3
    // @H
    // @L
    // @M
    // @Z
];

fn path_matches(pattern: &str, path: &str) -> bool {
    if let Some(prefix) = pattern.strip_suffix('*') {
        return path.starts_with(prefix);
    }
    let mut want = pattern.split('/');
    let mut got = path.split('/');
    loop {
        match (want.next(), got.next()) {
            (None, None) => return true,
            (Some(w), Some(g)) if w.starts_with('{') && w.ends_with('}') => {
                if g.is_empty() {
                    return false;
                }
            }
            (Some(w), Some(g)) if w == g => {}
            _ => return false,
        }
    }
}

fn listed(rules: &[Rule], method: &Method, path: &str) -> bool {
    rules.iter().any(|(m, p)| (*m == "*" || *m == method.as_str()) && path_matches(p, path))
}

/// A request anyone may make without signing in.
pub fn is_open(method: &Method, path: &str) -> bool {
    listed(OPEN, method, path)
}

/// The lowest role that may make this request.
pub fn required(method: &Method, path: &str) -> Role {
    // HEAD reads what GET reads.
    let method = if *method == Method::HEAD { &Method::GET } else { method };
    if path == "/mcp" || path.starts_with("/mcp/") || is_open(method, path) {
        // MCP lists and calls tools by the caller's role.
        return Role::Viewer;
    }
    if listed(OWNER, method, path) {
        return Role::Owner;
    }
    if listed(ANY_ROLE, method, path) {
        return Role::Viewer;
    }
    if listed(MANAGER_READS, method, path) {
        return Role::Manager;
    }
    if matches!(*method, Method::GET | Method::OPTIONS) {
        return Role::Viewer;
    }
    if listed(HAND, method, path) {
        return Role::Hand;
    }
    Role::Manager
}

/// 403 `{"error": "Your role can't do this."}` when the identity's role is
/// below what the request needs (401 when anonymous).
pub fn check(identity: &Identity, method: &Method, path: &str) -> Result<(), ApiError> {
    identity.require(required(method, path))
}

// ---- rate limit for open requests -------------------------------------------------------

/// Open requests allowed per peer per minute.
pub const OPEN_PER_MINUTE: usize = 5;
const WINDOW: Duration = Duration::from_secs(60);

/// A sliding one-minute window per peer.
#[derive(Default)]
pub struct RateLimit {
    seen: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

impl RateLimit {
    /// Count one request from `peer`; false once it made
    /// [`OPEN_PER_MINUTE`] in the last minute.
    pub fn allow(&self, peer: IpAddr) -> bool {
        let now = Instant::now();
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, q| {
            while q.front().is_some_and(|t| now.duration_since(*t) >= WINDOW) {
                q.pop_front();
            }
            !q.is_empty()
        });
        let q = seen.entry(peer).or_default();
        if q.len() >= OPEN_PER_MINUTE {
            return false;
        }
        q.push_back(now);
        true
    }
}

/// Who is asking, for the rate limit: the peer address, or for a proxy or
/// tunnel on this machine (a loopback peer) the client it names: the last
/// `X-Forwarded-For` entry (the one the nearest proxy added),
/// `CF-Connecting-IP` or `X-Real-IP`.
pub fn peer_key(peer: Option<SocketAddr>, headers: &HeaderMap) -> IpAddr {
    let ip = peer.map(|p| p.ip()).unwrap_or(IpAddr::from([0, 0, 0, 0]));
    let ip = match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        ip => ip,
    };
    if !ip.is_loopback() {
        return ip;
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let named = header("x-forwarded-for")
        .and_then(|v| v.rsplit(',').next())
        .or_else(|| header("cf-connecting-ip"))
        .or_else(|| header("x-real-ip"))
        .and_then(|v| v.trim().parse::<IpAddr>().ok());
    named.unwrap_or(ip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn m(s: &str) -> Method {
        Method::from_bytes(s.as_bytes()).unwrap()
    }

    #[test]
    fn patterns_match_segments_and_prefixes() {
        assert!(path_matches("/api/alerts/{id}/ack", "/api/alerts/alr_1/ack"));
        assert!(!path_matches("/api/alerts/{id}/ack", "/api/alerts//ack"));
        assert!(!path_matches("/api/alerts/{id}/ack", "/api/alerts/alr_1/ack/x"));
        assert!(!path_matches("/api/alerts/{id}/ack", "/api/alerts/alr_1"));
        assert!(path_matches("/api/users*", "/api/users"));
        assert!(path_matches("/api/users*", "/api/users/usr_1/revoke"));
        assert!(path_matches("/api/me/*", "/api/me/profile"));
        assert!(!path_matches("/api/me/*", "/api/me"));
    }

    #[test]
    fn roles_by_method_and_path() {
        use Role::*;
        let cases = [
            ("GET", "/api/state", Viewer),
            ("HEAD", "/api/collars", Viewer),
            ("GET", "/api/live", Viewer),
            ("POST", "/mcp", Viewer),
            ("PATCH", "/api/me/profile", Viewer),
            ("POST", "/api/me/signout", Viewer),
            ("POST", "/api/push/subscriptions", Viewer),
            ("DELETE", "/api/push/subscriptions/psh_1", Viewer),
            ("POST", "/api/invites/accept", Viewer),
            ("GET", "/api/messages", Manager),
            ("HEAD", "/api/messages", Manager),
            ("HEAD", "/api/users", Owner),
            ("GET", "/api/alerts/prefs", Manager),
            ("GET", "/api/alerts/prefs/me", Viewer),
            ("PUT", "/api/alerts/prefs/me", Hand),
            ("POST", "/api/herds/herd_1/move/stop", Hand),
            ("POST", "/api/collars/col_1/escape/stop", Hand),
            ("POST", "/api/herds/herd_1/check", Hand),
            ("POST", "/api/herds/herd_1/boundary", Manager),
            ("PATCH", "/api/herds/herd_1", Manager),
            ("POST", "/api/sql", Manager),
            ("POST", "/api/decisions/dec_1/respond", Manager),
            ("PUT", "/api/settings", Owner),
            ("GET", "/api/settings", Viewer),
            ("GET", "/api/secrets", Owner),
            ("GET", "/api/users", Owner),
            ("POST", "/api/invites", Owner),
            ("DELETE", "/api/invites/inv_1", Owner),
            ("GET", "/api/tokens", Owner),
            ("GET", "/api/brains/hosted/keys", Owner),
            ("GET", "/api/brains", Viewer),
            ("PUT", "/api/notify/channels", Owner),
            ("GET", "/api/texting", Owner),
            ("PUT", "/api/push/settings", Owner),
            ("POST", "/api/collars/col_1/rekey", Owner),
            ("POST", "/api/collars/col_1/park", Hand),
        ];
        for (method, path, need) in cases {
            assert_eq!(required(&m(method), path), need, "{method} {path}");
        }
    }

    #[test]
    fn rate_limit_is_per_peer_per_minute() {
        let rl = RateLimit::default();
        let a: IpAddr = "203.0.113.9".parse().unwrap();
        let b: IpAddr = "203.0.113.10".parse().unwrap();
        for _ in 0..OPEN_PER_MINUTE {
            assert!(rl.allow(a));
        }
        assert!(!rl.allow(a));
        assert!(rl.allow(b));
    }

    #[test]
    fn peer_key_reads_the_proxy_only_on_loopback() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", HeaderValue::from_static("198.51.100.1, 203.0.113.9"));
        let lo: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let lan: SocketAddr = "192.168.1.5:1".parse().unwrap();
        assert_eq!(peer_key(Some(lo), &h), "203.0.113.9".parse::<IpAddr>().unwrap(), "the entry the nearest proxy added");
        assert_eq!(peer_key(Some(lan), &h), "192.168.1.5".parse::<IpAddr>().unwrap(), "a LAN peer can't name someone else");
        assert_eq!(peer_key(Some(lo), &HeaderMap::new()), "127.0.0.1".parse::<IpAddr>().unwrap());
    }
}
