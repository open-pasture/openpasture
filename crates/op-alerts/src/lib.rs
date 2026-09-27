//! Alerts and texting: alert rules and their lifecycle, routing, text
//! templates, channels (Twilio SMS and WhatsApp, SMTP, webhook, relay), the
//! sender, the hosted relay, and inbound texts. Messages go through op-core's
//! `messages` outbox, so routing and delivery never call each other.

// @HUB
// @HUB-UI
// @E-lib
// @E-srv
// @J
// @A-engine
pub mod alerts_api;
pub mod engine;
pub mod routing;
pub mod rules;
pub mod text;
// @A-notify
pub mod hosting;
pub mod notify;
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

use op_core::Ctx;
use op_core::tools::ToolSpec;

/// Routes of this crate (`/api/alerts*`, `/api/notify/*`, `/api/messages`, `/api/texting*`, `/v1/notify*`, `/hooks/twilio/*`).
pub fn router() -> axum::Router<Ctx> {
    let mut app = axum::Router::new();
    for part in [
        axum::Router::new(),
        // @HUB
        // @HUB-UI
        // @E-lib
        // @E-srv
        // @J
        // @A-engine
        alerts_api::router(),
        // @A-notify
        notify::api::router(),
        hosting::router(),
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
    ] {
        app = app.merge(part);
    }
    app
}

/// Background tasks. Returns once they are spawned.
pub async fn start(ctx: Ctx) -> anyhow::Result<()> {
    register_tools(&ctx);
    // @HUB
    // @HUB-UI
    // @E-lib
    // @E-srv
    // @J
    // @A-engine
    engine::start(ctx.clone());
    // @A-notify
    notify::sender::spawn(ctx.clone()).await?;
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
    Ok(())
}

/// This crate's MCP tools, in registration order. Idempotent: a second call
/// registers nothing.
pub fn register_tools(ctx: &Ctx) {
    ctx.tools().register_once(env!("CARGO_PKG_NAME"), tools);
}

fn tools() -> Vec<ToolSpec> {
    vec![
        // @HUB
        // @HUB-UI
        // @E-lib
        // @E-srv
        // @J
        // @A-engine
        alerts_api::list_alerts_tool(),
        alerts_api::ack_alert_tool(),
        alerts_api::resolve_alert_tool(),
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
    ]
}
