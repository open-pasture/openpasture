//! Who may call what (field-ready §2.2), checked by the guard for every
//! `/api` and `/mcp` request once it knows the [`Identity`]. Coarse on
//! purpose, so feature handlers stay simple. Every route is listed with the
//! lowest role that may use it; a route on no list is the owner's alone
//! (denied to everyone else), and a test fails until it is listed.
//!
//! - **viewer**: [`VIEWER`] reads; `/mcp` (MCP lists and calls tools by
//!   role). Every role may also use [`ANY_ROLE`]: its own profile and token,
//!   its own push subscription, signing its own session out.
//! - **hand**: a viewer plus [`HAND`].
//! - **manager**: [`MANAGER_READS`] and [`MANAGER`].
//! - **owner**: everything, [`OWNER`] alone.
//!
//! [`OPEN`] needs no sign-in at all; the guard rate-limits it per peer.
//!
//! Patterns: `{x}` is one path segment, a trailing `*` any rest (`/api/users*`
//! is `/api/users` and everything under it). Method `*` is any method. A
//! stream adding an endpoint adds a line to the list for its role.

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
    // The owner's one-time sign-in code (op-server session).
    ("POST", "/api/session"),
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
    ("DELETE", "/api/session"),
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
    // Units, factory tests, firmware and the herd network (op-fleet). `GET /api/fleet` stays fleet care.
    ("GET", "/api/fleet/units*"),
    ("GET", "/api/fleet/test-plans*"),
    ("GET", "/api/fleet/firmware*"),
    ("GET", "/api/fleet/signing-keys*"),
    ("GET", "/api/fleet/network*"),
    ("GET", "/api/fleet/anchors*"),
    ("GET", "/api/fleet/collars/*"),
    ("GET", "/api/fleet/provisioning"),
    ("GET", "/api/fleet/yield"),
    ("GET", "/api/fleet/throughput"),
    ("GET", "/api/fleet/summary"),
    // @Z
];

/// The owner's alone, reads included unless a method is given.
pub const OWNER: &[Rule] = &[
    // @J
    ("PUT", "/api/settings"),
    // Revealing or replacing the app token.
    ("POST", "/api/settings/app-token/*"),
    ("*", "/api/secrets*"),
    ("*", "/api/users*"),
    ("*", "/api/invites*"),
    ("*", "/api/tokens*"),
    ("*", "/api/brains/hosted/keys*"),
    ("*", "/api/notify/*"),
    ("*", "/api/texting*"),
    ("PUT", "/api/push/settings"),
    ("POST", "/api/collars/{id}/rekey"),
    // The collar address goes into every collar.
    ("*", "/api/server/collar-address*"),
    // Retention deletes raw telemetry.
    ("PUT", "/api/server/retention"),
    // @A-engine
    // Someone else's alert prefs (people ids are `usr_…`; `/api/alerts/prefs/me` stays a hand's).
    ("PUT", "/api/alerts/prefs/usr_*"),
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
    // op-fleet: station tokens, test plans, the maker, firmware releases and keys.
    ("*", "/api/fleet/stations*"),
    ("PUT", "/api/fleet/test-plans/{class}"),
    ("PUT", "/api/fleet/provisioning"),
    ("POST", "/api/fleet/firmware"),
    ("PUT", "/api/fleet/firmware/{id}/image"),
    ("POST", "/api/fleet/firmware/{id}/withdraw"),
    ("POST", "/api/fleet/signing-keys*"),
    // @Z
];

/// Reads every signed-in role may make.
pub const VIEWER: &[Rule] = &[
    ("GET", "/api/alerts"),
    ("GET", "/api/alerts/prefs/me"),
    ("GET", "/api/alerts/rules"),
    ("GET", "/api/alerts/{id}"),
    ("GET", "/api/analytics/behaviour"),
    ("GET", "/api/analytics/health"),
    ("GET", "/api/analytics/heatmap"),
    ("GET", "/api/analytics/pasture"),
    ("GET", "/api/animals"),
    ("GET", "/api/animals/{id}"),
    ("GET", "/api/brains"),
    ("GET", "/api/brief"),
    ("GET", "/api/collars"),
    ("GET", "/api/collars/config"),
    ("GET", "/api/collars/{id}"),
    ("GET", "/api/collars/{id}/slots"),
    ("GET", "/api/collars/{id}/wire"),
    ("GET", "/api/coverage"),
    ("GET", "/api/decisions"),
    ("GET", "/api/decisions/{id}"),
    ("GET", "/api/export"),
    ("GET", "/api/farm"),
    ("GET", "/api/features"),
    ("GET", "/api/features/{id}"),
    ("GET", "/api/feed-log"),
    ("GET", "/api/fleet"),
    ("GET", "/api/fleet/settings"),
    ("GET", "/api/fleet/{collar_id}/fit-checks"),
    ("GET", "/api/gate/log"),
    ("GET", "/api/gate/settings"),
    ("GET", "/api/herds"),
    ("GET", "/api/herds/{id}"),
    ("GET", "/api/herds/{id}/boundary"),
    ("GET", "/api/herds/{id}/slots"),
    ("GET", "/api/import/positions"),
    ("GET", "/api/import/positions/tracks"),
    ("GET", "/api/import/positions/{id}"),
    ("GET", "/api/knowledge"),
    ("GET", "/api/land/{paddock_id}"),
    ("GET", "/api/layers/paddocks"),
    ("GET", "/api/layouts"),
    ("GET", "/api/layouts/{id}"),
    ("GET", "/api/leases"),
    ("GET", "/api/leases/{paddock_id}"),
    ("GET", "/api/live"),
    ("GET", "/api/paddocks"),
    ("GET", "/api/paddocks/{id}"),
    ("GET", "/api/paddocks/{id}/heights"),
    ("GET", "/api/positions"),
    ("GET", "/api/push"),
    ("GET", "/api/reports"),
    ("GET", "/api/reports/settings"),
    ("GET", "/api/reports/{id}"),
    ("GET", "/api/rules"),
    ("GET", "/api/schedules"),
    ("GET", "/api/schedules/{id}"),
    ("GET", "/api/schedules/{id}/moves"),
    ("GET", "/api/server"),
    ("GET", "/api/server/retention"),
    ("GET", "/api/settings"),
    ("GET", "/api/signals"),
    ("GET", "/api/state"),
    ("GET", "/api/tracks"),
    ("GET", "/api/welfare/animals"),
    ("GET", "/api/welfare/animals/{animal_id}/cues"),
    ("GET", "/api/welfare/cues/points"),
    ("GET", "/api/welfare/training/{herd_id}"),
];

/// Writes a manager may make.
pub const MANAGER: &[Rule] = &[
    ("PUT", "/api/alerts/prefs/{id}"),
    ("PUT", "/api/alerts/rules"),
    ("POST", "/api/animals"),
    ("POST", "/api/animals/import/preview"),
    ("POST", "/api/animals/import/{import_id}/commit"),
    ("DELETE", "/api/animals/{id}"),
    ("PATCH", "/api/animals/{id}"),
    ("POST", "/api/animals/{id}/remove"),
    ("POST", "/api/animals/{id}/swap"),
    ("POST", "/api/brains/{id}/test"),
    ("POST", "/api/cards"),
    ("POST", "/api/collars"),
    ("POST", "/api/collars/bulk"),
    ("PUT", "/api/collars/config"),
    ("DELETE", "/api/collars/{id}"),
    ("PATCH", "/api/collars/{id}"),
    ("POST", "/api/decisions/{id}/respond"),
    ("PATCH", "/api/farm"),
    ("POST", "/api/farm"),
    ("POST", "/api/features"),
    ("DELETE", "/api/features/{id}"),
    ("PATCH", "/api/features/{id}"),
    ("DELETE", "/api/feed-log/{id}"),
    ("PATCH", "/api/feed-log/{id}"),
    ("POST", "/api/fleet/anchors"),
    ("DELETE", "/api/fleet/anchors/{id}"),
    ("PATCH", "/api/fleet/anchors/{id}"),
    ("POST", "/api/fleet/claim"),
    ("PUT", "/api/fleet/firmware/policy"),
    ("POST", "/api/fleet/firmware/rollouts"),
    ("POST", "/api/fleet/firmware/rollouts/{id}/{action}"),
    ("PUT", "/api/fleet/settings"),
    ("POST", "/api/fleet/units"),
    ("POST", "/api/fleet/units/{serial}/claim"),
    ("POST", "/api/fleet/units/{serial}/provision"),
    ("POST", "/api/fleet/units/{serial}/state"),
    ("POST", "/api/fleet/units/{serial}/tests"),
    ("PUT", "/api/gate/settings"),
    ("POST", "/api/herds"),
    ("DELETE", "/api/herds/{id}"),
    ("PATCH", "/api/herds/{id}"),
    ("POST", "/api/herds/{id}/boundary"),
    ("POST", "/api/herds/{id}/decide"),
    ("POST", "/api/import/paddocks/commit"),
    ("POST", "/api/import/paddocks/preview"),
    ("POST", "/api/import/positions/preview"),
    ("DELETE", "/api/import/positions/{id}"),
    ("POST", "/api/import/positions/{id}/commit"),
    ("POST", "/api/import/positions/{id}/preview"),
    ("POST", "/api/layouts"),
    ("DELETE", "/api/layouts/{id}"),
    ("PATCH", "/api/layouts/{id}"),
    ("POST", "/api/layouts/{id}/apply"),
    ("DELETE", "/api/leases/{paddock_id}"),
    ("PUT", "/api/leases/{paddock_id}"),
    ("POST", "/api/paddocks"),
    ("DELETE", "/api/paddocks/{id}"),
    ("PATCH", "/api/paddocks/{id}"),
    ("POST", "/api/paddocks/{id}/copy"),
    ("PUT", "/api/reports/settings"),
    ("POST", "/api/rules"),
    ("DELETE", "/api/rules/{id}"),
    ("PATCH", "/api/rules/{id}"),
    ("POST", "/api/rules/{id}/convert"),
    ("POST", "/api/schedules"),
    ("POST", "/api/schedules/preview"),
    ("POST", "/api/schedules/{id}/end"),
    ("POST", "/api/schedules/{id}/hold"),
    ("POST", "/api/schedules/{id}/move-now"),
    ("POST", "/api/schedules/{id}/pause"),
    ("POST", "/api/schedules/{id}/resume"),
    ("POST", "/api/schedules/{id}/skip"),
    ("POST", "/api/schedules/{id}/time"),
    ("POST", "/api/sql"),
    ("POST", "/api/strips/preview"),
    ("PUT", "/api/welfare/training/{herd_id}"),
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

/// The lowest role a list gives this request; `None` when no list has it.
pub fn classified(method: &Method, path: &str) -> Option<Role> {
    // HEAD reads what GET reads; a preflight reads nothing.
    let method = if *method == Method::HEAD { &Method::GET } else { method };
    if path == "/mcp" || path.starts_with("/mcp/") || is_open(method, path) || *method == Method::OPTIONS {
        // MCP lists and calls tools by the caller's role.
        return Some(Role::Viewer);
    }
    if listed(OWNER, method, path) {
        return Some(Role::Owner);
    }
    if listed(ANY_ROLE, method, path) {
        return Some(Role::Viewer);
    }
    if listed(MANAGER_READS, method, path) {
        return Some(Role::Manager);
    }
    if *method == Method::GET {
        return listed(VIEWER, method, path).then_some(Role::Viewer);
    }
    if listed(HAND, method, path) {
        return Some(Role::Hand);
    }
    listed(MANAGER, method, path).then_some(Role::Manager)
}

/// The lowest role that may make this request: the owner's alone when no list has it.
pub fn required(method: &Method, path: &str) -> Role {
    classified(method, path).unwrap_or(Role::Owner)
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
pub struct RateLimit {
    limit: usize,
    seen: Mutex<HashMap<IpAddr, VecDeque<Instant>>>,
}

impl Default for RateLimit {
    /// [`OPEN_PER_MINUTE`] a minute.
    fn default() -> Self {
        Self::new(OPEN_PER_MINUTE)
    }
}

impl RateLimit {
    /// `limit` a minute per peer.
    pub fn new(limit: usize) -> Self {
        Self { limit, seen: Mutex::default() }
    }

    fn window(&self) -> std::sync::MutexGuard<'_, HashMap<IpAddr, VecDeque<Instant>>> {
        let now = Instant::now();
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, q| {
            while q.front().is_some_and(|t| now.duration_since(*t) >= WINDOW) {
                q.pop_front();
            }
            !q.is_empty()
        });
        seen
    }

    /// Count one request from `peer`; false once it made `limit` in the last minute.
    pub fn allow(&self, peer: IpAddr) -> bool {
        let mut seen = self.window();
        let q = seen.entry(peer).or_default();
        if q.len() >= self.limit {
            return false;
        }
        q.push_back(Instant::now());
        true
    }

    /// `peer` made `limit` in the last minute (nothing counted).
    pub fn full(&self, peer: IpAddr) -> bool {
        self.window().get(&peer).is_some_and(|q| q.len() >= self.limit)
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
            ("POST", "/api/settings/app-token/reveal", Owner),
            ("POST", "/api/settings/app-token/rotate", Owner),
            ("POST", "/api/session", Viewer),
            ("DELETE", "/api/session", Viewer),
            ("GET", "/api/rules", Viewer),
            ("POST", "/api/rules", Manager),
            ("POST", "/api/rules/rul_1/convert", Manager),
            ("GET", "/api/gate/log", Viewer),
            ("PUT", "/api/gate/settings", Manager),
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
            ("PUT", "/api/server/collar-address", Owner),
            ("POST", "/api/server/collar-address/check", Owner),
            ("PUT", "/api/server/retention", Owner),
            ("GET", "/api/server/retention", Viewer),
            ("POST", "/api/collars/col_1/park", Hand),
        ];
        for (method, path, need) in cases {
            assert_eq!(required(&m(method), path), need, "{method} {path}");
        }
    }

    #[test]
    fn someone_elses_alert_prefs_are_the_owners_own_are_a_hands() {
        use Role::*;
        assert_eq!(required(&m("PUT"), "/api/alerts/prefs/usr_01J0000000000000000000000"), Owner);
        assert_eq!(required(&m("PUT"), "/api/alerts/prefs/me"), Hand);
        assert_eq!(required(&m("GET"), "/api/alerts/prefs/me"), Viewer);
        assert_eq!(required(&m("GET"), "/api/alerts/prefs"), Manager);
    }

    /// Every `/api` route the crates register, from their sources: `.route("…", get(…).post(…))`.
    fn api_routes() -> Vec<(String, String, String)> {
        fn walk(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
            for e in std::fs::read_dir(dir).unwrap().flatten() {
                let p = e.path();
                if p.is_dir() && p.file_name().is_some_and(|n| n != "tests" && n != "target") {
                    walk(&p, out);
                } else if p.extension().is_some_and(|x| x == "rs") {
                    out.push(p);
                }
            }
        }
        let crates = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap().to_path_buf();
        let mut files = Vec::new();
        walk(&crates, &mut files);
        let mut out = Vec::new();
        for f in files {
            let s = std::fs::read_to_string(&f).unwrap();
            let mut rest = s.as_str();
            while let Some(i) = rest.find(".route(") {
                let body = &rest[i + 7..];
                let mut depth = 1;
                let end = body.char_indices().find_map(|(k, c)| {
                    depth += match c {
                        '(' => 1,
                        ')' => -1,
                        _ => 0,
                    };
                    (depth == 0).then_some(k)
                });
                let call = &body[..end.unwrap_or(body.len())];
                rest = &body[end.unwrap_or(body.len())..];
                let Some(path) = call.trim_start().strip_prefix('"').and_then(|p| p.split_once('"')).map(|(p, _)| p) else { continue };
                if !path.starts_with("/api") {
                    continue;
                }
                let bytes = call.as_bytes();
                for m in ["get", "post", "put", "patch", "delete"] {
                    let hit = call.match_indices(&format!("{m}(")).any(|(k, _)| k == 0 || !(bytes[k - 1].is_ascii_alphanumeric() || bytes[k - 1] == b'_'));
                    if hit {
                        out.push((m.to_ascii_uppercase(), path.to_owned(), f.display().to_string()));
                    }
                }
            }
        }
        out
    }

    #[test]
    fn every_api_route_is_on_a_list() {
        let routes = api_routes();
        assert!(routes.len() > 150, "found {} routes", routes.len());
        let missing: Vec<String> = routes.iter().filter(|(m, p, _)| classified(&self::m(m), p).is_none()).map(|(m, p, f)| format!("{m} {p} ({f})")).collect();
        assert!(missing.is_empty(), "not on any list in policy.rs (so the owner's alone):\n{}", missing.join("\n"));
        // Unlisted: the owner's.
        assert_eq!(required(&m("GET"), "/api/nothing-listed"), Role::Owner);
        assert_eq!(required(&m("POST"), "/api/nothing-listed"), Role::Owner);
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
