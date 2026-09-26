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

pub use api::{land_view, signals_view};

use op_core::Ctx;

/// Routes from docs/API.md, "Decisions and brains (op-engine, op-brain)", and `/mcp`.
pub fn router() -> axum::Router<Ctx> {
    api::router()
}

/// Background tasks: the daily decision, timer applications, outcome
/// evaluation. Builds the knowledge index if it is missing or stale.
pub async fn start(ctx: Ctx) -> anyhow::Result<()> {
    cycle::fail_interrupted(&ctx).await?;
    if let Err(e) = knowledge::ensure_index(&ctx).await {
        tracing::warn!("knowledge index: {e:#}");
    }
    scheduler::spawn(ctx);
    Ok(())
}
