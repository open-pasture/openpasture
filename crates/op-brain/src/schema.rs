//! The JSON Schema every LLM backend answers with.
//!
//! Written for OpenAI strict structured outputs (which Codex's
//! `--output-schema` also uses): every property required, no extra
//! properties, optional values as `null`. Anthropic tool input and Claude
//! Code's `--json-schema` accept it as is.

use serde_json::{Value, json};

/// JSON Schema of [`crate::DecisionOutput`] as the model writes it (without `model`,
/// which the backend fills in).
pub fn decision_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["action", "to_paddock_id", "geometry", "reasoning", "confidence", "need"],
        "properties": {
            "action": {
                "type": "string",
                "enum": ["STAY", "MOVE", "NEEDS_INFO"],
                "description": "STAY in the current paddock, MOVE to another, or NEEDS_INFO when the farmer must check something first."
            },
            "to_paddock_id": {
                "type": ["string", "null"],
                "description": "For MOVE: the id of the paddock to move to, from the context. Otherwise null."
            },
            "geometry": {
                "anyOf": [
                    { "type": "null" },
                    {
                        "type": "object",
                        "additionalProperties": false,
                        "required": ["type", "coordinates"],
                        "properties": {
                            "type": { "type": "string", "enum": ["Polygon"] },
                            "coordinates": {
                                "type": "array",
                                "items": {
                                    "type": "array",
                                    "items": { "type": "array", "items": { "type": "number" } }
                                }
                            }
                        }
                    }
                ],
                "description": "Only for MOVE with a custom boundary: a GeoJSON Polygon, [longitude, latitude], at most 64 corners. Null to use the paddock's own shape."
            },
            "reasoning": {
                "type": "string",
                "description": "Two to five plain sentences for the farmer: what you saw and why."
            },
            "confidence": {
                "type": "number",
                "description": "0 to 1."
            },
            "need": {
                "type": ["string", "null"],
                "description": "For NEEDS_INFO: the one thing the farmer should check or tell you. Otherwise null."
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_shape() {
        let s = decision_schema();
        assert_eq!(s["type"], "object");
        assert_eq!(s["additionalProperties"], false);
        let props: Vec<&String> = s["properties"].as_object().unwrap().keys().collect();
        let required: Vec<&str> = s["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        // Strict mode: every property is required.
        assert_eq!(props.len(), required.len());
        assert!(props.iter().all(|p| required.contains(&p.as_str())));
        assert_eq!(s["properties"]["action"]["enum"], json!(["STAY", "MOVE", "NEEDS_INFO"]));
        let geo = &s["properties"]["geometry"]["anyOf"][1];
        assert_eq!(geo["additionalProperties"], false);
    }

    #[test]
    fn output_round_trips_through_the_parser() {
        // What a model writes under this schema parses back.
        let v = json!({ "action": "MOVE", "to_paddock_id": "pad_b", "geometry": null, "reasoning": "r", "confidence": 0.9, "need": null });
        let out = crate::parse::parse_output(&v).unwrap();
        assert_eq!(out.action, crate::Action::Move);
        // And DecisionOutput serialises with the same field names.
        let back = serde_json::to_value(&out).unwrap();
        for k in s_keys() {
            assert!(back.get(&k).is_some(), "{k}");
        }
    }

    fn s_keys() -> Vec<String> {
        decision_schema()["properties"].as_object().unwrap().keys().cloned().collect()
    }
}
