//! Telemetry rollup to Parquet, DataFusion queries, SQL console, export. Owns
//! the "Analytics (op-analytics)" section of `docs/API.md`.
//!
//! Fixes and cues are written to SQLite by op-ingest. Whole UTC days older
//! than `analytics.hot_days` (default 3) move to
//! `<data_dir>/telemetry/<table>/date=YYYY-MM-DD/part.parquet`; every query
//! here reads both, so where a row lives never shows.

pub mod export;
pub mod metrics;
pub mod range;
pub mod rollup;
pub mod routes;
pub mod schema;
pub mod sql;
pub mod telemetry;
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
pub mod coverage;
pub mod days;
pub mod fleet;
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

use axum::routing::{get, post};
use op_core::Ctx;

/// Routes from docs/API.md, "Analytics (op-analytics)".
pub fn router() -> axum::Router<Ctx> {
    let mut app = axum::Router::new();
    for part in [
        axum::Router::new()
            .route("/api/tracks", get(routes::tracks))
            .route("/api/analytics/health", get(routes::health))
            .route("/api/analytics/behaviour", get(routes::behaviour))
            .route("/api/analytics/heatmap", get(routes::heatmap))
            .route("/api/analytics/pasture", get(routes::pasture))
            .route("/api/sql", post(routes::run_sql))
            .route("/api/export", get(export::export)),
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
        coverage::router(),
        fleet::router(),
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

/// Background task: the hourly Parquet rollup (first run now). Battery
/// history comes from op-ingest's `health` table. Returns once spawned.
pub async fn start(ctx: Ctx) -> anyhow::Result<()> {
    register_tools(&ctx);
    rollup::spawn_rollup(ctx.clone());
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
    days::spawn(ctx.clone());
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
    ctx.tools().register_once(env!("CARGO_PKG_NAME"), tool_specs);
}

fn tool_specs() -> Vec<op_core::tools::ToolSpec> {
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
        coverage::tool(),
        fleet::tool(),
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
