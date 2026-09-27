//! Margins for a herd boundary sent without them (field-ready §2.12).
//!
//! E-srv: the firmware defaults, warn 5 m and hysteresis 1 m. H changes the
//! body to use the herd's training `warn_m` while training mode is on.

use op_core::Ctx;
use op_geo::GeofenceConfig;

/// `(warn_m, hysteresis_m)` for a boundary of `herd_id` whose send left them out.
pub async fn default_margins(_ctx: &Ctx, _herd_id: &str) -> anyhow::Result<(f64, f64)> {
    let d = GeofenceConfig::default();
    Ok((d.warn_m, d.hysteresis_m))
}
