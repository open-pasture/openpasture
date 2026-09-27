//! Rule configs (`alerts.rules`) and the engine policy (`alerts.policy`).

use std::collections::BTreeMap;

use op_core::{ApiError, ApiResult, Ctx};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::rules::{RuleConfig, RuleDescriptor, rules};

pub const RULES_KEY: &str = "alerts.rules";
pub const POLICY_KEY: &str = "alerts.policy";

/// How alerts open, group, notify and clear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    /// Unacked critical alerts are sent again this often …
    pub renotify_every_min: u32,
    /// … at most this many times.
    pub renotify_max: u32,
    /// Unacked critical alerts go to the next role up this often.
    pub escalate_after_min: u32,
    /// Warnings wait this long and go out as one text per kind and herd.
    pub group_window_s: u32,
    /// This many of one kind in one herd at once are one alert.
    pub rollup_min: u32,
    /// More than this share of a herd's collars silent is `herd_silent`.
    pub herd_silent_share: f64,
    /// A key must be gone this long before its alert resolves.
    pub clear_after_min: u32,
    /// `silent` and `herd_silent` wait this long after a server start.
    pub start_grace_min: u32,
    /// Critical alerts wait this long before the first send, so a breakout
    /// that opens alerts over a few seconds goes out as one rollup.
    pub critical_window_s: u32,
    /// The farm's quiet hours (farm time, HH:MM), for people without their own.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quiet_start: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quiet_end: Option<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Self {
            renotify_every_min: 30,
            renotify_max: 3,
            escalate_after_min: 15,
            group_window_s: 60,
            rollup_min: 4,
            herd_silent_share: 0.5,
            clear_after_min: 2,
            start_grace_min: 20,
            critical_window_s: 10,
            quiet_start: None,
            quiet_end: None,
        }
    }
}

pub async fn policy(ctx: &Ctx) -> anyhow::Result<Policy> {
    Ok(ctx.store().get_setting::<Policy>(POLICY_KEY).await.ok().flatten().unwrap_or_default())
}

fn stored_rules(v: Option<Value>) -> BTreeMap<String, Value> {
    v.and_then(|v| serde_json::from_value(v).ok()).unwrap_or_default()
}

/// A rule's stored config over its default (fields the farm never set keep
/// the default).
fn effective(d: &RuleDescriptor, stored: Option<&Value>) -> RuleConfig {
    let Some(s) = stored else { return d.default.clone() };
    let mut full = serde_json::to_value(&d.default).unwrap_or(Value::Null);
    op_core::patch::merge(&mut full, s);
    serde_json::from_value(full).unwrap_or_else(|_| d.default.clone())
}

/// Every rule's config on this farm, by kind.
pub async fn rule_configs(ctx: &Ctx) -> anyhow::Result<BTreeMap<String, RuleConfig>> {
    let stored = stored_rules(ctx.store().get_setting_json(RULES_KEY).await?);
    Ok(rules()
        .iter()
        .map(|r| {
            let d = r.descriptor();
            (d.kind.to_owned(), effective(&d, stored.get(d.kind)))
        })
        .collect())
}

/// One rule's config (its default for an unknown kind is an error).
pub async fn rule_config(ctx: &Ctx, kind: &str) -> anyhow::Result<RuleConfig> {
    rule_configs(ctx).await?.remove(kind).ok_or_else(|| anyhow::anyhow!("no rule {kind}"))
}

/// A change to rules and policy: rules by kind (only the fields given), and
/// policy fields. `null` clears a rule's `after_min`/`threshold` back to the
/// default, and a quiet hour.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    #[serde(default)]
    pub rules: BTreeMap<String, Value>,
    #[serde(default)]
    pub policy: Option<Value>,
}

pub async fn update(ctx: &Ctx, change: Change) -> ApiResult<()> {
    let all = rules();
    let mut stored = stored_rules(ctx.store().get_setting_json(RULES_KEY).await?);
    for (kind, patch) in &change.rules {
        let d = all.iter().map(|r| r.descriptor()).find(|d| d.kind == kind).ok_or_else(|| ApiError::bad_request(format!("There is no rule {kind}.")))?;
        if !patch.is_object() {
            return Err(ApiError::bad_request(format!("Rule {kind} takes an object.")));
        }
        let mut next = stored.get(kind).cloned().unwrap_or_else(|| Value::Object(Default::default()));
        op_core::patch::merge(&mut next, patch);
        // Validate the result as a whole.
        let mut full = serde_json::to_value(&d.default).map_err(anyhow::Error::from)?;
        op_core::patch::merge(&mut full, &next);
        let cfg: RuleConfig = serde_json::from_value(full).map_err(|e| ApiError::bad_request(format!("Rule {kind}: {e}")))?;
        check_rule(&d, &cfg)?;
        stored.insert(kind.clone(), next);
    }
    let mut pol = None;
    if let Some(p) = &change.policy {
        if !p.is_object() {
            return Err(ApiError::bad_request("policy takes an object."));
        }
        let mut full = serde_json::to_value(policy(ctx).await?).map_err(anyhow::Error::from)?;
        op_core::patch::merge(&mut full, p);
        let next: Policy = serde_json::from_value(full).map_err(|e| ApiError::bad_request(format!("policy: {e}")))?;
        check_policy(&next)?;
        pol = Some(next);
    }
    if !change.rules.is_empty() {
        ctx.store().set_setting(RULES_KEY, &stored).await?;
    }
    if let Some(p) = pol {
        ctx.store().set_setting(POLICY_KEY, &p).await?;
    }
    Ok(())
}

fn check_rule(d: &RuleDescriptor, c: &RuleConfig) -> ApiResult<()> {
    let bad = |m: &str| Err(ApiError::bad_request(format!("{}: {m}", d.kind)));
    if let Some(a) = c.after_min
        && !(1..=10_080).contains(&a)
    {
        return bad("after_min must be 1 to 10080.");
    }
    if let Some(t) = c.threshold {
        let (lo, hi) = match d.unit {
            "%" => (1.0, 99.0),
            "m" => (0.5, 1000.0),
            _ => (0.0, 1e6),
        };
        let (lo, hi) = if d.kind == "drop_off" { (0.5, 100.0) } else { (lo, hi) };
        if !t.is_finite() || t < lo || t > hi {
            return bad(&format!("threshold must be {lo} to {hi}."));
        }
    }
    Ok(())
}

pub(crate) fn valid_hhmm(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 5 && b[2] == b':' && s[..2].parse::<u8>().is_ok_and(|h| h < 24) && s[3..].parse::<u8>().is_ok_and(|m| m < 60)
}

fn check_policy(p: &Policy) -> ApiResult<()> {
    let range = |name: &str, v: u32, lo: u32, hi: u32| {
        if (lo..=hi).contains(&v) { Ok(()) } else { Err(ApiError::bad_request(format!("{name} must be {lo} to {hi}."))) }
    };
    range("renotify_every_min", p.renotify_every_min, 1, 1440)?;
    range("renotify_max", p.renotify_max, 0, 20)?;
    range("escalate_after_min", p.escalate_after_min, 1, 1440)?;
    range("group_window_s", p.group_window_s, 0, 3600)?;
    range("rollup_min", p.rollup_min, 2, 1000)?;
    range("clear_after_min", p.clear_after_min, 0, 120)?;
    range("start_grace_min", p.start_grace_min, 0, 240)?;
    range("critical_window_s", p.critical_window_s, 0, 300)?;
    if !(p.herd_silent_share.is_finite() && (0.05..1.0).contains(&p.herd_silent_share)) {
        return Err(ApiError::bad_request("herd_silent_share must be at least 0.05 and below 1."));
    }
    match (&p.quiet_start, &p.quiet_end) {
        (None, None) => Ok(()),
        (Some(a), Some(b)) if valid_hhmm(a) && valid_hhmm(b) => Ok(()),
        _ => Err(ApiError::bad_request("Quiet hours need a start and an end, as HH:MM.")),
    }
}
