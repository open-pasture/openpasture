//! The no-LLM brain, ported from the kit's `HeuristicAdvisor`
//! (`briefing/advisor.py`). It reads field notes first, then collar and land
//! signals, and asks for a note when it has none.
//!
//! One addition over the kit: with no field notes, a paddock's `grazed_until`
//! still gives a call (move once the planned graze has ended, else stay), so a
//! farm with collars but no notes gets decisions.

use chrono::{DateTime, Utc};
use op_core::BrainId;
use serde_json::Value;

use crate::{Action, Brain, DecisionOutput, DecisionRequest};

const MOVE_SIGNALS: [&str; 8] = ["short", "overgraz", "bare", "mud", "trampled", "tight", "hungry", "pug"];
const STAY_SIGNALS: [&str; 6] = ["plenty", "good residual", "abundant", "fresh", "ready", "rested"];
const READY_STATUSES: [&str; 2] = ["resting", "ready"];
const LOW: f64 = 0.3;
const MEDIUM: f64 = 0.6;

pub struct HeuristicBrain;

#[async_trait::async_trait]
impl Brain for HeuristicBrain {
    fn id(&self) -> BrainId {
        BrainId::Heuristic
    }

    async fn decide(&self, req: DecisionRequest) -> anyhow::Result<DecisionOutput> {
        req.say("Heuristic: reading field notes and signals");
        let out = decide(&req.context);
        req.say(format!("Heuristic: {}", action_word(out.action)));
        Ok(out)
    }
}

fn action_word(a: Action) -> &'static str {
    match a {
        Action::Stay => "STAY",
        Action::Move => "MOVE",
        Action::NeedsInfo => "NEEDS_INFO",
    }
}

struct Pad<'a> {
    id: &'a str,
    name: &'a str,
    v: &'a Value,
}

impl<'a> Pad<'a> {
    fn from(v: &'a Value) -> Option<Self> {
        let id = v.get("id")?.as_str()?;
        Some(Pad { id, name: v.get("name").and_then(Value::as_str).unwrap_or(id), v })
    }
    fn status(&self) -> String {
        self.v.get("status").and_then(Value::as_str).unwrap_or_default().to_ascii_lowercase()
    }
}

/// A decision from the context JSON (shape in docs/API.md, Decision context).
pub fn decide(ctx: &Value) -> DecisionOutput {
    let paddocks: Vec<Pad> = ctx.get("paddocks").and_then(Value::as_array).map(|a| a.iter().filter_map(Pad::from).collect()).unwrap_or_default();
    let current_id = str_at(ctx, &["current_paddock_id"]).or_else(|| str_at(ctx, &["herd", "paddock_id"]));
    let current = current_id.and_then(|id| paddocks.iter().find(|p| p.id == id));
    let position_source = str_at(ctx, &["position_source"]).unwrap_or("unknown");
    let now = str_at(ctx, &["as_of"]).or_else(|| str_at(ctx, &["now"])).and_then(parse_time).unwrap_or_else(Utc::now);

    let target = choose_target(ctx, &paddocks, current_id);

    let observations: Vec<&Value> =
        ctx.get("observations").and_then(Value::as_array).map(|a| a.iter().filter(|o| !has_tag(o, "land-report")).collect()).unwrap_or_default();
    let field: Vec<&&Value> = observations.iter().filter(|o| is_field_source(str_at(o, &["source"]).unwrap_or(""))).collect();
    let current_text = observations
        .iter()
        .filter(|o| match str_at(o, &["paddock_id"]) {
            None => true,
            Some(p) => Some(p) == current_id,
        })
        .map(|o| obs_text(o).to_lowercase())
        .collect::<Vec<_>>()
        .join(" ");
    let wants_move = MOVE_SIGNALS.iter().any(|s| current_text.contains(s));
    let wants_stay = STAY_SIGNALS.iter().any(|s| current_text.contains(s));

    let mut action = Action::NeedsInfo;
    let mut confidence = LOW;
    let mut reasoning: Vec<String> = Vec::new();
    let inferred = position_source == "observation";

    if field.is_empty() {
        // Not in the kit: the planned graze end is the next best signal.
        let until = current.and_then(|p| str_at(p.v, &["grazed_until"])).and_then(parse_time);
        match (current, until) {
            (Some(cur), Some(until)) if until <= now => {
                if let Some(t) = &target {
                    action = Action::Move;
                    confidence = 0.5;
                    reasoning.push(format!("The planned graze in {} ended {}.", cur.name, until.format("%-d %b")));
                    reasoning.push(format!("{} is available as the next likely move option.", t.name));
                } else {
                    reasoning.push(format!("The planned graze in {} has ended, but there is no other paddock recorded yet.", cur.name));
                }
            }
            (Some(cur), Some(until)) => {
                action = Action::Stay;
                confidence = 0.5;
                reasoning.push(format!("{} is planned for grazing until {}.", cur.name, until.format("%-d %b")));
            }
            _ => reasoning.push("There is no recent field observation from the current paddock.".into()),
        }
    } else if inferred && current_id.is_some() {
        reasoning.push("I inferred the herd's current paddock from the most recent herd-linked field observation.".into());
        if wants_move && let Some(t) = &target {
            (action, confidence) = (Action::Move, MEDIUM);
            reasoning.push("That observation suggests forage pressure or ground stress in the current paddock.".into());
            reasoning.push(format!("{} is available as the next likely move option.", t.name));
        } else if wants_stay {
            (action, confidence) = (Action::Stay, MEDIUM);
            reasoning.push("That observation does not show enough pressure to force a move today.".into());
        }
    } else if wants_move {
        if let Some(t) = &target {
            (action, confidence) = (Action::Move, MEDIUM);
            reasoning.push("Recent observations suggest forage pressure or ground stress in the current paddock.".into());
            reasoning.push(format!("{} is available as the next likely move option.", t.name));
        } else {
            reasoning.push("Current paddock looks stressed, but there is no alternate paddock recorded yet.".into());
        }
    } else if wants_stay || matches!(position_source, "farm_record" | "collar") {
        (action, confidence) = (Action::Stay, MEDIUM);
        reasoning.push("Recent observations do not show urgent signs that animals need to move today.".into());
        reasoning.push("The current paddock still appears workable based on the last field notes.".into());
    }

    let move_target = if action == Action::Move { target.as_ref() } else { None };
    reasoning.extend(signal_notes(ctx, current, move_target));
    if let Some(k) = ctx.get("knowledge").and_then(Value::as_array) {
        reasoning.extend(k.iter().take(2).filter_map(|e| str_at(e, &["body"]).or_else(|| str_at(e, &["content"]))).map(first_sentence));
    }
    reasoning.truncate(6);
    if reasoning.is_empty() {
        reasoning.push("More on-the-ground context is needed before making a confident move call.".into());
    }

    let need = (action == Action::NeedsInfo || confidence <= LOW).then(|| match current {
        Some(p) => format!("How is {} looking? Residual grass height and ground condition.", p.name),
        None => "Which paddock is the herd in now, and how is its grass?".to_owned(),
    });

    DecisionOutput {
        action,
        to_paddock_id: move_target.map(|t| t.id.to_owned()),
        geometry: None,
        reasoning: reasoning.join(" "),
        confidence,
        need,
        model: None,
    }
}

/// The candidate with the most rest (no rest record sorts as fully rested),
/// resting or ready ones first.
fn choose_target<'a>(ctx: &Value, paddocks: &'a [Pad<'a>], current_id: Option<&str>) -> Option<&'a Pad<'a>> {
    let candidates: Vec<&Pad> = match ctx.get("candidate_paddock_ids").and_then(Value::as_array) {
        Some(ids) => ids.iter().filter_map(Value::as_str).filter_map(|id| paddocks.iter().find(|p| p.id == id)).collect(),
        None => paddocks.iter().filter(|p| Some(p.id) != current_id).collect(),
    };
    let mut ready: Vec<&Pad> = candidates.iter().copied().filter(|p| READY_STATUSES.contains(&p.status().as_str())).collect();
    if ready.is_empty() {
        ready = candidates;
    }
    // First with the most rest, like Python's stable sort.
    let mut best: Option<(&Pad, f64)> = None;
    for p in ready {
        let r = rest_days(ctx, p).unwrap_or(1e6);
        if best.is_none_or(|(_, b)| r > b) {
            best = Some((p, r));
        }
    }
    best.map(|(p, _)| p)
}

/// Days since the paddock was last grazed. The exact `last_grazed` time wins
/// over `rest_days`, which is rounded to a tenth of a day: a paddock left ten
/// minutes ago must not tie with one left yesterday afternoon.
fn rest_days(ctx: &Value, p: &Pad) -> Option<f64> {
    let now = str_at(ctx, &["as_of"]).and_then(parse_time);
    let last = p.v.get("last_grazed").and_then(Value::as_str).or_else(|| str_at(ctx, &["signals", "last_grazed", p.id])).and_then(parse_time);
    if let (Some(now), Some(last)) = (now, last) {
        return Some((now - last).num_seconds().max(0) as f64 / 86_400.0);
    }
    ctx.get("signals")
        .and_then(|s| s.get("rest_days"))
        .and_then(|r| r.get(p.id))
        .and_then(Value::as_f64)
        .or_else(|| p.v.get("rest_days").and_then(Value::as_f64))
}

/// One or two plain lines from collar and land signals.
fn signal_notes(ctx: &Value, current: Option<&Pad>, target: Option<&&Pad>) -> Vec<String> {
    let mut notes = Vec::new();
    if let (Some(collars), Some(cur)) = (ctx.get("collars"), current) {
        let counts = collars.get("paddock_fix_counts").and_then(Value::as_object);
        let total = collars
            .get("fixes_24h")
            .or_else(|| collars.get("fix_count"))
            .and_then(Value::as_u64)
            .or_else(|| counts.map(|c| c.values().filter_map(Value::as_u64).sum()))
            .unwrap_or(0);
        if total > 0 {
            let here = counts.and_then(|c| c.get(cur.id)).and_then(Value::as_u64).unwrap_or(0);
            notes.push(format!("Collars place the herd in {} ({here} of {total} recent fixes).", cur.name));
        }
    }
    if let Some(t) = target {
        if let Some(rest) = rest_days(ctx, t) {
            notes.push(match rest {
                r if r >= 1.5 => format!("{} has rested {r:.0} days.", t.name),
                r if r * 24.0 >= 1.5 => format!("{} has rested {:.0} hours.", t.name, r * 24.0),
                _ => format!("{} was grazed within the last hour.", t.name),
            });
        } else if let Some(h) =
            ctx.get("signals").and_then(|s| s.get("forage")).and_then(|f| f.get(t.id)).and_then(|f| f.get("height_inches")).filter(|h| !h.is_null())
        {
            notes.push(format!("Imagery puts {} at roughly {h} inches.", t.name));
        }
    }
    let flags = ctx.get("signals").and_then(|s| s.get("risk_flags")).or_else(|| ctx.get("risk_flags")).and_then(Value::as_array);
    if let Some(flag) = flags.and_then(|f| f.first())
        && str_at(flag, &["level"]) == Some("high")
        && let Some(reason) = str_at(flag, &["reason"])
    {
        notes.push(reason.to_owned());
    }
    notes
}

fn str_at<'a>(v: &'a Value, path: &[&str]) -> Option<&'a str> {
    let mut cur = v;
    for k in path {
        cur = cur.get(k)?;
    }
    cur.as_str().map(str::trim).filter(|s| !s.is_empty())
}

fn obs_text(o: &Value) -> &str {
    str_at(o, &["content"]).or_else(|| str_at(o, &["text"])).or_else(|| str_at(o, &["note"])).unwrap_or("")
}

fn has_tag(o: &Value, tag: &str) -> bool {
    o.get("tags").and_then(Value::as_array).is_some_and(|t| t.iter().any(|x| x.as_str() == Some(tag)))
}

/// The kit's `is_field_observation_source`.
fn is_field_source(source: &str) -> bool {
    let s = source.trim().to_ascii_lowercase().replace(['_', ' '], "-");
    matches!(
        s.as_str(),
        "field" | "field-note" | "field-observation" | "manual" | "farmer" | "farmer-note" | "farmer-observation" | "note" | "manual-note" | "photo" | "image"
    )
}

fn parse_time(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .ok()
        .or_else(|| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().and_then(|d| d.and_hms_opt(0, 0, 0)).map(|t| t.and_utc()))
}

fn first_sentence(s: &str) -> String {
    let s = s.trim();
    let end = s.find(". ").map(|i| i + 1).unwrap_or(s.len());
    let out: String = s[..end].chars().take(200).collect();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixture;
    use serde_json::json;

    #[test]
    fn short_grass_moves_to_most_rested() {
        let out = decide(&fixture::context());
        assert_eq!(out.action, Action::Move);
        assert_eq!(out.to_paddock_id.as_deref(), Some("pad_creek"));
        assert_eq!(out.confidence, MEDIUM);
        assert!(out.geometry.is_none());
        assert!(out.reasoning.contains("Creek has rested 34 days."), "{}", out.reasoning);
        assert!(out.reasoning.contains("Collars place the herd in Home"), "{}", out.reasoning);
        assert!(out.need.is_none());
    }

    #[test]
    fn plenty_left_stays() {
        let mut ctx = fixture::context();
        ctx["observations"] = json!([{ "content": "Plenty of grass, good residual.", "paddock_id": "pad_home", "source": "field" }]);
        let out = decide(&ctx);
        assert_eq!(out.action, Action::Stay);
        assert!(out.to_paddock_id.is_none());
    }

    #[test]
    fn no_notes_asks_for_one() {
        let mut ctx = fixture::context();
        ctx["observations"] = json!([]);
        ctx["paddocks"][0]["grazed_until"] = Value::Null;
        let out = decide(&ctx);
        assert_eq!(out.action, Action::NeedsInfo);
        assert_eq!(out.confidence, LOW);
        assert!(out.need.as_deref().unwrap().contains("Home"));
        assert!(out.reasoning.starts_with("There is no recent field observation"));
    }

    #[test]
    fn weather_and_land_reports_are_not_field_notes() {
        let mut ctx = fixture::context();
        ctx["observations"] = json!([
            { "content": "Short and bare in the imagery.", "source": "satellite", "tags": ["land-report"] },
            { "content": "Hot and dry.", "source": "weather" }
        ]);
        ctx["paddocks"][0]["grazed_until"] = Value::Null;
        assert_eq!(decide(&ctx).action, Action::NeedsInfo);
    }

    #[test]
    fn planned_graze_end_without_notes() {
        let mut ctx = fixture::context();
        ctx["observations"] = json!([]);
        ctx["paddocks"][0]["grazed_until"] = json!("2026-09-25T18:00:00Z");
        let out = decide(&ctx);
        assert_eq!(out.action, Action::Move);
        assert_eq!(out.to_paddock_id.as_deref(), Some("pad_creek"));
        ctx["paddocks"][0]["grazed_until"] = json!("2026-09-28T18:00:00Z");
        assert_eq!(decide(&ctx).action, Action::Stay);
    }

    #[test]
    fn target_prefers_resting_and_unrecorded_rest() {
        let mut ctx = fixture::context();
        // A resting paddock with no rest record counts as fully rested.
        ctx["signals"]["rest_days"]["pad_ridge"] = Value::Null;
        ctx["paddocks"][2]["rest_days"] = Value::Null;
        assert_eq!(decide(&ctx).to_paddock_id.as_deref(), Some("pad_ridge"));
        // No resting candidates: still the most rested of them.
        for i in 1..3 {
            ctx["paddocks"][i]["status"] = json!("grazing");
        }
        assert_eq!(decide(&ctx).to_paddock_id.as_deref(), Some("pad_ridge"));
    }

    #[test]
    fn exact_last_grazed_beats_rounded_rest() {
        // Both round to 0.0 rest days; Creek was left ten minutes ago, Ridge three hours ago.
        let mut ctx = fixture::context();
        for (i, id, at) in [(1, "pad_creek", "2026-09-26T11:50:00Z"), (2, "pad_ridge", "2026-09-26T09:00:00Z")] {
            ctx["paddocks"][i]["rest_days"] = json!(0.0);
            ctx["signals"]["rest_days"][id] = json!(0.0);
            ctx["paddocks"][i]["last_grazed"] = json!(at);
        }
        let out = decide(&ctx);
        assert_eq!(out.to_paddock_id.as_deref(), Some("pad_ridge"));
        assert!(out.reasoning.contains("Ridge has rested 3 hours."), "{}", out.reasoning);
    }

    #[test]
    fn stressed_with_nowhere_to_go() {
        let mut ctx = fixture::context();
        ctx["candidate_paddock_ids"] = json!([]);
        let out = decide(&ctx);
        assert_eq!(out.action, Action::NeedsInfo);
        assert!(out.reasoning.contains("no alternate paddock"));
    }

    #[test]
    fn high_risk_flag_is_mentioned() {
        let mut ctx = fixture::context();
        ctx["signals"]["risk_flags"] = json!([{ "type": "water", "level": "high", "reason": "Trough in Creek is dry." }]);
        assert!(decide(&ctx).reasoning.contains("Trough in Creek is dry."));
    }

    #[tokio::test]
    async fn brain_logs_progress() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let req = DecisionRequest {
            herd_id: "herd_1".into(),
            context: fixture::context(),
            instructions: String::new(),
            mcp_url: String::new(),
            tools: vec![],
            log: tx,
        };
        let out = HeuristicBrain.decide(req).await.unwrap();
        assert_eq!(out.action, Action::Move);
        assert!(rx.recv().await.unwrap().starts_with("Heuristic"));
    }
}
