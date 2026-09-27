//! `fit_check_due` (H): a collar on an animal whose fit hasn't been checked
//! for `fleet.fit_check_days` (G's setting, default 30): since its last
//! check in `collar_fit_checks`, else since the collar was added. Info, texts
//! off: the morning brief counts them ("Fit check due: 5").

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use op_core::time::from_db;
use op_core::{Ctx, Severity};
use serde::Deserialize;
use serde_json::json;
use sqlx::Row;

use super::fleet::fleet;
use super::{Candidate, Rule, RuleConfig, RuleDescriptor, collar_targets, config, ts};

/// G's fleet settings (`fleet`), the part this rule reads.
#[derive(Debug, Deserialize)]
#[serde(default)]
struct FleetSettings {
    fit_check_days: u32,
}

impl Default for FleetSettings {
    fn default() -> Self {
        Self { fit_check_days: 30 }
    }
}

pub struct FitCheckDue;

#[async_trait]
impl Rule for FitCheckDue {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "fit_check_due",
            sentence: "Collar fit check due",
            unit: "",
            default: config(true, Severity::Info, None, None, false),
            cadence_s: 60,
            wake_on: &[],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, _cfg: &RuleConfig, now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let days = ctx.store().get_setting::<FleetSettings>("fleet").await.ok().flatten().unwrap_or_default().fit_check_days.max(1);
        let f = fleet(ctx).await?;
        let last: HashMap<String, String> =
            sqlx::query_as::<_, (String, String)>("SELECT collar_id, MAX(checked_at) FROM collar_fit_checks GROUP BY collar_id")
                .fetch_all(ctx.db())
                .await?
                .into_iter()
                .collect();
        let added: HashMap<String, String> = sqlx::query("SELECT id, created_at FROM collars WHERE parked_at IS NULL")
            .fetch_all(ctx.db())
            .await?
            .iter()
            .map(|r| Ok((r.try_get(0)?, r.try_get(1)?)))
            .collect::<anyhow::Result<_>>()?;
        let mut out = Vec::new();
        // Only a collar on an animal has a fit to check.
        for u in f.units.iter().filter(|u| u.collar.animal_id.is_some()) {
            let Some(since) = last.get(&u.collar.id).or(added.get(&u.collar.id)) else { continue };
            let due = from_db(since)? + Duration::days(days as i64);
            if due > now {
                continue;
            }
            let herd = f.herd(&u.collar.herd_id);
            let mut data = json!({ "label": u.label, "herd": herd.name, "due_at": ts(&due), "since": ts(&due) });
            if let Some(p) = &herd.paddock {
                data["paddock"] = json!(p);
            }
            out.push(Candidate {
                key: format!("fit_check_due:{}", u.collar.id),
                subject: ("collar".into(), u.collar.id.clone()),
                herd_id: Some(u.collar.herd_id.clone()),
                title: format!("{} fit check due", u.label),
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
