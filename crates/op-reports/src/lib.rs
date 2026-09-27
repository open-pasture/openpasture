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
mod api;
mod csv;
mod feed_log;
mod history;
mod lease;
mod leases_api;
mod nrcs_528;
mod organic;
mod paddock_record;
mod report;
mod settings;
pub use report::{Column, Report, ReportDoc, ReportParams, ReportSection};
// @B
// @G
// @P
// @Q
// @C
// @F
// @S
mod schedule_cols;
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
        api::router(),
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
        api::get_report_tool(),
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

/// Every report, in the order `GET /api/reports` lists them.
pub fn reports() -> Vec<Box<dyn Report>> {
    vec![
        // @I
        Box::new(paddock_record::PaddockRecord),
        Box::new(nrcs_528::Nrcs528),
        Box::new(organic::OrganicSeason),
        Box::new(lease::LeaseHeadDays),
        // @H
        // @S
    ]
}

/// The report with this id.
pub fn report(id: &str) -> Option<Box<dyn Report>> {
    reports().into_iter().find(|r| r.id() == id)
}
