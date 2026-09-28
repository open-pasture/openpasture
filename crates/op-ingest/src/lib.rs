//! Collar HTTP endpoints, per-collar auth, boundary dispatch and acks. Owns the "Collars and boundaries (op-ingest)" section of `docs/API.md`.
//!
//! Collars pull: a boundary is "sent" by storing it as the herd's next
//! version. Collars fetch it from `/collar/v1/boundary?have=N`, signed with
//! the server key, and acknowledge it on `/collar/v1/ack`.

mod boundary;
mod collars;
mod db;
mod device;
pub mod escapes;
pub mod moves;
pub mod planner;
// @HUB
// @HUB-UI
// @E-lib
// @E-srv
pub mod config;
pub mod margins;
pub mod shape;
pub mod slots;
// @J
// @A-engine
// @A-notify
// @D
// @K-animals
pub use collars::{NewLinked, collar_endpoint, collar_key_matches, create_linked_collars, link_collar, park_collar, rekey_collar, unpark_collar};
// @K-files
// @I
// @B
// @G
// @P
// @Q
// @C
// @F
pub mod prepare;
// @S
pub mod schedule;
// @A3
// @H
// @L
// @M
// @Z

use op_core::Ctx;

pub use boundary::{SendOpts, boundary_status, move_herd_on_record, send_boundary, supersede_proposals};
pub use collars::latest_positions;
pub use escapes::{current_escapes, stop_escape};
pub use moves::{Started, current_move, start_move, stop_move};
// @E-srv
pub use boundary::announce_activations;
pub use db::{HerdBoundaries, herd_boundaries, sql};
pub use margins::default_margins;
pub use shape::{CollarCaps, Prepared, command_for, fence_geometry, herd_limits, prepare};

/// Routes from docs/API.md, "Collars and boundaries (op-ingest)".
pub fn router() -> axum::Router<Ctx> {
    let mut app = axum::Router::new();
    for part in [
        collars::router(),
        boundary::router(),
        moves::router(),
        escapes::router(),
        device::router(),
        // @HUB
        // @HUB-UI
        // @E-lib
        // @E-srv
        config::router(),
        slots::router(),
        // @J
        // @A-engine
        // @A-notify
        // @D
        // @K-animals
        collars::lifecycle_router(),
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

/// Background tasks. Returns once they are spawned: the watcher that
/// announces staged boundaries when they take effect, the move driver and
/// the escape driver.
pub async fn start(ctx: Ctx) -> anyhow::Result<()> {
    register_tools(&ctx);
    boundary::spawn_activation_watcher(ctx.clone());
    moves::spawn_driver(ctx.clone());
    escapes::spawn_driver(ctx.clone());
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
    schedule::spawn_driver(ctx.clone());
    // @A3
    // @H
    // @L
    op_core::store::spawn_checkpointer(&ctx, op_core::store::CHECKPOINT_EVERY);
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
