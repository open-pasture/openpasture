//! Rules that read raw fixes, always through bounded windows on
//! `fixes(collar_id, t)` / `fixes(herd_id, t)`: a collar that stopped moving
//! (dropped off), and weak GPS.

use std::collections::{HashMap, HashSet};

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use op_core::{Ctx, Severity};
use op_geo::Projection;
use serde_json::json;
use sqlx::Row;

use super::fleet::{Unit, fleet, median};
use super::{Candidate, Rule, RuleConfig, RuleDescriptor, after_min, collar_targets, config, threshold, ts};

/// One sample of a collar's position per this many minutes of the window.
const SAMPLE_MIN: i64 = 5;
/// Share of samples that must have a fix for the window to count.
const COVERAGE: f64 = 0.8;

// ---- drop off -------------------------------------------------------------------------

/// Every fix (sampled every 5 minutes) within `threshold` m of their median
/// for `after_min`: the collar is lying on the ground, or the animal is down.
pub struct DropOff;

#[async_trait]
impl Rule for DropOff {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "drop_off",
            sentence: "Not moving for {n}",
            unit: "min",
            default: config(true, Severity::Warning, Some(240), Some(4.0), true),
            cadence_s: 300,
            wake_on: &[],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, cfg: &RuleConfig, now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let d = self.descriptor().default;
        let window = Duration::minutes(after_min(cfg, &d, 240).max(SAMPLE_MIN as u32) as i64);
        let radius = threshold(cfg, &d, 4.0);
        let f = fleet(ctx).await?;
        let start = now - window;
        let steps = window.num_minutes() / SAMPLE_MIN;
        let step_ms = SAMPLE_MIN * 60_000;
        let mut out = Vec::new();
        // Only collars with a fresh fix whose track reaches back to the window's start.
        let fresh = f.units.iter().filter(|u| u.collar.last_fix.as_ref().is_some_and(|x| now - x.at <= Duration::minutes(15)));
        for u in fresh {
            let rows = sqlx::query(
                "WITH RECURSIVE s(k, t) AS (SELECT 0, ?1 UNION ALL SELECT k + 1, t + ?2 FROM s WHERE k + 1 < ?3)
                 SELECT s.k, f.lon, f.lat FROM s JOIN fixes f ON f.id = (
                     SELECT id FROM fixes WHERE collar_id = ?4 AND t >= s.t AND t < s.t + ?2 ORDER BY t LIMIT 1)",
            )
            .bind(start.timestamp_millis())
            .bind(step_ms)
            .bind(steps)
            .bind(&u.collar.id)
            .fetch_all(ctx.db())
            .await?;
            let ks: HashSet<i64> = rows.iter().filter_map(|r| r.try_get::<i64, _>(0).ok()).collect();
            if !ks.contains(&0) || !ks.contains(&(steps - 1)) || (ks.len() as f64) < COVERAGE * steps as f64 {
                continue;
            }
            let pts: Vec<[f64; 2]> = rows.iter().filter_map(|r| Some([r.try_get::<f64, _>(1).ok()?, r.try_get::<f64, _>(2).ok()?])).collect();
            if !still(&pts, radius) {
                continue;
            }
            let herd = f.herd(&u.collar.herd_id);
            let mut data = json!({ "label": u.label, "herd": herd.name, "since": ts(&start) });
            if let Some(p) = &herd.paddock {
                data["paddock"] = json!(p);
            }
            out.push(Candidate {
                key: format!("drop_off:{}", u.collar.id),
                subject: ("collar".into(), u.collar.id.clone()),
                herd_id: Some(u.collar.herd_id.clone()),
                title: format!("{} not moving", u.label),
                body: None,
                at: u.collar.last_fix.as_ref().map(|x| x.point),
                targets: collar_targets(u),
                data,
                severity: None,
            });
        }
        Ok(out)
    }
}

/// Whether every point is within `radius` m of the points' median.
pub(crate) fn still(pts: &[[f64; 2]], radius: f64) -> bool {
    let (Some(lon), Some(lat)) = (median(&mut pts.iter().map(|p| p[0]).collect::<Vec<_>>()), median(&mut pts.iter().map(|p| p[1]).collect::<Vec<_>>())) else {
        return false;
    };
    let proj = Projection::new([lon, lat]);
    pts.iter().all(|p| {
        let [x, y] = proj.forward(*p);
        x.hypot(y) <= radius
    })
}

// ---- GPS degraded ---------------------------------------------------------------------

/// Minutes of fixes GPS quality is judged on.
const GPS_WINDOW_MIN: i64 = 10;

/// Median accuracy over the last 10 minutes worse than `threshold` m, or no
/// fix for 10 minutes while reports keep arriving.
pub struct GpsDegraded;

#[async_trait]
impl Rule for GpsDegraded {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "gps_degraded",
            sentence: "GPS accuracy worse than {n}",
            unit: "m",
            default: config(true, Severity::Info, None, Some(10.0), false),
            cadence_s: 60,
            wake_on: &[],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, cfg: &RuleConfig, now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let limit = threshold(cfg, &self.descriptor().default, 10.0);
        let window = Duration::minutes(GPS_WINDOW_MIN);
        let f = fleet(ctx).await?;
        let herds: HashSet<&str> = f.units.iter().map(|u| u.collar.herd_id.as_str()).collect();
        let mut acc: HashMap<String, Vec<f64>> = HashMap::new();
        for h in herds {
            let rows = sqlx::query("SELECT collar_id, accuracy_m FROM fixes WHERE herd_id = ? AND t >= ?")
                .bind(h)
                .bind((now - window).timestamp_millis())
                .fetch_all(ctx.db())
                .await?;
            for r in rows {
                acc.entry(r.try_get(0)?).or_default().push(r.try_get(1)?);
            }
        }
        let mut out = Vec::new();
        for u in &f.units {
            let reporting = u.collar.last_seen.is_some_and(|s| now - s <= window);
            if !reporting {
                continue;
            }
            let no_fix = u.collar.last_fix.as_ref().is_none_or(|x| now - x.at > window);
            let accuracy = acc.get_mut(&u.collar.id).filter(|v| v.len() >= 3).and_then(|v| median(v));
            let herd = f.herd(&u.collar.herd_id);
            let mut data = json!({ "label": u.label, "herd": herd.name });
            if no_fix {
                if let Some(x) = &u.collar.last_fix {
                    data["no_fix_since"] = json!(ts(&x.at));
                }
            } else if let Some(a) = accuracy.filter(|a| *a > limit) {
                data["accuracy_m"] = json!((a * 10.0).round() / 10.0);
            } else {
                continue;
            }
            out.push(candidate(u, data));
        }
        Ok(out)
    }
}

fn candidate(u: &Unit, data: serde_json::Value) -> Candidate {
    Candidate {
        key: format!("gps_degraded:{}", u.collar.id),
        subject: ("collar".into(), u.collar.id.clone()),
        herd_id: Some(u.collar.herd_id.clone()),
        title: format!("{} GPS weak", u.label),
        body: None,
        at: u.collar.last_fix.as_ref().map(|x| x.point),
        targets: collar_targets(u),
        data,
        severity: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn still_means_every_point_near_the_median() {
        let proj = Projection::new([-93.6, 42.0]);
        let near: Vec<[f64; 2]> = (0..20).map(|i| proj.offset((i % 5) as f64 * 0.5, (i % 3) as f64 * 0.5)).collect();
        assert!(still(&near, 4.0));
        let mut moved = near.clone();
        moved.push(proj.offset(9.0, 0.0));
        assert!(!still(&moved, 4.0));
        assert!(!still(&[], 4.0));
    }
}
