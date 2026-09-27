//! Rules about single collars: escaped, outside, silent (and the herd going
//! silent), low battery, a boundary not applied.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use op_core::time::{from_db, opt_from_db};
use op_core::{Ctx, Severity};
use serde_json::json;
use sqlx::Row;

use super::fleet::{Fleet, Unit, fleet, is_silent, silence};
use super::{Candidate, Rule, RuleConfig, RuleDescriptor, after_min, collar_targets, config, threshold, ts};
use crate::engine;

/// "214 outside P3", or "outside the boundary" when the herd has no paddock.
pub(crate) fn outside_title(label: &str, paddock: Option<&str>) -> String {
    match paddock {
        Some(p) => format!("{label} outside {p}"),
        None => format!("{label} outside the boundary"),
    }
}

fn collar_candidate(kind: &str, u: &Unit, title: String, mut data: serde_json::Value, herd: &super::HerdInfo) -> Candidate {
    data["label"] = json!(u.label);
    if let Some(p) = &herd.paddock {
        data["paddock"] = json!(p);
    }
    data["herd"] = json!(herd.name);
    Candidate {
        key: format!("{kind}:{}", u.collar.id),
        subject: ("collar".into(), u.collar.id.clone()),
        herd_id: Some(u.collar.herd_id.clone()),
        title,
        body: None,
        at: u.collar.last_fix.as_ref().map(|f| f.point),
        targets: collar_targets(u),
        data,
        severity: None,
    }
}

/// Silence limits by the `silent` rule's configured minutes.
async fn silence_limits(ctx: &Ctx, f: &Fleet, now: DateTime<Utc>) -> anyhow::Result<HashMap<String, Duration>> {
    let cfg = engine::config::rule_config(ctx, "silent").await?;
    let minutes = after_min(&cfg, &Silent.descriptor().default, 20);
    silence(ctx, &f.units, minutes, now).await
}

/// Collars on an open escape, with when it started.
async fn open_escapes(ctx: &Ctx) -> anyhow::Result<HashMap<String, DateTime<Utc>>> {
    let rows = sqlx::query("SELECT collar_id, started_at FROM escapes WHERE status = 'returning'").fetch_all(ctx.db()).await?;
    rows.iter().map(|r| Ok((r.try_get::<String, _>(0)?, from_db(&r.try_get::<String, _>(1)?)?))).collect()
}

// ---- escaped -------------------------------------------------------------------------

/// An animal out on its own boundary (an open escape).
pub struct Escaped;

#[async_trait]
impl Rule for Escaped {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "escaped",
            sentence: "An animal gets out",
            unit: "",
            default: config(true, Severity::Critical, None, None, true),
            cadence_s: 10,
            wake_on: &["escape"],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, _cfg: &RuleConfig, _now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let open = open_escapes(ctx).await?;
        if open.is_empty() {
            return Ok(vec![]);
        }
        let f = fleet(ctx).await?;
        Ok(f.units
            .iter()
            .filter_map(|u| {
                let started = open.get(&u.collar.id)?;
                let herd = f.herd(&u.collar.herd_id);
                let since = u.collar.outside_since.unwrap_or(*started).min(*started);
                Some(collar_candidate("escaped", u, outside_title(&u.label, herd.paddock.as_deref()), json!({ "since": ts(&since) }), &herd))
            })
            .collect())
    }
}

// ---- outside --------------------------------------------------------------------------

/// Outside the herd's boundary for a while with no escape bringing it back
/// (none could start, or the farmer let it go on this trip out).
pub struct Outside;

#[async_trait]
impl Rule for Outside {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "outside",
            sentence: "Outside the boundary for {n}",
            unit: "min",
            default: config(true, Severity::Warning, Some(5), None, true),
            cadence_s: 10,
            wake_on: &["escape"],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, cfg: &RuleConfig, now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let after = Duration::minutes(after_min(cfg, &self.descriptor().default, 5) as i64);
        let f = fleet(ctx).await?;
        let out: Vec<&Unit> = f.units.iter().filter(|u| u.collar.outside_since.is_some_and(|s| now - s >= after)).collect();
        if out.is_empty() {
            return Ok(vec![]);
        }
        let escaping = open_escapes(ctx).await?;
        let let_go: HashMap<String, DateTime<Utc>> =
            sqlx::query("SELECT collar_id, MAX(ended_at) FROM escapes WHERE status = 'stopped' AND ended_at IS NOT NULL GROUP BY collar_id")
                .fetch_all(ctx.db())
                .await?
                .iter()
                .filter_map(|r| Some((r.try_get::<String, _>(0).ok()?, opt_from_db(r.try_get(1).ok()?).ok()??)))
                .collect();
        // After a restart every collar looks silent until it reports again: no severity from that.
        let limits = if engine::in_grace(ctx, now).await? { HashMap::new() } else { silence_limits(ctx, &f, now).await? };
        Ok(out
            .into_iter()
            .filter(|u| !escaping.contains_key(&u.collar.id))
            .filter_map(|u| {
                let since = u.collar.outside_since?;
                if let_go.get(&u.collar.id).is_some_and(|t| *t >= since) {
                    return None;
                }
                let herd = f.herd(&u.collar.herd_id);
                let mut c = collar_candidate("outside", u, outside_title(&u.label, herd.paddock.as_deref()), json!({ "since": ts(&since) }), &herd);
                if is_silent(u, &limits, now) {
                    c.severity = Some(Severity::Critical);
                    c.data["silent"] = json!(true);
                }
                Some(c)
            })
            .collect())
    }
}

// ---- silent ---------------------------------------------------------------------------

/// No report for `max(after_min, 3 × the collar's usual interval)`. The engine
/// holds this rule for `start_grace_min` after a server start.
pub struct Silent;

#[async_trait]
impl Rule for Silent {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "silent",
            sentence: "Collar silent for {n}",
            unit: "min",
            default: config(true, Severity::Warning, Some(20), None, true),
            cadence_s: 10,
            wake_on: &[],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, cfg: &RuleConfig, now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let f = fleet(ctx).await?;
        let limits = silence(ctx, &f.units, after_min(cfg, &self.descriptor().default, 20), now).await?;
        Ok(f.units
            .iter()
            .filter(|u| is_silent(u, &limits, now))
            .filter_map(|u| {
                let seen = u.collar.last_seen?;
                let herd = f.herd(&u.collar.herd_id);
                Some(collar_candidate("silent", u, format!("{} silent", u.label), json!({ "since": ts(&seen) }), &herd))
            })
            .collect())
    }
}

/// More than `herd_silent_share` of a herd's collars silent (and at least
/// two): one alert for the herd that takes in its `silent` alerts. Held after
/// a server start like `silent`.
pub struct HerdSilent;

#[async_trait]
impl Rule for HerdSilent {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "herd_silent",
            sentence: "Most of a herd's collars silent",
            unit: "",
            default: config(true, Severity::Critical, None, None, true),
            cadence_s: 10,
            wake_on: &[],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, _cfg: &RuleConfig, now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let share = engine::config::policy(ctx).await?.herd_silent_share;
        let f = fleet(ctx).await?;
        let limits = silence_limits(ctx, &f, now).await?;
        // Per herd: collars that have reported, and those silent now.
        let mut by_herd: HashMap<&str, (Vec<&Unit>, Vec<&Unit>)> = HashMap::new();
        for u in f.units.iter().filter(|u| u.collar.last_seen.is_some()) {
            let e = by_herd.entry(u.collar.herd_id.as_str()).or_default();
            e.0.push(u);
            if is_silent(u, &limits, now) {
                e.1.push(u);
            }
        }
        let mut out = Vec::new();
        for (herd_id, (all, silent)) in by_herd {
            if silent.len() < 2 || (silent.len() as f64) <= share * all.len() as f64 {
                continue;
            }
            let herd = f.herd(herd_id);
            let since = silent.iter().filter_map(|u| u.collar.last_seen).max().unwrap_or(now);
            let points: Vec<_> = silent.iter().filter_map(|u| u.collar.last_fix.as_ref().map(|x| x.point)).collect();
            out.push(Candidate {
                key: format!("herd_silent:herd:{herd_id}"),
                subject: ("herd".into(), herd_id.to_owned()),
                herd_id: Some(herd_id.to_owned()),
                title: format!("{} of {} collars silent", silent.len(), all.len()),
                body: None,
                at: crate::engine::centroid(&points),
                targets: silent.iter().flat_map(|u| collar_targets(u)).collect(),
                data: json!({
                    "herd": herd.name, "count": silent.len(), "total": all.len(), "since": ts(&since),
                    "members": silent.iter().map(|u| json!({ "label": u.label })).collect::<Vec<_>>(),
                }),
                severity: None,
            });
        }
        out.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(out)
    }
}

// ---- low battery ----------------------------------------------------------------------

pub struct LowBattery;

#[async_trait]
impl Rule for LowBattery {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "low_battery",
            sentence: "Battery below {n}",
            unit: "%",
            default: config(true, Severity::Warning, None, Some(20.0), false),
            cadence_s: 60,
            wake_on: &[],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, cfg: &RuleConfig, _now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let below = threshold(cfg, &self.descriptor().default, 20.0) / 100.0;
        let f = fleet(ctx).await?;
        Ok(f.units
            .iter()
            .filter_map(|u| {
                let b = u.collar.battery.filter(|b| *b < below)?;
                let pct = (b * 100.0).round().max(0.0) as i64;
                let herd = f.herd(&u.collar.herd_id);
                Some(collar_candidate("low_battery", u, format!("{} battery {pct}%", u.label), json!({ "pct": pct }), &herd))
            })
            .collect())
    }
}

// ---- boundary not applied -------------------------------------------------------------

/// The herd's boundary has been in effect a while and a collar still holds an
/// older one (or rejected it), while not escaped and not silent.
pub struct BoundaryNotApplied;

#[async_trait]
impl Rule for BoundaryNotApplied {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "boundary_not_applied",
            sentence: "Boundary not applied after {n}",
            unit: "min",
            default: config(true, Severity::Warning, Some(10), None, true),
            cadence_s: 10,
            wake_on: &["ack", "boundary"],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, cfg: &RuleConfig, now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let after = Duration::minutes(after_min(cfg, &self.descriptor().default, 10) as i64);
        let f = fleet(ctx).await?;
        let herds: HashSet<&str> = f.units.iter().map(|u| u.collar.herd_id.as_str()).collect();
        // Each herd's boundary in effect, if it has been for `after`.
        let mut settled: HashMap<&str, (u32, DateTime<Utc>)> = HashMap::new();
        for h in herds {
            let row = sqlx::query(
                "SELECT version, created_at, effective_at FROM boundaries
                 WHERE herd_id = ? AND collar_id IS NULL AND (effective_at IS NULL OR effective_at <= ?)
                 ORDER BY version DESC LIMIT 1",
            )
            .bind(h)
            .bind(ts(&now))
            .fetch_optional(ctx.db())
            .await?;
            let Some(r) = row else { continue };
            let created = from_db(&r.try_get::<String, _>("created_at")?)?;
            let since = opt_from_db(r.try_get("effective_at")?)?.map_or(created, |e| e.max(created));
            if now - since >= after {
                settled.insert(h, (r.try_get::<i64, _>("version")? as u32, since));
            }
        }
        if settled.is_empty() {
            return Ok(vec![]);
        }
        let held: HashMap<String, (Option<String>, u32, String)> = sqlx::query("SELECT collar_id, herd_id, version, status FROM collar_boundary_state")
            .fetch_all(ctx.db())
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, (r.try_get(1)?, r.try_get::<i64, _>(2)? as u32, r.try_get(3)?))))
            .collect::<anyhow::Result<_>>()?;
        let escaping = open_escapes(ctx).await?;
        let limits = silence_limits(ctx, &f, now).await?;
        Ok(f.units
            .iter()
            .filter(|u| u.collar.last_seen.is_some() && !escaping.contains_key(&u.collar.id) && !is_silent(u, &limits, now))
            .filter_map(|u| {
                let (version, since) = *settled.get(u.collar.herd_id.as_str())?;
                let behind = match held.get(&u.collar.id) {
                    None => true,
                    Some((herd, v, status)) => herd.as_deref() != Some(u.collar.herd_id.as_str()) || *v < version || (*v == version && status == "rejected"),
                };
                if !behind {
                    return None;
                }
                let herd = f.herd(&u.collar.herd_id);
                let held_v = held.get(&u.collar.id).map(|h| h.1);
                Some(collar_candidate(
                    "boundary_not_applied",
                    u,
                    format!("{} boundary not applied", u.label),
                    json!({ "since": ts(&since), "version": version, "held": held_v }),
                    &herd,
                ))
            })
            .collect())
    }
}
