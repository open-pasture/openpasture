//! Training mode per herd (setting `welfare.training`): a wider warning zone
//! while animals learn the boundary, and how many turned-back episodes in a
//! row make an animal trained. op-ingest's `default_margins` reads the same
//! setting, so a boundary sent without margins takes the training `warn_m`
//! while training is on.

use std::collections::BTreeMap;

use op_core::{ApiError, ApiResult, Ctx};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Setting key: herd id → [`Training`]. The same key as op-ingest's margins.
pub const TRAINING_KEY: &str = "welfare.training";

/// Narrowest and widest training warning zone, metres (the boundary tool's range).
pub const WARN_M: (f64, f64) = (1.0, 100.0);
/// Most turned-back episodes in a row a herd can ask for.
pub const MAX_TRAINED_AFTER: u32 = 50;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Training {
    pub enabled: bool,
    /// The warning zone boundaries of this herd get while training is on, metres.
    pub warn_m: f64,
    /// Turned-back episodes in a row, with no crossing, that make an animal trained.
    pub trained_after: u32,
}

impl Default for Training {
    fn default() -> Self {
        Self { enabled: false, warn_m: 10.0, trained_after: 5 }
    }
}

/// What a PUT may change; fields left out keep their value.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrainingPatch {
    pub enabled: Option<bool>,
    pub warn_m: Option<f64>,
    pub trained_after: Option<u32>,
}

/// Every herd's training mode as stored (herds never set are absent).
pub async fn all(ctx: &Ctx) -> anyhow::Result<BTreeMap<String, Training>> {
    let v = ctx.store().get_setting_json(TRAINING_KEY).await?;
    Ok(parse(v))
}

fn parse(v: Option<Value>) -> BTreeMap<String, Training> {
    let Some(Value::Object(m)) = v else { return BTreeMap::new() };
    m.into_iter().filter_map(|(k, v)| serde_json::from_value::<Training>(v).ok().map(|t| (k, t))).collect()
}

/// One herd's training mode (the defaults when never set).
pub async fn of(ctx: &Ctx, herd_id: &str) -> anyhow::Result<Training> {
    Ok(all(ctx).await?.remove(herd_id).unwrap_or_default())
}

/// Change one herd's training mode, in one write transaction so two herds
/// saved at once both keep their change.
pub async fn update(ctx: &Ctx, herd_id: &str, p: &TrainingPatch) -> ApiResult<Training> {
    if ctx.store().get_herd(herd_id).await?.is_none() {
        return Err(ApiError::not_found("No such herd."));
    }
    let fmt = op_core::units::Fmt::of(ctx).await?;
    if let Some(w) = p.warn_m {
        if !w.is_finite() || w < WARN_M.0 || w > WARN_M.1 {
            return Err(ApiError::bad_request(format!("The warning zone must be {} to {}.", fmt.len(WARN_M.0), fmt.len(WARN_M.1))));
        }
    }
    if let Some(n) = p.trained_after {
        if n == 0 || n > MAX_TRAINED_AFTER {
            return Err(ApiError::bad_request(format!("Trained after must be 1 to {MAX_TRAINED_AFTER} episodes.")));
        }
    }
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let raw: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?").bind(TRAINING_KEY).fetch_optional(&mut *tx).await?;
    let mut map = serde_json::Map::new();
    if let Some((s,)) = raw {
        if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(&s) {
            map = m;
        }
    }
    let mut t = map.get(herd_id).and_then(|v| serde_json::from_value::<Training>(v.clone()).ok()).unwrap_or_default();
    if let Some(e) = p.enabled {
        t.enabled = e;
    }
    if let Some(w) = p.warn_m {
        t.warn_m = (w * 100.0).round() / 100.0;
    }
    if let Some(n) = p.trained_after {
        t.trained_after = n;
    }
    map.insert(herd_id.to_owned(), serde_json::to_value(t).map_err(anyhow::Error::from)?);
    sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
        .bind(TRAINING_KEY)
        .bind(Value::Object(map).to_string())
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(t)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stored_herds_read_with_defaults_for_what_is_missing() {
        let m = parse(Some(json!({ "herd_a": { "enabled": true, "warn_m": 12.5 }, "herd_b": "nonsense" })));
        assert_eq!(m.len(), 1);
        assert_eq!(m["herd_a"], Training { enabled: true, warn_m: 12.5, trained_after: 5 });
        assert!(parse(Some(json!([1, 2]))).is_empty());
        assert!(parse(None).is_empty());
    }
}
