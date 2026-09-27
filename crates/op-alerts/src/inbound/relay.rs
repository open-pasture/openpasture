//! Texts through the hosted relay: while the relay channel is on, this
//! server long-polls `GET {hosted_url}/v1/notify/inbox?since=<cursor>&wait=25`
//! and takes each text as if it had come in on its own number (channel
//! `relay`, so replies go back through the relay). The cursor is kept, and a
//! text the relay hands over twice is taken once (by the relay's id).
//! Polling is also how the relay knows this server is alive (its dead-man),
//! so it goes on while texting in is off; texts then count only for STOP and
//! START.

use std::time::Duration;

use op_core::Ctx;
use op_core::messages::Inbound;
use op_core::notify_config::configured_channels;

use super::state;
use crate::notify::relay::Relay;

pub const KEY: &str = "relay:inbox";
/// Seconds one long-poll may wait.
pub const WAIT_S: u64 = 25;

/// Whether the relay's inbox is read: whenever the relay channel is on, even
/// with texting in off (then only STOP and START count), because polling is
/// how the relay's dead-man knows this server is up.
pub async fn on(ctx: &Ctx) -> anyhow::Result<bool> {
    Ok(configured_channels(ctx).await?.contains(&"relay"))
}

/// One long-poll (at most `wait_s` s). Returns how many new texts were taken.
pub async fn run_once(ctx: &Ctx, wait_s: u64) -> anyhow::Result<usize> {
    if !on(ctx).await? {
        return Ok(0);
    }
    let Some(relay) = Relay::from_secrets(ctx)? else { return Ok(0) };
    let cursor = state::get(ctx, KEY).await?.and_then(|s| s.cursor);
    let inbox = match relay.inbox(cursor.as_deref(), wait_s).await {
        Ok(i) => i,
        Err(e) => {
            state::failed(ctx, KEY, e.message()).await?;
            anyhow::bail!("{e}");
        }
    };
    let mut taken = 0;
    for m in inbox.messages {
        let inbound = Inbound { channel: "relay".into(), from: m.from, text: m.text, provider_id: Some(m.id), at: m.at };
        if super::receive(ctx, inbound).await?.is_some() {
            taken += 1;
        }
    }
    state::ok(ctx, KEY, Some(&inbox.cursor), None).await?;
    Ok(taken)
}

/// Long-poll while the relay is on; back off 5 s, 30 s, 60 s after failures.
pub fn spawn(ctx: Ctx) {
    tokio::spawn(async move {
        let mut fails = 0u32;
        loop {
            let started = std::time::Instant::now();
            let pause = match on(&ctx).await {
                Ok(true) => match tokio::select! {
                    r = run_once(&ctx, WAIT_S) => r,
                    _ = ctx.on_shutdown() => break,
                } {
                    Ok(_) => {
                        fails = 0;
                        // A relay that answers at once, every time, isn't polled flat out.
                        Duration::from_secs(1).saturating_sub(started.elapsed())
                    }
                    Err(e) => {
                        fails += 1;
                        tracing::warn!("reading the relay's inbox: {e:#}");
                        Duration::from_secs(match fails {
                            1 => 5,
                            2 => 30,
                            _ => 60,
                        })
                    }
                },
                _ => Duration::from_secs(10),
            };
            if ctx.is_shutting_down() {
                break;
            }
            if !pause.is_zero() {
                tokio::select! {
                    _ = ctx.on_shutdown() => break,
                    _ = tokio::time::sleep(pause) => {}
                }
            }
        }
    });
}
