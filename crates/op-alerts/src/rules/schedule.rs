//! `schedule_not_stored` (S): the next scheduled move of an active strip
//! schedule is due within `after_min` (120) minutes and isn't stored on
//! every collar expected to hold it. Expected: the herd's collars on duty
//! that report (not silent), aren't out on an escape, and have had five
//! minutes since the move was staged to fetch it. One alert per schedule,
//! targeting the collars that lack it (the map rings them).

use std::collections::HashSet;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use op_core::time::from_db;
use op_core::{Ctx, Polygon, Severity};
use serde_json::json;
use sqlx::Row;

use super::fleet::{fleet, is_silent, silence};
use super::{Candidate, Rule, RuleConfig, RuleDescriptor, after_min, config, ts};

/// A staged move gets this long to reach the collars before it counts as missing.
const FETCH_GRACE: Duration = Duration::minutes(5);
/// Labels listed in the alert's data.
const MAX_LABELS: usize = 10;

pub struct ScheduleNotStored;

#[async_trait]
impl Rule for ScheduleNotStored {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "schedule_not_stored",
            sentence: "Next strip not on every collar {n} before it opens",
            unit: "min",
            default: config(true, Severity::Warning, Some(120), None, true),
            cadence_s: 60,
            wake_on: &["ack", "boundary", "schedule"],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, cfg: &RuleConfig, now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let window = Duration::minutes(after_min(cfg, &self.descriptor().default, 120) as i64);
        let schedules = sqlx::query("SELECT id, herd_id FROM schedules WHERE status = 'active'").fetch_all(ctx.db()).await?;
        if schedules.is_empty() {
            return Ok(vec![]);
        }
        let f = fleet(ctx).await?;
        let cfg_silent = crate::engine::config::rule_config(ctx, "silent").await?;
        let limits = silence(ctx, &f.units, after_min(&cfg_silent, &super::Silent.descriptor().default, 20), now).await?;
        let escaping: HashSet<String> =
            sqlx::query_scalar("SELECT collar_id FROM escapes WHERE status = 'returning'").fetch_all(ctx.db()).await?.into_iter().collect();
        let mut out = Vec::new();
        for r in &schedules {
            let (sid, herd_id): (String, String) = (r.try_get(0)?, r.try_get(1)?);
            let next = sqlx::query(
                "SELECT strip, step, at, geometry, boundary_version, updated_at FROM schedule_moves
                 WHERE schedule_id = ? AND state IN ('planned', 'staged') AND at > ? ORDER BY at LIMIT 1",
            )
            .bind(&sid)
            .bind(ts(&now))
            .fetch_optional(ctx.db())
            .await?;
            let Some(m) = next else { continue };
            let at = from_db(&m.try_get::<String, _>("at")?)?;
            let changed = from_db(&m.try_get::<String, _>("updated_at")?)?;
            if at - now > window || now - changed < FETCH_GRACE {
                continue;
            }
            let version: Option<i64> = m.try_get("boundary_version")?;
            let expected: Vec<&super::Unit> = f
                .units
                .iter()
                .filter(|u| u.collar.herd_id == herd_id && u.collar.last_seen.is_some() && !escaping.contains(&u.collar.id) && !is_silent(u, &limits, now))
                .collect();
            if expected.is_empty() {
                continue;
            }
            let holding: HashSet<String> = match version {
                Some(v) => sqlx::query(
                    "SELECT s.collar_id FROM collar_slots s LEFT JOIN boundaries b ON b.version = s.version
                     WHERE s.status IN ('received', 'applied') AND (s.version = ?1 OR b.copy_of = ?1)",
                )
                .bind(v)
                .fetch_all(ctx.db())
                .await?
                .iter()
                .map(|r| r.try_get(0))
                .collect::<Result<_, _>>()?,
                None => HashSet::new(),
            };
            let missing: Vec<&super::Unit> = expected.iter().copied().filter(|u| !holding.contains(&u.collar.id)).collect();
            if missing.is_empty() {
                continue;
            }
            let strip = m.try_get::<i64, _>("strip")? + 1;
            let step: i64 = m.try_get("step")?;
            let herd = f.herd(&herd_id);
            let title = if missing.len() == 1 {
                format!("{} missing strip {strip}", missing[0].label)
            } else {
                format!("{} collars missing strip {strip}", missing.len())
            };
            let geometry: Polygon = serde_json::from_str(&m.try_get::<String, _>("geometry")?)?;
            let mut targets = vec![("schedule".to_owned(), sid.clone())];
            targets.extend(missing.iter().map(|u| ("collar".to_owned(), u.collar.id.clone())));
            out.push(Candidate {
                key: format!("schedule_not_stored:{sid}"),
                subject: ("schedule".into(), sid.clone()),
                herd_id: Some(herd_id.clone()),
                title,
                body: None,
                at: geometry.centroid(),
                targets,
                data: json!({
                    "herd": herd.name,
                    "strip": strip,
                    "step": step,
                    "opens_at": ts(&at),
                    "since": ts(&changed.max(at - window)),
                    "missing": missing.len(),
                    "total": expected.len(),
                    "labels": missing.iter().take(MAX_LABELS).map(|u| u.label.clone()).collect::<Vec<_>>(),
                    "version": version,
                }),
                severity: None,
            });
        }
        Ok(out)
    }
}
