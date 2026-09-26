//! The one prompt every LLM backend sends.

use serde_json::Value;

/// System text: the role and the output rules.
pub const SYSTEM: &str = "\
You are the grazing brain for an openpasture farm. Once a day you decide where one herd grazes next: \
STAY in its current paddock, MOVE to another paddock, or NEEDS_INFO when the farmer has to look at \
something before a confident call.

Rules:
- The farm context you are given is current. When tools are offered, use them as the Tools section says \
before you answer.
- Only use paddock ids that appear in the context. For MOVE set to_paddock_id.
- Leave geometry null unless the herd needs a custom boundary (a strip or part of a paddock). \
Geometry is a GeoJSON Polygon, [longitude, latitude], at most 64 corners, not crossing itself.
- Confidence is a number from 0 to 1. Be honest; thin data means low confidence.
- For NEEDS_INFO, set need to the one thing the farmer should check.
- Write reasoning for the farmer in a few plain sentences.
- Do not change anything. You only decide; the farmer or the autonomy setting applies it.
- Reply with only a JSON object that matches the output schema. No prose around it.";

/// Build the user prompt: instructions, context and schema. `with_mcp`
/// mentions the read tools on the openpasture MCP server.
pub fn build_prompt(instructions: &str, context: &Value, schema: &Value, with_mcp: bool) -> String {
    let mut out = String::new();
    let instructions = instructions.trim();
    if !instructions.is_empty() {
        out.push_str("## Instructions\n\n");
        out.push_str(instructions);
        out.push_str("\n\n");
    }
    if with_mcp {
        out.push_str(
            "## Tools\n\nRead-only tools on the `openpasture` MCP server (get_farm, list_paddocks, get_herd, \
get_herd_positions, get_boundary_status, get_signals, get_land_report, search_knowledge, list_decisions, \
get_decision, run_sql) can fetch more detail. Before deciding, call search_knowledge once with this \
herd's situation in a few words; the context only carries general entries. Use the others for anything else \
the context leaves open, e.g. get_land_report for a candidate paddock or get_herd_positions for where each \
animal is now. If the tools are unavailable, decide from the context.\n\n",
        );
    }
    out.push_str("## Farm context\n\n```json\n");
    out.push_str(&serde_json::to_string_pretty(context).unwrap_or_else(|_| context.to_string()));
    out.push_str("\n```\n\n## Output schema\n\n```json\n");
    out.push_str(&serde_json::to_string(schema).unwrap_or_default());
    out.push_str("\n```\n\nReply with only the JSON object.\n");
    out
}

/// System text and prompt in one string, for CLIs that take a single prompt.
pub fn build_full_prompt(instructions: &str, context: &Value, schema: &Value, with_mcp: bool) -> String {
    format!("{SYSTEM}\n\n{}", build_prompt(instructions, context, schema, with_mcp))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prompt_has_every_part() {
        let ctx = serde_json::json!({ "herd": { "id": "herd_1" } });
        let p = build_full_prompt("Follow the skill.", &ctx, &crate::decision_schema(), true);
        assert!(p.starts_with("You are the grazing brain"));
        assert!(p.contains("Follow the skill."));
        assert!(p.contains("\"herd_1\""));
        assert!(p.contains("NEEDS_INFO"));
        assert!(p.contains("openpasture` MCP server"));
        assert!(p.contains("call search_knowledge once"));
        let p = build_prompt("", &ctx, &crate::decision_schema(), false);
        assert!(!p.contains("## Instructions"));
        assert!(!p.contains("MCP"));
    }
}
