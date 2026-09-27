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

use op_core::Ctx;

pub use boundary::{SendOpts, boundary_status, move_herd_on_record, send_boundary, supersede_proposals};
pub use collars::latest_positions;
pub use escapes::{current_escapes, stop_escape};
pub use moves::{Started, current_move, start_move, stop_move};

/// Routes from docs/API.md, "Collars and boundaries (op-ingest)".
pub fn router() -> axum::Router<Ctx> {
    axum::Router::new().merge(collars::router()).merge(boundary::router()).merge(moves::router()).merge(escapes::router()).merge(device::router())
}

/// Background tasks. Returns once they are spawned: the watcher that
/// announces staged boundaries when they take effect, the move driver and
/// the escape driver.
pub async fn start(ctx: Ctx) -> anyhow::Result<()> {
    boundary::spawn_activation_watcher(ctx.clone());
    moves::spawn_driver(ctx.clone());
    escapes::spawn_driver(ctx);
    Ok(())
}
