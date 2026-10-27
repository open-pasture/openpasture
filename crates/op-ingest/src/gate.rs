//! The safety gate: one check every herd boundary passes before it can reach
//! the collars, whoever sends it (docs/API.md "Safety gate").
//!
//! **Hard findings** are the critical ones [`crate::prepare`] finds: a road
//! (`crosses_road`) or the farm boundary (`crosses_farm_boundary`) crossed, a
//! no-go zone (`enters_no_go`) or a hazard marked hard (`overlaps_hazard`)
//! taken in, all of it in an exclusion (`overlaps_exclusion`), and no water
//! inside when the farm requires it (`no_water`). A shape that can't be sent
//! at all (self-crossing, too small, nowhere clear of the warning zone) never
//! gets this far: `prepare` refuses it.
//!
//! **Who may go past one.** Nobody may send a boundary into a no-go zone. A
//! person (in the app, or a REST client with a person's or the app token)
//! may send one with any other hard finding by naming each code in
//! `confirm`. Every other sender (the brain, an MCP agent, the timer, auto,
//! a strip schedule, a sweep step, an escape pen) is refused on any hard
//! finding. A sweep step carries what a person confirmed for its move.
//! Sweep steps and escape pens are passing shapes: water isn't asked of them.
//!
//! Every decision is logged, allowed or refused, with the sender, who, the
//! hard findings and what was confirmed (`GET /api/gate/log`).

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::check::Finding;
use op_core::time::{from_db, now, to_db};
use op_core::{Actor, ApiError, ApiJson, ApiResult, Ctx, DbEnum, Identity, Polygon, Severity, Via, id};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

/// `gat_…`
pub const GATE: &str = "gat";
/// Hard codes nobody may go past.
pub const NEVER: [&str; 1] = ["enters_no_go"];
/// Where the gate's settings are kept (the settings table).
const SETTINGS_KEY: &str = "gate";

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/gate/log", get(get_log)).route("/api/gate/settings", get(get_settings).put(put_settings))
}

/// Who is sending a boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    /// A person: in the app, or a REST client with their token.
    Farmer,
    /// A REST client with the app token.
    Api,
    /// A decision an MCP agent proposed.
    Mcp,
    /// A decision the farm's brain made.
    Brain,
    /// A proposal applied when its timer ran out.
    Timer,
    /// A proposal applied at once (autonomy auto).
    Auto,
    /// A strip schedule's move.
    Schedule,
    /// A step of a move's sweep.
    Sweep,
    /// An escaped animal's pen.
    Escape,
    /// The server itself (a reissue).
    #[default]
    System,
}

impl DbEnum for SenderKind {}

impl SenderKind {
    /// A person, who may confirm hard findings.
    pub fn is_person(self) -> bool {
        matches!(self, Self::Farmer | Self::Api)
    }

    /// Shapes on the way somewhere: water isn't asked of them.
    fn passing(self) -> bool {
        matches!(self, Self::Sweep | Self::Escape)
    }
}

/// A sender and what they confirmed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sender {
    pub kind: SenderKind,
    pub by: Option<Actor>,
    /// Hard finding codes a person confirmed (`confirm` in the request).
    pub confirm: Vec<String>,
}

impl Sender {
    pub fn of(kind: SenderKind) -> Self {
        Self { kind, ..Default::default() }
    }

    /// A person behind a request: the app token is `api`, anyone else `farmer`.
    pub fn person(identity: &Identity, confirm: Vec<String>) -> Self {
        Self::answered_by(identity.actor(), confirm)
    }

    /// [`Sender::person`] from who answered (a decision's responder).
    pub fn answered_by(by: Actor, confirm: Vec<String>) -> Self {
        let kind = if by.via == Via::AppToken { SenderKind::Api } else { SenderKind::Farmer };
        Self { kind, by: Some(by), confirm }
    }
}

/// What the gate made of a boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub allowed: bool,
    /// The hard findings, as found.
    pub hard: Vec<Finding>,
    /// Hard codes nobody may go past.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub never: Vec<String>,
    /// Hard codes a person still has to confirm (none for other senders).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub confirm: Vec<String>,
    /// Why it was refused, one sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// The hard findings for a sender: the critical ones, less water for passing shapes.
pub fn hard(findings: &[Finding], kind: SenderKind) -> Vec<Finding> {
    findings.iter().filter(|f| f.severity == Severity::Critical && !(kind.passing() && f.code == "no_water")).cloned().collect()
}

/// Judge `findings` for `sender` (see the module docs).
pub fn judge(findings: &[Finding], sender: &Sender) -> Verdict {
    let hard = hard(findings, sender.kind);
    let mut codes: Vec<String> = Vec::new();
    for f in &hard {
        if !codes.contains(&f.code) {
            codes.push(f.code.clone());
        }
    }
    let never: Vec<String> = codes.iter().filter(|c| NEVER.contains(&c.as_str())).cloned().collect();
    let open: Vec<String> = codes.iter().filter(|c| !NEVER.contains(&c.as_str())).cloned().collect();
    let confirm: Vec<String> = if sender.kind.is_person() { open.iter().filter(|c| !sender.confirm.contains(c)).cloned().collect() } else { vec![] };
    let allowed = never.is_empty() && (if sender.kind.is_person() { confirm.is_empty() } else { open.is_empty() });
    let reason = (!allowed).then(|| {
        let said = sentence(&hard);
        if !never.is_empty() {
            format!("{said}. No boundary may take in a no-go zone; change the shape or remove the zone first.")
        } else if sender.kind.is_person() {
            format!("{said}. Send it again confirming {} to go ahead.", confirm.join(", "))
        } else {
            format!("{said}. Only a person can send this, after seeing it on the map.")
        }
    });
    Verdict { allowed, hard, never, confirm, reason }
}

/// "Crosses County Road; goes past the farm boundary"
fn sentence(hard: &[Finding]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for f in hard {
        let t = if parts.is_empty() { f.text.clone() } else { lower_first(&f.text) };
        if !parts.contains(&t) {
            parts.push(t);
        }
    }
    parts.join("; ")
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default()
}

/// The refusal: 409 with the hard findings, the codes a person may confirm
/// and those nobody may.
pub fn refusal(v: &Verdict) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, v.reason.clone().unwrap_or_else(|| "Refused by the safety gate.".into()))
        .with_details(json!({ "findings": v.hard, "confirm": v.confirm, "never": v.never }))
}

/// One boundary at the gate.
pub struct Check<'a> {
    pub herd_id: &'a str,
    /// The shape as it would go to the collars.
    pub geometry: &'a Polygon,
    pub findings: &'a [Finding],
    pub sender: &'a Sender,
    pub decision_id: Option<&'a str>,
    /// An escape pen's collar.
    pub collar_id: Option<&'a str>,
}

/// Judge and log. A refusal is [`refusal`] (409).
pub async fn pass(ctx: &Ctx, c: &Check<'_>) -> ApiResult<Verdict> {
    let v = judge(c.findings, c.sender);
    log(ctx, c, &v).await?;
    if !v.allowed {
        tracing::warn!(herd = %c.herd_id, sender = %c.sender.kind.as_db(), codes = ?v.hard.iter().map(|f| f.code.as_str()).collect::<Vec<_>>(), "boundary refused by the safety gate");
        return Err(refusal(&v));
    }
    Ok(v)
}

async fn log(ctx: &Ctx, c: &Check<'_>, v: &Verdict) -> anyhow::Result<()> {
    let codes: Vec<&str> = v.hard.iter().map(|f| f.code.as_str()).collect();
    let confirmed: Vec<&String> = c.sender.confirm.iter().filter(|x| codes.contains(&x.as_str())).collect();
    sqlx::query(
        "INSERT INTO gate_log (id, at, herd_id, collar_id, sender, actor, decision_id, allowed, codes, confirmed, findings, geometry, reason)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(id::new_id(GATE))
    .bind(to_db(&now()))
    .bind(c.herd_id)
    .bind(c.collar_id)
    .bind(c.sender.kind.as_db())
    .bind(c.sender.by.as_ref().map(serde_json::to_string).transpose()?)
    .bind(c.decision_id)
    .bind(v.allowed)
    .bind(serde_json::to_string(&codes)?)
    .bind(serde_json::to_string(&confirmed)?)
    .bind(serde_json::to_string(&v.hard)?)
    .bind(serde_json::to_string(c.geometry)?)
    .bind(&v.reason)
    .execute(ctx.db())
    .await?;
    Ok(())
}

/// What a person confirmed for a decision the last time the gate let it
/// through: a move's sweep steps carry it.
pub async fn confirmed_for(ctx: &Ctx, decision_id: &str) -> anyhow::Result<Vec<String>> {
    let row: Option<(String,)> = sqlx::query_as(
        "SELECT confirmed FROM gate_log WHERE decision_id = ? AND allowed = 1 AND sender IN ('farmer', 'api') ORDER BY at DESC, rowid DESC LIMIT 1",
    )
    .bind(decision_id)
    .fetch_optional(ctx.db())
    .await?;
    Ok(row.map(|(c,)| serde_json::from_str(&c)).transpose()?.unwrap_or_default())
}

// ---- settings -------------------------------------------------------------------------------

/// The farm's gate settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GateSettings {
    /// A boundary with none of the farm's mapped water inside is refused.
    #[serde(default)]
    pub require_water: bool,
}

pub async fn settings(ctx: &Ctx) -> anyhow::Result<GateSettings> {
    Ok(ctx.store().get_setting::<GateSettings>(SETTINGS_KEY).await?.unwrap_or_default())
}

async fn get_settings(State(ctx): State<Ctx>) -> ApiResult<Json<GateSettings>> {
    Ok(Json(settings(&ctx).await?))
}

async fn put_settings(State(ctx): State<Ctx>, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<GateSettings>> {
    let current = settings(&ctx).await?;
    let next: GateSettings = op_core::patch::apply(&current, &body, &[])?;
    ctx.store().set_setting(SETTINGS_KEY, &next).await?;
    Ok(Json(next))
}

// ---- the log --------------------------------------------------------------------------------

/// One logged gate decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateEntry {
    pub id: String,
    pub at: DateTime<Utc>,
    pub herd_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
    pub sender: SenderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<Actor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    pub allowed: bool,
    pub codes: Vec<String>,
    pub confirmed: Vec<String>,
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<Polygon>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct LogQuery {
    pub herd_id: Option<String>,
    pub decision_id: Option<String>,
    /// Only refusals.
    #[serde(default)]
    pub refused: bool,
    pub limit: Option<i64>,
}

/// Newest first: at most `limit` (1 to 500, default 100).
pub async fn entries(ctx: &Ctx, q: &LogQuery) -> anyhow::Result<Vec<GateEntry>> {
    let rows = sqlx::query(
        "SELECT * FROM gate_log WHERE (?1 IS NULL OR herd_id = ?1) AND (?2 IS NULL OR decision_id = ?2) AND (?3 = 0 OR allowed = 0)
         ORDER BY at DESC, rowid DESC LIMIT ?4",
    )
    .bind(&q.herd_id)
    .bind(&q.decision_id)
    .bind(q.refused)
    .bind(q.limit.unwrap_or(100).clamp(1, 500))
    .fetch_all(ctx.db())
    .await?;
    rows.iter()
        .map(|r| {
            let actor: Option<String> = r.try_get("actor")?;
            let geometry: Option<String> = r.try_get("geometry")?;
            Ok(GateEntry {
                id: r.try_get("id")?,
                at: from_db(&r.try_get::<String, _>("at")?)?,
                herd_id: r.try_get("herd_id")?,
                collar_id: r.try_get("collar_id")?,
                sender: SenderKind::from_db(&r.try_get::<String, _>("sender")?)?,
                by: actor.map(|a| serde_json::from_str(&a)).transpose()?,
                decision_id: r.try_get("decision_id")?,
                allowed: r.try_get("allowed")?,
                codes: serde_json::from_str(&r.try_get::<String, _>("codes")?)?,
                confirmed: serde_json::from_str(&r.try_get::<String, _>("confirmed")?)?,
                findings: serde_json::from_str(&r.try_get::<String, _>("findings")?)?,
                geometry: geometry.map(|g| serde_json::from_str(&g)).transpose()?,
                reason: r.try_get("reason")?,
            })
        })
        .collect()
}

async fn get_log(State(ctx): State<Ctx>, Query(q): Query<LogQuery>) -> ApiResult<Json<Vec<GateEntry>>> {
    Ok(Json(entries(&ctx, &q).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(code: &str, severity: Severity) -> Finding {
        Finding { code: code.into(), severity, text: format!("{code} text"), geometry: None, targets: vec![] }
    }

    #[test]
    fn people_confirm_hard_findings_and_nobody_else_can() {
        let found = [f("crosses_road", Severity::Critical), f("no_water", Severity::Warning), f("simplified", Severity::Info)];
        for kind in [
            SenderKind::Mcp,
            SenderKind::Brain,
            SenderKind::Timer,
            SenderKind::Auto,
            SenderKind::Schedule,
            SenderKind::Sweep,
            SenderKind::Escape,
            SenderKind::System,
        ] {
            let v = judge(&found, &Sender { kind, confirm: vec!["crosses_road".into()], ..Default::default() });
            assert!(!v.allowed, "{kind:?}");
            assert!(v.confirm.is_empty(), "only a person is asked to confirm");
            assert!(v.reason.unwrap().contains("Only a person"));
        }
        let person = |confirm: &[&str]| Sender { kind: SenderKind::Farmer, confirm: confirm.iter().map(|s| s.to_string()).collect(), ..Default::default() };
        let v = judge(&found, &person(&[]));
        assert!(!v.allowed);
        assert_eq!(v.confirm, ["crosses_road"]);
        assert!(judge(&found, &person(&["crosses_road"])).allowed);
        assert!(!judge(&found, &person(&["crosses_farm_boundary"])).allowed, "the code named must be the one found");
        // Warnings and info never stop anyone.
        assert!(judge(&found[1..], &Sender::of(SenderKind::Auto)).allowed);
    }

    #[test]
    fn nobody_goes_into_a_no_go_zone() {
        let found = [f("enters_no_go", Severity::Critical)];
        let v = judge(&found, &Sender { kind: SenderKind::Farmer, confirm: vec!["enters_no_go".into()], ..Default::default() });
        assert!(!v.allowed);
        assert_eq!(v.never, ["enters_no_go"]);
        assert!(v.confirm.is_empty());
        assert!(v.reason.unwrap().contains("no-go zone"));
    }

    #[test]
    fn passing_shapes_are_not_asked_for_water() {
        let found = [f("no_water", Severity::Critical)];
        assert!(judge(&found, &Sender::of(SenderKind::Sweep)).allowed);
        assert!(judge(&found, &Sender::of(SenderKind::Escape)).allowed);
        assert!(!judge(&found, &Sender::of(SenderKind::Schedule)).allowed);
    }

    #[test]
    fn the_app_token_is_api_and_a_person_is_farmer() {
        assert_eq!(Sender::person(&Identity::owner(Via::AppToken), vec![]).kind, SenderKind::Api);
        assert_eq!(Sender::person(&Identity::owner(Via::Local), vec![]).kind, SenderKind::Farmer);
        assert_eq!(SenderKind::Escape.as_db(), "escape");
    }
}
