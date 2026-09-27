//! Margins for a herd boundary sent without them (field-ready §2.12).
//!
//! The firmware defaults, warn 5 m and hysteresis 1 m; while the herd's
//! training mode is on (setting `welfare.training`, kept by op-analytics'
//! welfare routes), its training `warn_m` instead, so animals learning the
//! boundary hear the warning further inside it.

use std::collections::HashMap;

use op_core::Ctx;
use op_geo::GeofenceConfig;
use serde::Deserialize;

/// Setting holding each herd's training mode: herd id → `{enabled, warn_m, trained_after}`.
pub const TRAINING_KEY: &str = "welfare.training";

/// The part of a herd's training mode that sets margins.
#[derive(Debug, Deserialize)]
struct Training {
    #[serde(default)]
    enabled: bool,
    #[serde(default)]
    warn_m: Option<f64>,
}

/// `(warn_m, hysteresis_m)` for a boundary of `herd_id` whose send left them out.
pub async fn default_margins(ctx: &Ctx, herd_id: &str) -> anyhow::Result<(f64, f64)> {
    let d = GeofenceConfig::default();
    let herds: Option<HashMap<String, serde_json::Value>> = ctx.store().get_setting(TRAINING_KEY).await.unwrap_or_default();
    let training = herds.and_then(|h| h.get(herd_id).and_then(|v| serde_json::from_value::<Training>(v.clone()).ok()));
    match training {
        Some(Training { enabled: true, warn_m: Some(w) }) if w.is_finite() && w > 0.0 => Ok((w, d.hysteresis_m)),
        _ => Ok((d.warn_m, d.hysteresis_m)),
    }
}
