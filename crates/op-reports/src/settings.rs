//! Report inputs the farm record doesn't hold: who the operator is, the FSA
//! farm number, animal-unit factors, and per-herd weights and mixes
//! (`reports.settings` and `reports.herds`, contract §2.7).
//!
//! `GET/PUT /api/reports/settings` serves both keys as one object; PUT is a
//! JSON merge patch (`null` clears a field or removes a herd's entry).

use std::collections::BTreeMap;

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use op_core::{ApiError, ApiJson, ApiResult, Ctx};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SETTINGS_KEY: &str = "reports.settings";
pub const HERDS_KEY: &str = "reports.herds";

/// Animal units per head for the parts of a cattle herd's mix.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AuFactors {
    pub cow: f64,
    pub bull: f64,
    pub pair: f64,
    pub weaned_calf: f64,
}

impl Default for AuFactors {
    fn default() -> Self {
        Self { cow: 1.0, bull: 1.35, pair: 1.3, weaned_calf: 0.5 }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ReportsSettings {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operator: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fsa_farm: Option<String>,
    pub au: AuFactors,
}

/// What a cattle herd is made of. With `pairs`, each cow has a calf at side,
/// the pair counted at the pair factor, and `calves` is ignored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HerdMix {
    pub cows: u32,
    pub bulls: u32,
    pub calves: u32,
    pub pairs: bool,
}

impl HerdMix {
    /// Head the mix names, a pair as one head.
    fn head(&self) -> u32 {
        self.cows + self.bulls + if self.pairs { 0 } else { self.calves }
    }

    /// The head count the mix's animal units stand at, for a herd of `count`
    /// head: a pair as one head, or, when `count` is nearer that, a pair as
    /// two (its calf registered as an animal of its own). A calf at side is
    /// in its pair's factor either way, so counting calves never adds AU.
    pub fn head_at(&self, count: u32) -> u32 {
        let one = self.head();
        let two = one + self.cows;
        if self.pairs && count.abs_diff(two) < count.abs_diff(one) { two } else { one }
    }

    /// How much of the mix a herd of `count` head is.
    fn share(&self, count: u32) -> f64 {
        count as f64 / self.head_at(count).max(1) as f64
    }

    /// The mix's animal units.
    pub fn animal_units(&self, f: &AuFactors) -> f64 {
        if self.pairs {
            self.cows as f64 * f.pair + self.bulls as f64 * f.bull
        } else {
            self.cows as f64 * f.cow + self.bulls as f64 * f.bull + self.calves as f64 * f.weaned_calf
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct HerdReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mean_weight_kg: Option<f64>,
    /// Daily dry-matter intake, percent of body weight.
    pub intake_pct: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mix: Option<HerdMix>,
}

impl Default for HerdReport {
    fn default() -> Self {
        Self { mean_weight_kg: None, intake_pct: 2.5, mix: None }
    }
}

/// Both keys together, as the API serves them.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Inputs {
    #[serde(flatten)]
    pub settings: ReportsSettings,
    #[serde(default)]
    pub herds: BTreeMap<String, HerdReport>,
}

impl Inputs {
    pub async fn load(ctx: &Ctx) -> anyhow::Result<Self> {
        let settings = ctx.store().get_setting::<ReportsSettings>(SETTINGS_KEY).await?.unwrap_or_default();
        let herds = ctx.store().get_setting::<BTreeMap<String, HerdReport>>(HERDS_KEY).await?.unwrap_or_default();
        Ok(Self { settings, herds })
    }

    pub fn herd(&self, herd_id: &str) -> HerdReport {
        self.herds.get(herd_id).cloned().unwrap_or_default()
    }

    /// A cattle herd's mix, when one with any head is set.
    pub fn mix(&self, herd_id: &str, species: &str) -> Option<HerdMix> {
        self.herds.get(herd_id).and_then(|h| h.mix).filter(|m| species == "cattle" && m.head() > 0)
    }

    /// Animal units of the herd at `count` head: its mix's, by the AU
    /// factors, when one is set ([`HerdMix::share`] of them), else the species
    /// factor of `calc::animal_units` per head.
    pub fn animal_units(&self, herd_id: &str, species: &str, count: u32) -> f64 {
        match self.mix(herd_id, species) {
            Some(m) => m.animal_units(&self.settings.au) * m.share(count),
            None => op_engine::calc::animal_units(species, 1, None) * count as f64,
        }
    }

    /// Cow-calf pairs in the herd at `count` head, when the mix says so.
    pub fn pairs(&self, herd_id: &str, species: &str, count: u32) -> Option<f64> {
        self.mix(herd_id, species).filter(|m| m.pairs).map(|m| m.cows as f64 * m.share(count))
    }

    fn validate(&self) -> Result<(), String> {
        let f = self.settings.au;
        for (name, v) in [("cow", f.cow), ("bull", f.bull), ("pair", f.pair), ("weaned_calf", f.weaned_calf)] {
            if !(v.is_finite() && v > 0.0 && v <= 5.0) {
                return Err(format!("The {} AU factor must be between 0 and 5.", name.replace('_', " ")));
            }
        }
        for text in [&self.settings.operator, &self.settings.fsa_farm].into_iter().flatten() {
            if text.chars().count() > 120 {
                return Err("Keep names under 120 characters.".into());
            }
        }
        for (id, h) in &self.herds {
            if id.trim().is_empty() {
                return Err("Herd ids can't be empty.".into());
            }
            if !(h.intake_pct.is_finite() && h.intake_pct > 0.0 && h.intake_pct <= 10.0) {
                return Err("Intake must be between 0 and 10 % of body weight.".into());
            }
            if let Some(w) = h.mean_weight_kg
                && !(w.is_finite() && w > 0.0 && w <= 3000.0)
            {
                return Err("Mean weight must be between 0 and 3,000 kg.".into());
            }
        }
        Ok(())
    }
}

/// Text fields: trimmed, and gone when empty.
fn tidy(v: Option<String>) -> Option<String> {
    v.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty())
}

async fn get_inputs(State(ctx): State<Ctx>) -> ApiResult<Json<Inputs>> {
    Ok(Json(Inputs::load(&ctx).await?))
}

async fn put_inputs(State(ctx): State<Ctx>, ApiJson(patch): ApiJson<Value>) -> ApiResult<Json<Inputs>> {
    if !patch.is_object() {
        return Err(ApiError::bad_request("Expected a JSON object."));
    }
    // One write transaction: read, merge, write both keys.
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let mut current = Value::Object(Default::default());
    for key in [SETTINGS_KEY, HERDS_KEY] {
        let row: Option<(String,)> = sqlx::query_as("SELECT value FROM settings WHERE key = ?").bind(key).fetch_optional(&mut *tx).await?;
        if let Some((v,)) = row {
            let v: Value = serde_json::from_str(&v).map_err(anyhow::Error::from)?;
            if key == HERDS_KEY {
                current["herds"] = v;
            } else {
                op_core::patch::merge(&mut current, &v);
            }
        }
    }
    op_core::patch::merge(&mut current, &patch);
    let mut inputs: Inputs = serde_json::from_value(current).map_err(|e| ApiError::bad_request(e.to_string()))?;
    inputs.settings.operator = tidy(inputs.settings.operator);
    inputs.settings.fsa_farm = tidy(inputs.settings.fsa_farm);
    inputs.validate().map_err(ApiError::bad_request)?;
    for (key, value) in [(SETTINGS_KEY, serde_json::to_value(&inputs.settings)), (HERDS_KEY, serde_json::to_value(&inputs.herds))] {
        sqlx::query("INSERT INTO settings (key, value) VALUES (?, ?) ON CONFLICT(key) DO UPDATE SET value = excluded.value")
            .bind(key)
            .bind(value.map_err(anyhow::Error::from)?.to_string())
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(Json(inputs))
}

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/reports/settings", get(get_inputs).put(put_inputs))
}
