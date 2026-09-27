//! Alert rules. Each rule reads database state and says what is wrong right
//! now as [`Candidate`]s; the engine (`crate::engine`) turns candidates into
//! alerts: it opens new keys, updates changed ones, rolls many of one kind in
//! a herd into one alert and resolves keys that have been gone for a while.
//! A rule never writes.
//!
//! Every collar rule skips parked collars and collars whose animal is removed
//! ([`fleet`]).

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use op_core::{Ctx, LonLat, Severity};
use serde::{Deserialize, Serialize};
use serde_json::Value;

mod collar_rules;
mod fleet;
mod herd_rules;
mod telemetry;
// @S
mod schedule;
// @H
mod fit_check;

pub use collar_rules::{BoundaryNotApplied, Escaped, HerdSilent, LowBattery, Outside, Silent};
pub use fleet::{Fleet, HerdInfo, Unit, fleet, silence};
pub use herd_rules::{DecisionWaiting, MoveStalled, Stragglers};
pub use telemetry::{DropOff, GpsDegraded};
// @S
pub use schedule::ScheduleNotStored;
// @H
pub use fit_check::FitCheckDue;

/// How a rule behaves on this farm (`alerts.rules`, per kind). `after_min`
/// and `threshold` mean what the rule's sentence says.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleConfig {
    pub enabled: bool,
    pub severity: Severity,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after_min: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub threshold: Option<f64>,
    /// Texts, emails and the webhook go out (info never pushes).
    pub notify: bool,
}

/// What a rule is, for the engine and for Settings.
#[derive(Debug, Clone)]
pub struct RuleDescriptor {
    pub kind: &'static str,
    /// Settings sentence; `{n}` is the rule's number with its unit
    /// (`after_min` when `unit` is "min", else `threshold`): "Collar silent
    /// for {n}" reads "Collar silent for [20] min".
    pub sentence: &'static str,
    /// "min", "%", "m" (shown in the farm's length unit), or "" when the
    /// sentence has no number.
    pub unit: &'static str,
    pub default: RuleConfig,
    /// Seconds between evaluations on the engine's 10 s tick.
    pub cadence_s: u32,
    /// Bus event types (`Event`'s `type`, e.g. "escape") that evaluate the
    /// rule early. Never "fix" or "collar".
    pub wake_on: &'static [&'static str],
}

/// Something wrong right now. `key` is stable while it lasts:
/// `<kind>:<subject id>`.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub key: String,
    /// `("collar", id)`, `("herd", id)`, `("decision", id)`, `("move", id)`.
    /// Collar candidates roll up per herd.
    pub subject: (String, String),
    pub herd_id: Option<String>,
    pub title: String,
    pub body: Option<String>,
    pub at: Option<LonLat>,
    pub targets: Vec<(String, String)>,
    /// Facts the texts and the brief read: `label`, `since`, `paddock`, …
    pub data: Value,
    /// Raises the configured severity for this candidate (never lowers it).
    pub severity: Option<Severity>,
}

#[async_trait]
pub trait Rule: Send + Sync {
    fn descriptor(&self) -> RuleDescriptor;
    async fn evaluate(&self, ctx: &Ctx, cfg: &RuleConfig, now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>>;
}

/// Every rule, in Settings order.
pub fn rules() -> Vec<Box<dyn Rule>> {
    vec![
        // @A-engine
        Box::new(Escaped),
        Box::new(Outside),
        Box::new(Silent),
        Box::new(HerdSilent),
        Box::new(LowBattery),
        Box::new(BoundaryNotApplied),
        Box::new(DecisionWaiting),
        Box::new(MoveStalled),
        Box::new(Stragglers),
        Box::new(DropOff),
        Box::new(GpsDegraded),
        // @S
        Box::new(ScheduleNotStored),
        // @H
        Box::new(FitCheckDue),
    ]
}

/// A config's `after_min`, else the default's, else `fallback`.
pub(crate) fn after_min(cfg: &RuleConfig, d: &RuleConfig, fallback: u32) -> u32 {
    cfg.after_min.or(d.after_min).unwrap_or(fallback)
}

/// A config's `threshold`, else the default's, else `fallback`.
pub(crate) fn threshold(cfg: &RuleConfig, d: &RuleConfig, fallback: f64) -> f64 {
    cfg.threshold.or(d.threshold).unwrap_or(fallback)
}

pub(crate) fn config(enabled: bool, severity: Severity, after_min: Option<u32>, threshold: Option<f64>, notify: bool) -> RuleConfig {
    RuleConfig { enabled, severity, after_min, threshold, notify }
}

/// A collar's own targets: the collar, and its animal when it has one.
pub(crate) fn collar_targets(u: &Unit) -> Vec<(String, String)> {
    let mut t = vec![("collar".to_owned(), u.collar.id.clone())];
    if let Some(a) = &u.collar.animal_id {
        t.push(("animal".to_owned(), a.clone()));
    }
    t
}

pub(crate) fn ts(t: &DateTime<Utc>) -> String {
    op_core::time::to_db(t)
}
