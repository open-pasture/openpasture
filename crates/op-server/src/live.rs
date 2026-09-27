//! `/api/live`: what the socket's identity may see ([`op_core::Event::min_role`]),
//! as JSON text messages. Fixes, acks, cues and telemetry-only collar changes
//! arrive coalesced per herd (`positions`, `ack_batch`, `cue_batch`), gathered
//! for the whole farm every 500 ms and sent as one message (a `batch` when
//! there are several, see [`crate::coalesce`]); every other event as it happens.

use std::time::Duration;

use axum::Extension;
use axum::extract::State;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures::SinkExt;
use op_core::people::SessionToken;
use op_core::{Ctx, Identity};
use tokio::sync::broadcast::{self, error::RecvError};

use crate::coalesce::{self, Out};

const RESYNC: &str = r#"{"type":"resync"}"#;
/// How often a socket opened with a person's token checks it still holds.
pub const SESSION_CHECK: Duration = Duration::from_secs(5);

pub async fn handler(ws: WebSocketUpgrade, State(ctx): State<Ctx>, identity: Identity, token: Option<Extension<SessionToken>>) -> Response {
    // Subscribed before the upgrade answers, so a client that just connected
    // misses nothing published after that.
    let rx = coalesce::hub(&ctx).subscribe();
    let token = token.map(|Extension(SessionToken(id))| id);
    ws.on_upgrade(move |socket| stream(socket, ctx, identity, token, rx))
}

/// Sockets listening on this context's live feed.
pub fn subscribers(ctx: &Ctx) -> usize {
    coalesce::hub(ctx).subscribers()
}

async fn stream(mut socket: WebSocket, ctx: Ctx, identity: Identity, token: Option<String>, mut rx: broadcast::Receiver<Out>) {
    let shutdown = ctx.on_shutdown();
    tokio::pin!(shutdown);
    let mut check = tokio::time::interval_at(tokio::time::Instant::now() + SESSION_CHECK, SESSION_CHECK);
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            // A revoked token, a disabled person or a changed role closes the socket.
            _ = check.tick(), if token.is_some() => {
                let id = token.as_deref().unwrap_or_default();
                if !op_core::live::session_holds(ctx.store(), id, identity.role).await.unwrap_or(true) {
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
                _ => {}
            },
        }
    }
}
