//! `/api/live`: every [`op_core::Event`] as a JSON text message.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use op_core::Ctx;
use tokio::sync::broadcast::error::RecvError;

pub async fn handler(ws: WebSocketUpgrade, State(ctx): State<Ctx>) -> Response {
    ws.on_upgrade(move |socket| stream(socket, ctx))
}

async fn stream(mut socket: WebSocket, ctx: Ctx) {
    let mut rx = ctx.subscribe();
    let shutdown = ctx.on_shutdown();
    tokio::pin!(shutdown);
    loop {
        tokio::select! {
            _ = &mut shutdown => {
                let _ = socket.send(Message::Close(None)).await;
                break;
            }
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    let Ok(text) = serde_json::to_string(&ev) else { continue };
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Lagged(n)) => {
                    // The client missed events: tell it to refetch.
                    tracing::warn!("live client lagged, dropped {n} events; sending resync");
                    let Ok(text) = serde_json::to_string(&op_core::Event::Resync) else { continue };
                    if socket.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Err(RecvError::Closed) => break,
            },
            msg = socket.recv() => match msg {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            },
        }
    }
}
