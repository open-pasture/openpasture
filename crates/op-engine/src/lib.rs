//! Decision cycle, grazing signals, autonomy, land reports, knowledge, MCP tools. Owns "Decisions and brains (op-engine, op-brain)" in `docs/API.md` except `/api/brains*`, plus "MCP tools (op-engine)" and `POST /mcp`.

mod api;
pub mod calc;
pub mod context;
pub mod cycle;
pub mod db;
pub mod knowledge;
pub mod land;
pub mod mcp;
mod scheduler;
pub mod signals;
pub mod skills;
// @HUB
pub mod tools;
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
pub mod heights;
pub mod layers;
// @G
// @P
// @Q
pub mod brief;
// @C
pub mod layouts;
pub mod strips;
// @F
pub mod presend;
// @S
pub mod schedules;
// @A3
// @H
// @L
// @M
// @Z

pub use api::{land_view, signals_view};
pub use tools::register_tools;

use op_core::Ctx;

/// Routes from docs/API.md, "Decisions and brains (op-engine, op-brain)", and `/mcp`.
pub fn router() -> axum::Router<Ctx> {
    let mut app = axum::Router::new();
    for part in [
        api::router(),
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
        heights::router(),
        layers::router(),
        // @G
        // @P
        // @Q
        brief::router(),
        // @C
        strips::router(),
        layouts::router(),
        // @F
        presend::router(),
        // @S
        schedules::router(),
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

/// Background tasks: the daily decision, timer applications, outcome
/// evaluation. Builds the knowledge index if it is missing or stale.
pub async fn start(ctx: Ctx) -> anyhow::Result<()> {
    register_tools(&ctx);
    cycle::fail_interrupted(&ctx).await?;
    if let Err(e) = knowledge::ensure_index(&ctx).await {
        tracing::warn!("knowledge index: {e:#}");
    }
    scheduler::spawn(ctx.clone());
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
    schedules::register_brief(&ctx);
    // @A3
    // @H
    // @L
    // @M
    // @Z
    Ok(())
}
