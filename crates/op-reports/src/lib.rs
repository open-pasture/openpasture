//! Reports built from the farm record: paddock grazing record, NRCS 528,
//! organic grazing season, lease head-days, welfare record.

// @HUB
// @HUB-UI
// @E-lib
// @E-srv
// @J
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

use op_core::Ctx;
use op_core::tools::ToolSpec;

/// Routes of this crate (`/api/reports*`, `/api/feed-log*`, `/api/leases*`).
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
