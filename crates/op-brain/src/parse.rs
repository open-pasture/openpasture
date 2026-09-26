//! Turning model output into a [`DecisionOutput`]: find the JSON in the text,
//! accept the usual variations, validate geometry and paddock ids.

use anyhow::{Context, bail};
use serde_json::Value;

use crate::{Action, DecisionOutput};

/// The decision JSON object in a model's reply: the whole text, a fenced
/// block, or the last balanced `{…}` that looks like a decision.
pub fn extract_json(text: &str) -> Option<Value> {
    let t = text.trim();
    if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(t) {
        return Some(v);
    }
    // Fenced blocks, last first.
    let mut fenced = Vec::new();
    let mut rest = t;
    while let Some(start) = rest.find("```") {
        let after = &rest[start + 3..];
        let body_start = after.find('\n').map(|i| i + 1).unwrap_or(0);
        let Some(end) = after[body_start..].find("```") else { break };
        fenced.push(after[body_start..body_start + end].trim());
        rest = &after[body_start + end + 3..];
    }
    for block in fenced.iter().rev() {
        if let Ok(v @ Value::Object(_)) = serde_json::from_str::<Value>(block) {
            return Some(v);
        }
    }
    // Balanced objects anywhere; prefer the last one that has an action.
    let objects = balanced_objects(t);
    let parsed: Vec<Value> = objects.iter().filter_map(|s| serde_json::from_str::<Value>(s).ok()).collect();
    parsed.iter().rev().find(|v| find_decision(v).is_some()).or_else(|| parsed.last()).cloned()
}

/// Top-level `{…}` spans, skipping braces inside strings.
fn balanced_objects(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut start, mut in_str, mut esc) = (0usize, 0usize, false, false);
    for (i, &b) in bytes.iter().enumerate() {
        if in_str {
            match (esc, b) {
                (true, _) => esc = false,
                (false, b'\\') => esc = true,
                (false, b'"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match b {
            b'"' if depth > 0 => in_str = true,
            b'{' => {
                if depth == 0 {
                    start = i;
                }
                depth += 1;
            }
            b'}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    out.push(&text[start..=i]);
                }
            }
            _ => {}
        }
    }
    out
}

/// The object holding `action`, possibly wrapped (`{ "decision": {…} }`).
fn find_decision(v: &Value) -> Option<&serde_json::Map<String, Value>> {
    let obj = v.as_object()?;
    if obj.contains_key("action") {
        return Some(obj);
    }
    for key in ["decision", "output", "result", "structured_output", "data"] {
        if let Some(inner) = obj.get(key).and_then(find_decision) {
            return Some(inner);
        }
    }
    None
}

/// Parse a model's reply text.
pub fn parse_text(text: &str) -> anyhow::Result<DecisionOutput> {
    let v = extract_json(text).with_context(|| format!("no JSON in the reply: {}", snippet(text)))?;
    parse_output(&v)
}

/// Parse a decision object. Lenient about spelling, strict about meaning.
pub fn parse_output(v: &Value) -> anyhow::Result<DecisionOutput> {
    let obj = find_decision(v).with_context(|| format!("the reply has no action: {}", snippet(&v.to_string())))?;

    let action = parse_action(obj.get("action").unwrap_or(&Value::Null))?;
    let to_paddock_id = str_field(obj, &["to_paddock_id", "paddock_id", "to_paddock", "target_paddock_id"]);
    let geometry = match obj.get("geometry").or_else(|| obj.get("boundary")) {
        None | Some(Value::Null) => None,
        Some(g) => match parse_geometry(g) {
            Ok(p) => Some(p),
            // The engine can still use the paddock's own shape.
            Err(_) if to_paddock_id.is_some() => None,
            Err(e) => return Err(e.context("the boundary is not a valid polygon")),
        },
    };
    let reasoning = match obj.get("reasoning").or_else(|| obj.get("reason")) {
        Some(Value::String(s)) => s.trim().to_owned(),
        Some(Value::Array(items)) => items.iter().filter_map(|i| i.as_str()).map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" "),
        _ => String::new(),
    };
    let confidence = parse_confidence(obj.get("confidence"));
    let need = str_field(obj, &["need", "needs", "question", "uncertainty_request"]);
    let model = str_field(obj, &["model"]);

    let (to_paddock_id, geometry) = if action == Action::Move {
        if to_paddock_id.is_none() && geometry.is_none() {
            bail!("MOVE without a paddock or a boundary");
        }
        (to_paddock_id, geometry)
    } else {
        (None, None)
    };
    Ok(DecisionOutput { action, to_paddock_id, geometry, reasoning, confidence, need, model })
}

fn parse_action(v: &Value) -> anyhow::Result<Action> {
    let s = v.as_str().unwrap_or_default().trim().to_ascii_uppercase().replace([' ', '-'], "_");
    Ok(match s.as_str() {
        "STAY" | "HOLD" | "REMAIN" => Action::Stay,
        "MOVE" => Action::Move,
        "NEEDS_INFO" | "NEED_INFO" | "NEEDSINFO" | "NEEDS_MORE_INFO" | "INFO" => Action::NeedsInfo,
        _ => bail!("unknown action {v}"),
    })
}

fn str_field(obj: &serde_json::Map<String, Value>, keys: &[&str]) -> Option<String> {
    keys.iter()
        .find_map(|k| obj.get(*k).and_then(Value::as_str))
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.eq_ignore_ascii_case("null") && !s.eq_ignore_ascii_case("none"))
        .map(str::to_owned)
}

/// Numbers in 0..1, percentages, or the kit's low/medium/high.
pub fn parse_confidence(v: Option<&Value>) -> f64 {
    let n = match v {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.5),
        Some(Value::String(s)) => {
            let s = s.trim().trim_end_matches('%').to_ascii_lowercase();
            match s.as_str() {
                "low" => 0.3,
                "medium" | "moderate" => 0.6,
                "high" => 0.85,
                _ => s.parse::<f64>().unwrap_or(0.5),
            }
        }
        _ => 0.5,
    };
    let n = if n > 1.0 && n <= 100.0 { n / 100.0 } else { n };
    if n.is_finite() { n.clamp(0.0, 1.0) } else { 0.5 }
}

/// A GeoJSON Polygon, a Feature holding one, or bare coordinates; validated.
pub fn parse_geometry(v: &Value) -> anyhow::Result<op_geo::Polygon> {
    let v = match v {
        Value::String(s) => serde_json::from_str(s).context("geometry is not JSON")?,
        other => other.clone(),
    };
    let poly: op_geo::Polygon = match &v {
        Value::Object(o) if o.get("type").and_then(Value::as_str) == Some("Feature") => {
            serde_json::from_value(o.get("geometry").cloned().unwrap_or(Value::Null))?
        }
        Value::Object(o) if o.get("type").is_none() && o.contains_key("coordinates") => {
            serde_json::from_value(serde_json::json!({ "type": "Polygon", "coordinates": o["coordinates"] }))?
        }
        Value::Array(a) => {
            // [[lon, lat], …] or [[[lon, lat], …]]
            let rings = if a.first().and_then(|p| p.get(0)).is_some_and(Value::is_number) { Value::Array(vec![v.clone()]) } else { v.clone() };
            serde_json::from_value(serde_json::json!({ "type": "Polygon", "coordinates": rings }))?
        }
        _ => serde_json::from_value(v.clone())?,
    };
    Ok(poly.validated()?)
}

/// Check `to_paddock_id` against the context's paddocks. A paddock name is
/// mapped to its id; an unknown id is an error.
pub fn check_paddock(out: &mut DecisionOutput, context: &Value) -> anyhow::Result<()> {
    let Some(want) = out.to_paddock_id.clone() else { return Ok(()) };
    let Some(paddocks) = context.get("paddocks").and_then(Value::as_array) else { return Ok(()) };
    if paddocks.is_empty() {
        return Ok(());
    }
    let id_of = |p: &Value| p.get("id").and_then(Value::as_str).map(str::to_owned);
    if paddocks.iter().any(|p| id_of(p).as_deref() == Some(want.as_str())) {
        return Ok(());
    }
    let by_name = paddocks.iter().find(|p| p.get("name").and_then(Value::as_str).is_some_and(|n| n.trim().eq_ignore_ascii_case(want.trim())));
    match by_name.and_then(id_of) {
        Some(id) => {
            out.to_paddock_id = Some(id);
            Ok(())
        }
        None if out.geometry.is_some() => {
            out.to_paddock_id = None;
            Ok(())
        }
        None => bail!("the brain chose a paddock that doesn't exist ({want})"),
    }
}

/// Parse, then check against the context. Used by every LLM backend.
pub fn finish(v: &Value, context: &Value, model: Option<String>) -> anyhow::Result<DecisionOutput> {
    let mut out = parse_output(v)?;
    check_paddock(&mut out, context)?;
    if out.model.is_none() {
        out.model = model;
    }
    Ok(out)
}

pub(crate) fn snippet(text: &str) -> String {
    let t = text.trim();
    let mut s: String = t.chars().take(200).collect();
    if t.chars().count() > 200 {
        s.push('…');
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SQUARE: &str = r#"{"type":"Polygon","coordinates":[[[-92.41,38.12],[-92.40,38.12],[-92.40,38.13],[-92.41,38.13],[-92.41,38.12]]]}"#;

    #[test]
    fn plain_json() {
        let out = parse_text(r#"{"action":"STAY","to_paddock_id":null,"geometry":null,"reasoning":"Plenty left.","confidence":0.7,"need":null}"#).unwrap();
        assert_eq!(out.action, Action::Stay);
        assert_eq!(out.reasoning, "Plenty left.");
        assert_eq!(out.confidence, 0.7);
        assert!(out.to_paddock_id.is_none() && out.geometry.is_none() && out.need.is_none());
    }

    #[test]
    fn fenced_json_with_prose() {
        let text = "Here is my call.\n\n```json\n{\"action\": \"move\", \"to_paddock_id\": \"pad_b\", \"reasoning\": [\"Short grass.\", \"B rested 30 days.\"], \"confidence\": \"medium\"}\n```\nLet me know.";
        let out = parse_text(text).unwrap();
        assert_eq!(out.action, Action::Move);
        assert_eq!(out.to_paddock_id.as_deref(), Some("pad_b"));
        assert_eq!(out.reasoning, "Short grass. B rested 30 days.");
        assert_eq!(out.confidence, 0.6);
        assert!(out.geometry.is_none());
    }

    #[test]
    fn extra_text_and_braces_in_strings() {
        let text = r#"Thinking {not json}. Final: {"action":"NEEDS_INFO","reasoning":"No note says {x}.","confidence":45,"need":"Walk paddock A"} done"#;
        let out = parse_text(text).unwrap();
        assert_eq!(out.action, Action::NeedsInfo);
        assert_eq!(out.need.as_deref(), Some("Walk paddock A"));
        assert_eq!(out.confidence, 0.45);
        assert_eq!(out.reasoning, "No note says {x}.");
    }

    #[test]
    fn wrapped_and_geometry() {
        let v: Value =
            serde_json::from_str(&format!(r#"{{"decision":{{"action":"MOVE","geometry":{SQUARE},"reasoning":"Strip.","confidence":0.8}}}}"#)).unwrap();
        let out = parse_output(&v).unwrap();
        assert_eq!(out.action, Action::Move);
        assert!(out.geometry.is_some());
        // A Feature and bare coordinates work too.
        let feat = json!({ "type": "Feature", "properties": {}, "geometry": serde_json::from_str::<Value>(SQUARE).unwrap() });
        assert!(parse_geometry(&feat).is_ok());
        assert!(parse_geometry(&json!([[-92.41, 38.12], [-92.40, 38.12], [-92.40, 38.13]])).is_ok());
    }

    #[test]
    fn bad_geometry() {
        let bowtie = json!({"action":"MOVE","geometry":{"type":"Polygon","coordinates":[[[0,0],[1,1],[1,0],[0,1],[0,0]]]},"reasoning":"x","confidence":0.5});
        assert!(parse_output(&bowtie).is_err());
        // With a paddock the bad shape is dropped; the engine uses the paddock.
        let mut with_pad = bowtie.clone();
        with_pad["to_paddock_id"] = json!("pad_b");
        let out = parse_output(&with_pad).unwrap();
        assert!(out.geometry.is_none());
        assert_eq!(out.to_paddock_id.as_deref(), Some("pad_b"));
    }

    #[test]
    fn move_needs_a_target_and_stay_drops_it() {
        assert!(parse_output(&json!({"action":"MOVE","reasoning":"x"})).is_err());
        let out = parse_output(&json!({"action":"STAY","to_paddock_id":"pad_b","reasoning":"x"})).unwrap();
        assert!(out.to_paddock_id.is_none());
        assert!(parse_text("no json here").is_err());
        assert!(parse_output(&json!({"action":"GRAZE"})).is_err());
    }

    #[test]
    fn paddock_names_map_to_ids() {
        let ctx = json!({ "paddocks": [{ "id": "pad_a", "name": "Home" }, { "id": "pad_b", "name": "Creek" }] });
        let mut out = parse_output(&json!({"action":"MOVE","to_paddock_id":"creek","reasoning":"x"})).unwrap();
        check_paddock(&mut out, &ctx).unwrap();
        assert_eq!(out.to_paddock_id.as_deref(), Some("pad_b"));
        let mut out = parse_output(&json!({"action":"MOVE","to_paddock_id":"pad_zz","reasoning":"x"})).unwrap();
        assert!(check_paddock(&mut out, &ctx).is_err());
    }

    #[test]
    fn confidence_forms() {
        assert_eq!(parse_confidence(Some(&json!(0.2))), 0.2);
        assert_eq!(parse_confidence(Some(&json!("80%"))), 0.8);
        assert_eq!(parse_confidence(Some(&json!("high"))), 0.85);
        assert_eq!(parse_confidence(Some(&json!(-3))), 0.0);
        assert_eq!(parse_confidence(None), 0.5);
    }
}
