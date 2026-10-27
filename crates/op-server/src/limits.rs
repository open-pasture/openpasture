//! What the outside world can make this server do.
//!
//! - **Public routes** (`/collar/v1`, `/hooks`, `/v1`: collars, Twilio, other
//!   servers) answer before their own checks are paid for, too often: a peer
//!   that got [`FAILURES_PER_MINUTE`] refusals (401, 403, 404) in a minute
//!   gets 429 for the rest of it; one collar key may post
//!   [`REPORTS_PER_MINUTE`] reports a minute (more is 429 with
//!   `Retry-After`); at most [`PUBLIC_IN_FLIGHT`] public requests run at once
//!   (503 with `Retry-After` past that); each has [`PUBLIC_TIMEOUT`] and a
//!   body of at most [`PUBLIC_BODY_BYTES`]. Collars behind one carrier
//!   address or one LoRa anchor share a peer, so only refusals count per peer.
//! - **Connections**: at most [`MAX_CONNECTIONS`] at once, and one that has
//!   sent and received nothing for [`IDLE`] is closed ([`capped`]).
//! - **Headers** on every response: no framing (`X-Frame-Options: DENY`), no
//!   type sniffing, no referrer. The UI's own `Content-Security-Policy` comes
//!   with its pages (`ui`).

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use op_core::ApiError;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::policy::{self, RateLimit};

/// Prefixes of the routes anyone on the internet may call.
pub const PUBLIC: [&str; 3] = ["/collar/v1/", "/hooks/", "/v1/"];
/// Refusals one peer may get on public routes in a minute.
pub const FAILURES_PER_MINUTE: usize = 30;
/// Reports one collar key may post in a minute (a sweep's fast config is one every few seconds).
pub const REPORTS_PER_MINUTE: usize = 30;
/// Public requests running at once.
pub const PUBLIC_IN_FLIGHT: usize = 64;
/// How long one public request may take.
pub const PUBLIC_TIMEOUT: Duration = Duration::from_secs(30);
/// Largest public request body by its `Content-Length` (a report of 10,000
/// fixes fits); a body without one meets axum's own 2 MB limit.
pub const PUBLIC_BODY_BYTES: u64 = 2 * 1024 * 1024;
/// Connections open at once.
pub const MAX_CONNECTIONS: usize = 1024;
/// A connection with no bytes either way for this long is closed (the live
/// feed pings every 15 s).
pub const IDLE: Duration = Duration::from_secs(120);

fn public(path: &str) -> bool {
    PUBLIC.iter().any(|p| path.starts_with(p))
}

/// Counts per key in a sliding minute.
#[derive(Default)]
struct PerKey {
    seen: Mutex<HashMap<String, VecDeque<Instant>>>,
}

impl PerKey {
    fn allow(&self, key: &str, limit: usize) -> bool {
        let now = Instant::now();
        let mut seen = self.seen.lock().unwrap_or_else(|e| e.into_inner());
        seen.retain(|_, q| {
            while q.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
                q.pop_front();
            }
            !q.is_empty()
        });
        let q = seen.entry(key.to_owned()).or_default();
        if q.len() >= limit {
            return false;
        }
        q.push_back(now);
        true
    }
}

/// The public routes' limits, shared by every request.
pub struct Public {
    failed: RateLimit,
    reports: PerKey,
    in_flight: Arc<Semaphore>,
}

impl Default for Public {
    fn default() -> Self {
        Self { failed: RateLimit::new(FAILURES_PER_MINUTE), reports: PerKey::default(), in_flight: Arc::new(Semaphore::new(PUBLIC_IN_FLIGHT)) }
    }
}

fn refuse(status: StatusCode, msg: &str, retry_after: Option<u64>) -> Response {
    let mut r = ApiError::new(status, msg).into_response();
    if let Some(s) = retry_after {
        r.headers_mut().insert(header::RETRY_AFTER, HeaderValue::from(s));
    }
    r
}

/// The public routes' limits (see the module docs). Everything else passes untouched.
pub async fn limit_public(State(p): State<Arc<Public>>, req: Request, next: Next) -> Response {
    let path = req.uri().path().to_owned();
    if !public(&path) {
        return next.run(req).await;
    }
    let peer = policy::peer_key(req.extensions().get::<ConnectInfo<SocketAddr>>().map(|c| c.0), req.headers());
    if p.failed.full(peer) {
        return refuse(StatusCode::TOO_MANY_REQUESTS, "Too many refused requests from here. Wait a minute.", Some(60));
    }
    let length = req.headers().get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if length.is_some_and(|n| n > PUBLIC_BODY_BYTES) {
        return refuse(StatusCode::PAYLOAD_TOO_LARGE, "The request is too large.", None);
    }
    // Counted per collar key as the collar endpoints read it (scheme and spaces don't make a new one).
    if path == "/collar/v1/report"
        && let Some(key) = req.headers().get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()).map(collar_key)
        && !p.reports.allow(&op_core::keys::hash_key(key), REPORTS_PER_MINUTE)
    {
        return refuse(StatusCode::TOO_MANY_REQUESTS, "Too many reports from this collar. Report less often.", Some(10));
    }
    let Ok(_permit) = p.in_flight.clone().try_acquire_owned() else {
        return refuse(StatusCode::SERVICE_UNAVAILABLE, "Busy. Try again shortly.", Some(5));
    };
    // A body without a length (chunked) is cut at the same cap as it streams in: nothing
    // downstream (the wire log, the JSON reader) ever buffers more.
    let req = req.map(|b| axum::body::Body::new(http_body_util::Limited::new(b, PUBLIC_BODY_BYTES as usize)));
    let res = match tokio::time::timeout(PUBLIC_TIMEOUT, next.run(req)).await {
        Ok(r) => r,
        Err(_) => return refuse(StatusCode::SERVICE_UNAVAILABLE, "That took too long. Try again.", Some(5)),
    };
    if matches!(res.status(), StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND) {
        p.failed.allow(peer);
    }
    res
}

/// The key in an `Authorization` header as the collar endpoints read it:
/// `Bearer `/`bearer ` off, spaces trimmed.
fn collar_key(header: &str) -> &str {
    let h = header.trim();
    let rest = if h.len() > 7 && h[..7].eq_ignore_ascii_case("bearer ") { &h[7..] } else { h };
    rest.trim()
}

/// No framing, no type sniffing, no referrer, on every response.
pub async fn headers(req: Request, next: Next) -> Response {
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    h.entry(header::X_CONTENT_TYPE_OPTIONS).or_insert(HeaderValue::from_static("nosniff"));
    res
}

// ---- connections ----------------------------------------------------------------------------

/// The listener, at most [`MAX_CONNECTIONS`] connections at once, idle ones
/// closed after [`IDLE`]. Wrap it with `tap_io` for axum's `ConnectInfo`.
pub fn capped(listener: TcpListener) -> Capped {
    Capped { listener, open: Arc::new(Semaphore::new(MAX_CONNECTIONS)) }
}

pub struct Capped {
    listener: TcpListener,
    open: Arc<Semaphore>,
}

impl axum::serve::Listener for Capped {
    type Io = Counted;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        // Past the cap, wait for a connection to close before taking another.
        let permit = self.open.clone().acquire_owned().await.expect("the connection semaphore is never closed");
        let (io, addr) = axum::serve::Listener::accept(&mut self.listener).await;
        (Counted { io, _permit: permit, idle: Box::pin(tokio::time::sleep(IDLE)) }, addr)
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
    }
}

/// One connection: holds its place under the cap, and closes when idle.
pub struct Counted {
    io: TcpStream,
    _permit: OwnedSemaphorePermit,
    idle: Pin<Box<tokio::time::Sleep>>,
}

impl Counted {
    fn active(&mut self) {
        self.idle.as_mut().reset(tokio::time::Instant::now() + IDLE);
    }

    fn timed_out(&mut self, cx: &mut Context<'_>) -> bool {
        self.idle.as_mut().poll(cx).is_ready()
    }
}

impl AsyncRead for Counted {
    fn poll_read(self: Pin<&mut Self>, cx: &mut Context<'_>, buf: &mut ReadBuf<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        match Pin::new(&mut this.io).poll_read(cx, buf) {
            Poll::Ready(r) => {
                if buf.filled().len() > before {
                    this.active();
                }
                Poll::Ready(r)
            }
            Poll::Pending if this.timed_out(cx) => Poll::Ready(Err(io::Error::new(io::ErrorKind::TimedOut, "idle connection"))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for Counted {
    fn poll_write(self: Pin<&mut Self>, cx: &mut Context<'_>, data: &[u8]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_write(cx, data);
        if matches!(r, Poll::Ready(Ok(n)) if n > 0) {
            this.active();
        }
        r
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().io).poll_shutdown(cx)
    }

    fn poll_write_vectored(self: Pin<&mut Self>, cx: &mut Context<'_>, bufs: &[io::IoSlice<'_>]) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        let r = Pin::new(&mut this.io).poll_write_vectored(cx, bufs);
        if matches!(r, Poll::Ready(Ok(n)) if n > 0) {
            this.active();
        }
        r
    }

    fn is_write_vectored(&self) -> bool {
        self.io.is_write_vectored()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_public_prefixes_are_limited() {
        assert!(public("/collar/v1/report") && public("/hooks/twilio/sms") && public("/v1/decide"));
        assert!(!public("/api/state") && !public("/mcp") && !public("/") && !public("/v1"));
    }

    #[test]
    fn per_key_counts_a_minute() {
        let k = PerKey::default();
        for _ in 0..3 {
            assert!(k.allow("a", 3));
        }
        assert!(!k.allow("a", 3));
        assert!(k.allow("b", 3));
    }
}
