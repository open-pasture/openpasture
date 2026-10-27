//! `/api/live`: what the socket's identity may see ([`op_core::Event::min_role`]),
//! as JSON text messages. Fixes, acks, cues and telemetry-only collar changes
//! arrive coalesced per herd (`positions`, `ack_batch`, `cue_batch`), gathered
//! for the whole farm every 500 ms and sent as one message (a `batch` when
//! there are several, see [`crate::coalesce`]); every other event as it happens.
//!
//! Every 15 s the server sends `{"type":"ping"}` (so a client can tell a
//! quiet feed from a dead one) and a WebSocket ping; a socket that sends
//! nothing back, not even the pong, for 45 s is closed.
//!
//! A browser can't set headers on a WebSocket, so the token goes in the
//! subprotocol list instead of the URL: `["openpasture",
//! "openpasture.token.<token>"]`. The server answers with `openpasture`.

use std::time::Duration;

use axum::Extension;
use axum::extract::State;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::http::{HeaderMap, header};
use axum::response::Response;
use futures::SinkExt;
use op_core::{Ctx, Identity};
use tokio::sync::broadcast::{self, error::RecvError};

use crate::coalesce::{self, Out};

const RESYNC: &str = r#"{"type":"resync"}"#;
const PING: &str = r#"{"type":"ping"}"#;
/// How often a socket checks the sign-in it was opened with still holds.
pub const SESSION_CHECK: Duration = Duration::from_secs(5);
/// How often the server pings, and how long a silent socket lives.
pub const HEARTBEAT: Duration = Duration::from_secs(15);
pub const SILENT_FOR: Duration = Duration::from_secs(45);
/// The subprotocol the server answers with, and the prefix of the one that carries the token.
pub const PROTOCOL: &str = "openpasture";
const TOKEN_PROTOCOL: &str = "openpasture.token.";

/// The token a WebSocket upgrade carries in `Sec-WebSocket-Protocol`.
pub fn protocol_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::SEC_WEBSOCKET_PROTOCOL)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .find_map(|p| p.trim().strip_prefix(TOKEN_PROTOCOL).filter(|t| !t.is_empty()).map(str::to_owned))
}

pub async fn handler(ws: WebSocketUpgrade, State(ctx): State<Ctx>, identity: Identity, credential: Option<Extension<crate::auth::Credential>>) -> Response {
    // Subscribed before the upgrade answers, so a client that just connected
    // misses nothing published after that.
    let rx = coalesce::hub(&ctx).subscribe();
    let token = credential.map(|Extension(crate::auth::Credential(t))| t);
    ws.protocols([PROTOCOL]).on_upgrade(move |socket| stream(socket, ctx, identity, token, rx))
}

/// Sockets listening on this context's live feed.
pub fn subscribers(ctx: &Ctx) -> usize {
    coalesce::hub(ctx).subscribers()
}

async fn stream(mut socket: WebSocket, ctx: Ctx, identity: Identity, token: Option<String>, mut rx: broadcast::Receiver<Out>) {
    let shutdown = ctx.on_shutdown();
    tokio::pin!(shutdown);
    let mut check = tokio::time::interval_at(tokio::time::Instant::now() + SESSION_CHECK, SESSION_CHECK);
    let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + HEARTBEAT, HEARTBEAT);
    let mut heard = tokio::time::Instant::now();
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            _ = beat.tick() => {
                if heard.elapsed() >= SILENT_FOR {
                    tracing::debug!("live client silent for {}s; closing", heard.elapsed().as_secs());
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
                if socket.send(Message::Text(Utf8Bytes::from_static(PING))).await.is_err() || socket.send(Message::Ping(Default::default())).await.is_err() {
                    break;
                }
            }
            // Whatever it signed in with, checked again: signed out, revoked, run out,
            // rotated, a disabled person or a changed role closes the socket (a failed check too).
            _ = check.tick(), if token.is_some() => {
                let t = token.as_deref().unwrap_or_default();
                if !crate::auth::still_signed_in(&ctx, t, &identity).await {
                    let _ = socket.send(Message::Close(None)).await;
                    break;
                }
            }
            out = rx.recv() => match out {
                Ok(out) => {
                    if !identity.can(out.role) {
                        continue;
                    }
                    if socket.send(Message::Text(out.text)).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // The client missed messages: tell it to refetch.
                    tracing::warn!("live client lagged, dropped {n} messages; sending resync");
                    if socket.send(Message::Text(Utf8Bytes::from_static(RESYNC))).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) => {
                    // The close reply is queued; write it out so the client sees a clean close.
                    let _ = socket.flush().await;
                    break;
                }
                Some(Err(_)) | None => break,
                // Pongs, and anything else the client sends, say it's still there.
                Some(Ok(_)) => heard = tokio::time::Instant::now(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_token_is_read_from_the_subprotocol_list() {
        let mut h = HeaderMap::new();
        h.insert(header::SEC_WEBSOCKET_PROTOCOL, "openpasture, openpasture.token.abc123".parse().unwrap());
        assert_eq!(protocol_token(&h).as_deref(), Some("abc123"));
        h.insert(header::SEC_WEBSOCKET_PROTOCOL, "openpasture".parse().unwrap());
        assert_eq!(protocol_token(&h), None);
        h.insert(header::SEC_WEBSOCKET_PROTOCOL, "openpasture.token.".parse().unwrap());
        assert_eq!(protocol_token(&h), None);
    }
}
