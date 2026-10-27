//! Units from the factory to the farm (`docs/api/fleet.md`): the registry of
//! units by serial, factory test plans and runs judged by the server,
//! provisioning and the unit certificate, claim into a herd, labels, batch
//! yield and throughput, station tokens, firmware images and rollouts, the
//! herd network (anchors, nodes, link quality) and the per-collar console.
//!
//! This crate depends on op-core alone, so the collar ingest can call its
//! hooks ([`network::record_uplink`], [`firmware::offer_for`], …) without a
//! cycle.

pub mod batch;
pub mod console;
pub mod firmware;
pub mod keys;
pub mod label;
pub mod network;
pub mod plans;
pub mod provision;
pub mod record;
pub mod runs;
pub mod stations;
pub mod units;

use op_core::Ctx;

/// Every `/api/fleet/...` route of this crate. `GET /api/fleet` and the fit
/// checks under it are op-analytics' fleet care.
pub fn router() -> axum::Router<Ctx> {
    let mut app = axum::Router::new();
    for part in [
        units::router(),
        plans::router(),
        runs::router(),
        provision::router(),
        stations::router(),
        batch::router(),
        label::router(),
        firmware::router(),
        network::router(),
        console::router(),
    ] {
        app = app.merge(part);
    }
    app
}

/// Shipped test plans, then the background jobs: units turning `in_service`
/// on their collar's first report, and the firmware rollout tick.
pub async fn start(ctx: Ctx) -> anyhow::Result<()> {
    plans::ensure_shipped(&ctx).await?;
    units::spawn_in_service_watch(&ctx);
    firmware::rollout::spawn(&ctx);
    Ok(())
}

/// Who did something, for the unit history: the person's name, the station's
/// name, or how the request came (`local`, `app_token`).
pub(crate) fn who(identity: &op_core::Identity) -> Option<String> {
    if let Some(name) = &identity.name {
        return Some(name.clone());
    }
    if let Some(id) = &identity.user_id {
        return Some(id.clone());
    }
    serde_json::to_value(identity.via).ok().and_then(|v| v.as_str().map(str::to_owned))
}
