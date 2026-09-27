//! Rules about a herd's decisions and moves.

use std::collections::HashMap;

use async_trait::async_trait;
use chrono::{DateTime, Duration, Utc};
use op_core::time::{from_db, to_db};
use op_core::{Ctx, DbEnum, Decision, DecisionAction, DecisionStatus, FenceState, LonLat, Paddock, Polygon, Severity};
use rand::Rng;
use serde_json::{Value, json};
use sqlx::Row;

use super::fleet::fleet;
use super::{Candidate, Rule, RuleConfig, RuleDescriptor, after_min, config, ts};

fn paddock_name(paddocks: &[Paddock], id: Option<&str>) -> Option<String> {
    paddocks.iter().find(|p| Some(p.id.as_str()) == id).map(|p| p.name.clone())
}

/// The paddock a target mostly sits in: the one holding its centroid.
fn paddock_of(paddocks: &[Paddock], g: &Polygon) -> Option<String> {
    let c = g.centroid()?;
    paddocks.iter().filter(|p| p.geometry.contains(c)).min_by(|a, b| a.area_ha.total_cmp(&b.area_ha)).map(|p| p.name.clone())
}

// ---- decision waiting -----------------------------------------------------------------

/// A proposed decision nobody has answered for `after_min`, or a timer
/// decision (it applies on its own unless someone says no). Its text is the
/// approval prompt, with a 4-digit code kept in `data.code` for replies.
pub struct DecisionWaiting;

#[async_trait]
impl Rule for DecisionWaiting {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "decision_waiting",
            sentence: "Decision waiting {n}",
            unit: "min",
            default: config(true, Severity::Warning, Some(30), None, true),
            cadence_s: 10,
            wake_on: &["decision"],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, cfg: &RuleConfig, now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let after = Duration::minutes(after_min(cfg, &self.descriptor().default, 30) as i64);
        let rows =
            sqlx::query("SELECT * FROM decisions WHERE status = ? ORDER BY created_at").bind(DecisionStatus::Proposed.as_db()).fetch_all(ctx.db()).await?;
        let decisions = rows.iter().map(op_core::store::decision_from_row).collect::<anyhow::Result<Vec<_>>>()?;
        let waiting: Vec<Decision> = decisions
            .into_iter()
            .filter(|d| matches!(d.action, Some(DecisionAction::Move | DecisionAction::Stay)))
            .filter(|d| d.apply_at.is_some() || now - d.created_at >= after)
            .collect();
        if waiting.is_empty() {
            return Ok(vec![]);
        }
        let paddocks = ctx.store().list_paddocks().await?;
        let herds: HashMap<String, op_core::Herd> = ctx.store().list_herds().await?.into_iter().map(|h| (h.id.clone(), h)).collect();
        let mut out = Vec::new();
        for d in waiting {
            let Some(herd) = herds.get(&d.herd_id) else { continue };
            let key = format!("decision_waiting:{}", d.id);
            let code = match existing_code(ctx, &key).await? {
                Some(c) => c,
                None => format!("{:04}", rand::thread_rng().gen_range(1000..=9999)),
            };
            let mut data = json!({ "herd": herd.name, "code": code, "action": d.action.map(|a| a.as_db()) });
            if let Some(t) = d.apply_at {
                data["apply_at"] = json!(to_db(&t));
            }
            let mut targets = vec![("decision".to_owned(), d.id.clone())];
            let (title, at) = if d.action == Some(DecisionAction::Move) {
                let name = paddock_name(&paddocks, d.to_paddock_id.as_deref()).or_else(|| d.geometry.as_ref().and_then(|g| paddock_of(&paddocks, g)));
                if let Some(p) = &d.to_paddock_id {
                    targets.push(("paddock".into(), p.clone()));
                }
                let area =
                    d.geometry.as_ref().map(|g| g.area_ha()).or_else(|| paddocks.iter().find(|p| Some(&p.id) == d.to_paddock_id.as_ref()).map(|p| p.area_ha));
                if let Some(a) = area {
                    data["area_ha"] = json!(a);
                    if let Some(days) = grazing_days(&d, a) {
                        data["days"] = json!(days);
                    }
                }
                if let Some(n) = &name {
                    data["paddock"] = json!(n);
                }
                (name.map_or_else(|| "Move to the new boundary?".to_owned(), |n| format!("Move to {n}?")), d.geometry.as_ref().and_then(|g| g.centroid()))
            } else {
                let name = paddock_name(&paddocks, herd.paddock_id.as_deref());
                if let Some(n) = &name {
                    data["paddock"] = json!(n);
                }
                (name.map_or_else(|| "Stay?".to_owned(), |n| format!("Stay in {n}?")), None)
            };
            out.push(Candidate {
                key,
                subject: ("decision".into(), d.id.clone()),
                herd_id: Some(d.herd_id.clone()),
                title,
                body: None,
                at,
                targets,
                data,
                severity: None,
            });
        }
        Ok(out)
    }
}

/// The code already texted for this decision, so it stays the same while
/// the alert is open.
async fn existing_code(ctx: &Ctx, key: &str) -> anyhow::Result<Option<String>> {
    let data: Option<String> = sqlx::query_scalar("SELECT data FROM alerts WHERE key = ? AND status != 'resolved'").bind(key).fetch_optional(ctx.db()).await?;
    Ok(data.and_then(|d| serde_json::from_str::<Value>(&d).ok()).and_then(|v| v["code"].as_str().map(str::to_owned)))
}

/// Days of grazing the target holds for this herd, from the decision's own
/// signals: `feed_budget_days(forage × area, AU, 11.8, 0.6, 0)`. `None` when
/// the forage or the animal units weren't known.
fn grazing_days(d: &Decision, area_ha: f64) -> Option<f64> {
    use op_engine::calc;
    let signals = &d.inputs["signals"];
    let au = signals["herd_animal_units"].as_f64()?;
    let pid = d.to_paddock_id.as_deref()?;
    let per_ha = signals["forage"][pid]["available_kg_dm_per_ha"].as_f64()?;
    calc::feed_budget_days(per_ha * area_ha, au, calc::DEFAULT_INTAKE_KG_DM_PER_AU_DAY, calc::DEFAULT_UTILIZATION, 0.0)
}

// ---- moves ----------------------------------------------------------------------------

struct MoveRow {
    id: String,
    herd_id: String,
    decision_id: String,
    target: Polygon,
    status: String,
    stragglers: Vec<String>,
    started_at: DateTime<Utc>,
    updated_at: DateTime<Utc>,
    last_step_at: Option<DateTime<Utc>>,
}

/// Each herd's latest move (the sweeping one if any): one indexed read per
/// herd on `moves(herd_id, started_at)`, however many moves the farm has had.
async fn latest_moves(ctx: &Ctx) -> anyhow::Result<Vec<MoveRow>> {
    let herds: Vec<String> = sqlx::query_scalar("SELECT id FROM herds").fetch_all(ctx.db()).await?;
    let mut out = Vec::new();
    for h in herds {
        let row = sqlx::query("SELECT * FROM moves WHERE herd_id = ? ORDER BY (status = 'sweeping') DESC, started_at DESC, id DESC LIMIT 1")
            .bind(&h)
            .fetch_optional(ctx.db())
            .await?;
        let Some(r) = row else { continue };
        let sweep: Value = serde_json::from_str(&r.try_get::<String, _>("sweep")?).unwrap_or(Value::Null);
        out.push(MoveRow {
            id: r.try_get("id")?,
            herd_id: r.try_get("herd_id")?,
            decision_id: r.try_get("decision_id")?,
            target: serde_json::from_str(&r.try_get::<String, _>("target")?)?,
            status: r.try_get("status")?,
            stragglers: serde_json::from_str(&r.try_get::<String, _>("stragglers")?).unwrap_or_default(),
            started_at: from_db(&r.try_get::<String, _>("started_at")?)?,
            updated_at: from_db(&r.try_get::<String, _>("updated_at")?)?,
            last_step_at: sweep["last_step_at"].as_str().and_then(|s| from_db(s).ok()),
        });
    }
    Ok(out)
}

/// Where a move is going, by paddock name: its decision's paddock, else the
/// paddock under the target.
async fn move_to(ctx: &Ctx, paddocks: &[Paddock], m: &MoveRow) -> anyhow::Result<Option<String>> {
    let pid: Option<String> =
        sqlx::query_scalar("SELECT to_paddock_id FROM decisions WHERE id = ?").bind(&m.decision_id).fetch_optional(ctx.db()).await?.flatten();
    Ok(paddock_name(paddocks, pid.as_deref()).or_else(|| paddock_of(paddocks, &m.target)))
}

/// A sweeping move that hasn't stepped for `after_min` (the herd isn't moving
/// up, or the move waits on a staged boundary).
pub struct MoveStalled;

#[async_trait]
impl Rule for MoveStalled {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "move_stalled",
            sentence: "Move stalled {n}",
            unit: "min",
            default: config(true, Severity::Warning, Some(15), None, true),
            cadence_s: 10,
            wake_on: &["move", "boundary"],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, cfg: &RuleConfig, now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let after = Duration::minutes(after_min(cfg, &self.descriptor().default, 15) as i64);
        let stalled: Vec<MoveRow> =
            latest_moves(ctx).await?.into_iter().filter(|m| m.status == "sweeping" && now - m.last_step_at.unwrap_or(m.started_at) >= after).collect();
        if stalled.is_empty() {
            return Ok(vec![]);
        }
        let paddocks = ctx.store().list_paddocks().await?;
        let mut out = Vec::new();
        for m in stalled {
            let Some(herd) = ctx.store().get_herd(&m.herd_id).await? else { continue };
            let staged: bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM boundaries WHERE herd_id = ? AND collar_id IS NULL AND effective_at > ?)")
                .bind(&m.herd_id)
                .bind(ts(&now))
                .fetch_one(ctx.db())
                .await?;
            let to = move_to(ctx, &paddocks, &m).await?;
            let since = m.last_step_at.unwrap_or(m.started_at);
            let mut data = json!({ "herd": herd.name, "since": ts(&since), "staged": staged });
            if let Some(t) = &to {
                data["paddock"] = json!(t);
            }
            out.push(Candidate {
                key: format!("move_stalled:{}", m.id),
                subject: ("move".into(), m.id.clone()),
                herd_id: Some(m.herd_id.clone()),
                title: to.map_or_else(|| "Move stalled".to_owned(), |t| format!("Move to {t} stalled")),
                body: None,
                at: m.target.centroid(),
                targets: vec![("move".into(), m.id.clone()), ("decision".into(), m.decision_id.clone())],
                data,
                severity: None,
            });
        }
        Ok(out)
    }
}

/// Animals a move left behind: its stragglers while it sweeps, and after it
/// is done those still not inside (for a day).
pub struct Stragglers;

#[async_trait]
impl Rule for Stragglers {
    fn descriptor(&self) -> RuleDescriptor {
        RuleDescriptor {
            kind: "stragglers",
            sentence: "A move leaves animals behind",
            unit: "",
            default: config(true, Severity::Info, None, None, false),
            cadence_s: 10,
            wake_on: &["move"],
        }
    }

    async fn evaluate(&self, ctx: &Ctx, _cfg: &RuleConfig, now: DateTime<Utc>) -> anyhow::Result<Vec<Candidate>> {
        let moves: Vec<MoveRow> = latest_moves(ctx)
            .await?
            .into_iter()
            .filter(|m| !m.stragglers.is_empty() && (m.status == "sweeping" || (m.status == "done" && now - m.updated_at < Duration::hours(24))))
            .collect();
        if moves.is_empty() {
            return Ok(vec![]);
        }
        let f = fleet(ctx).await?;
        let mut out = Vec::new();
        for m in moves {
            let behind: Vec<&super::Unit> =
                f.units.iter().filter(|u| m.stragglers.contains(&u.collar.id) && (m.status == "sweeping" || u.collar.state != FenceState::Inside)).collect();
            if behind.is_empty() {
                continue;
            }
            let herd = f.herd(&m.herd_id);
            let points: Vec<LonLat> = behind.iter().filter_map(|u| u.collar.last_fix.as_ref().map(|x| x.point)).collect();
            let labels: Vec<&str> = behind.iter().map(|u| u.label.as_str()).collect();
            let title = if behind.len() == 1 { format!("{} behind", labels[0]) } else { format!("{} behind", behind.len()) };
            out.push(Candidate {
                key: format!("stragglers:{}", m.id),
                subject: ("move".into(), m.id.clone()),
                herd_id: Some(m.herd_id.clone()),
                title,
                body: None,
                at: crate::engine::centroid(&points),
                targets: behind.iter().flat_map(|u| super::collar_targets(u)).chain([("move".to_owned(), m.id.clone())]).collect(),
                data: json!({ "herd": herd.name, "count": behind.len(), "labels": labels }),
                severity: None,
            });
        }
        Ok(out)
    }
}
