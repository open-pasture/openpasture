//! op-engine's tools, registered in op-core's tool registry (docs/API.md "MCP
//! tools"). MCP, brains and text questions all list and call them through the
//! registry. Order here is the order `tools/list` shows.
//!
//! `propose_boundary` is for outside agents: it records a decision with
//! source `brain` (an agent, not the farmer), `inputs.via = "mcp"` and the
//! client's name as `model`; the herd's autonomy then applies exactly as for
//! the built-in brain.

use op_core::tools::{ToolCall, ToolSpec};
use op_core::{ApiError, Ctx, Decision, DecisionAction, DecisionSource, DecisionStatus, Herd, Polygon, Role, id, time};
use serde_json::{Value, json};

use crate::{api, context, cycle, db, knowledge};

/// The read tools offered to the daily decision brain, in order.
pub const READ_TOOLS: [&str; 11] = [
    "get_farm",
    "list_paddocks",
    "get_herd",
    "get_herd_positions",
    "get_boundary_status",
    "get_signals",
    "get_land_report",
    "search_knowledge",
    "list_decisions",
    "get_decision",
    "run_sql",
];
/// Write tools for outside agents only.
pub const WRITE_TOOLS: [&str; 1] = ["propose_boundary"];

/// Register op-engine's tools. Idempotent.
pub fn register_tools(ctx: &Ctx) {
    ctx.tools().register_once(env!("CARGO_PKG_NAME"), specs);
}

fn herd_prop() -> Value {
    json!({ "type": "string", "description": "Herd id. Optional when the farm has one herd." })
}

fn schema(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required, "additionalProperties": false })
}

/// A read tool the decision brain gets too.
fn brain_read(name: &'static str, description: &'static str, input_schema: Value) -> ToolSpec {
    ToolSpec { name, description, input_schema, read: true, brain: true, min_role: Role::Viewer, run: ToolSpec::run_fn(move |c: ToolCall| run(name, c)) }
}

/// Every op-engine tool with its description and input schema.
pub fn specs() -> Vec<ToolSpec> {
    vec![
        brain_read("get_farm", "The farm record, its herds, and the decision settings.", schema(json!({}), &[])),
        brain_read("list_paddocks", "Every paddock: id, name, status, area in hectares, GeoJSON geometry, notes, last grazed.", schema(json!({}), &[])),
        brain_read(
            "get_herd",
            "A herd: species, count, animal units, autonomy, the paddock it is in (and how we know), and its collars.",
            schema(json!({ "herd_id": herd_prop() }), &[]),
        ),
        brain_read("get_herd_positions", "Latest fix and fence state for each collar in the herd.", schema(json!({ "herd_id": herd_prop() }), &[])),
        brain_read(
            "get_boundary_status",
            "The herd's active and staged boundaries, any proposal waiting on the farmer, each collar's latest ack, and the move: the target the herd is being swept into (status sweeping, done or stopped; step; remaining_m from the back line to the target; stragglers dropped from the sweep).",
            schema(json!({ "herd_id": herd_prop() }), &[]),
        ),
        brain_read(
            "get_signals",
            "Grazing signals per paddock: rest days, grazing pressure from collars, forage and recovery from imagery, feed budget, weather risk, cue pressure.",
            schema(json!({ "herd_id": herd_prop() }), &[]),
        ),
        brain_read(
            "get_land_report",
            "Land report for a paddock: weather history and forecast, imagery, climate, water. Unavailable sections say why. Cached for six hours.",
            schema(
                json!({
                    "paddock_id": { "type": "string" },
                    "refresh": { "type": "boolean", "description": "Fetch fresh instead of using a report under six hours old." }
                }),
                &["paddock_id"],
            ),
        ),
        brain_read(
            "search_knowledge",
            "Search grazing principles and this farm's own lessons. Ask about the specific situation, e.g. \"residual before heavy rain\".",
            schema(json!({ "query": { "type": "string" }, "limit": { "type": "integer", "minimum": 1, "maximum": 20 } }), &["query"]),
        ),
        brain_read(
            "list_decisions",
            "Recent decisions, newest first, with status, reasoning, the farmer's response and outcome.",
            schema(json!({ "herd_id": { "type": "string" }, "limit": { "type": "integer", "minimum": 1, "maximum": 100 } }), &[]),
        ),
        brain_read("get_decision", "One decision by id.", schema(json!({ "decision_id": { "type": "string" } }), &["decision_id"])),
        brain_read(
            "run_sql",
            "Read-only SQL over fixes, cues, acks, boundaries, decisions, collars, animals, paddocks, herds. Times: `t` is unix ms.",
            schema(json!({ "query": { "type": "string" }, "max_rows": { "type": "integer", "minimum": 1, "maximum": 1000 } }), &["query"]),
        ),
        ToolSpec {
            name: "propose_boundary",
            description: "Propose moving a herd: a paddock, a GeoJSON Polygon boundary, or both, with plain reasons. Recorded as a decision; the herd's autonomy decides whether it waits for the farmer, runs on a timer, or goes to the collars now.",
            input_schema: schema(
                json!({
                    "herd_id": herd_prop(),
                    "to_paddock_id": { "type": "string", "description": "Paddock to move to. Its shape is the boundary unless geometry is given." },
                    "geometry": { "type": "object", "description": "GeoJSON Polygon, [longitude, latitude], 3-64 corners." },
                    "reasoning": { "type": "string", "description": "Two to four short lines, each a reason tied to a fact." },
                    "confidence": { "type": "number", "minimum": 0, "maximum": 1 }
                }),
                &["reasoning"],
            ),
            read: false,
            brain: false,
            min_role: Role::Manager,
            run: ToolSpec::run_fn(|c: ToolCall| async move { propose(&c.ctx, &c.args, c.client).await }),
        },
        // @HUB
        // @HUB-UI
        // @E-lib
        // @E-srv
        // @J
        // @A-engine
        // @A-notify
        // @D
        // @K-animals
        // @K-files
        // @I
        // @B
        // @G
        // @P
        // @Q
        crate::brief::tool(),
        // @C
        // @F
        // @S
        // @A3
        // @H
        // @L
        // @M
        // @Z
    ]
}

fn arg_str<'a>(a: &'a Value, k: &str) -> Option<&'a str> {
    a.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn need_str<'a>(a: &'a Value, k: &str) -> Result<&'a str, ApiError> {
    arg_str(a, k).ok_or_else(|| ApiError::bad_request(format!("{k} is required.")))
}

async fn herd_arg(ctx: &Ctx, a: &Value) -> Result<Herd, ApiError> {
    if let Some(id) = arg_str(a, "herd_id") {
        return ctx.store().get_herd(id).await?.ok_or_else(|| ApiError::not_found(format!("No herd {id}.")));
    }
    let mut herds = ctx.store().list_herds().await?;
    match herds.len() {
        1 => Ok(herds.remove(0)),
        0 => Err(ApiError::not_found("The farm has no herds yet.")),
        n => Err(ApiError::bad_request(format!("The farm has {n} herds; pass herd_id."))),
    }
}

/// The body of each read tool.
async fn run(name: &'static str, c: ToolCall) -> Result<Value, ApiError> {
    let (ctx, a) = (&c.ctx, &c.args);
    let store = ctx.store();
    Ok(match name {
        "get_farm" => {
            let s = ctx.settings().await?;
            json!({
                "farm": store.get_farm().await?,
                "herds": store.list_herds().await?,
                "settings": { "decision_time": s.decision_time, "units": s.units, "brain": s.brain },
            })
        }
        "list_paddocks" => json!({ "paddocks": store.list_paddocks().await? }),
        "get_herd" => {
            let h = herd_arg(ctx, a).await?;
            let paddocks = store.list_paddocks().await?;
            let (current, source, _, _) = context::locate(ctx, &h, &paddocks).await?;
            let collars = context::herd_collars(ctx, &h.id).await?;
            let units = crate::calc::animal_units(&op_core::DbEnum::as_db(&h.species), h.count as i64, None);
            json!({
                "herd": h,
                "animal_units": units,
                "current_paddock_id": current,
                "current_paddock": current.as_deref().map(|p| context::name_of(&paddocks, p)),
                "position_source": source,
                "animals": store.list_animals(Some(&h.id)).await?.len(),
                "collars": collars,
            })
        }
        "get_herd_positions" => {
            let h = herd_arg(ctx, a).await?;
            json!({ "herd_id": h.id, "positions": op_ingest::latest_positions(ctx, &h.id).await? })
        }
        "get_boundary_status" => {
            let h = herd_arg(ctx, a).await?;
            serde_json::to_value(op_ingest::boundary_status(ctx, &h.id).await?).map_err(anyhow::Error::from)?
        }
        "get_signals" => {
            let h = herd_arg(ctx, a).await?;
            api::signals_view(ctx, Some(&h.id)).await?
        }
        "get_land_report" => api::land_view(ctx, need_str(a, "paddock_id")?, a.get("refresh").and_then(Value::as_bool).unwrap_or(false)).await?,
        "search_knowledge" => {
            let limit = a.get("limit").and_then(Value::as_u64).unwrap_or(5) as usize;
            json!({ "results": knowledge::search(ctx, need_str(a, "query")?, limit.min(20)).await? })
        }
        "list_decisions" => {
            let limit = a.get("limit").and_then(Value::as_i64).unwrap_or(10).clamp(1, 100);
            json!({ "decisions": db::list(ctx, arg_str(a, "herd_id"), limit).await? })
        }
        "get_decision" => {
            let id = need_str(a, "decision_id")?;
            serde_json::to_value(db::get(ctx, id).await?.ok_or_else(|| ApiError::not_found(format!("No decision {id}.")))?).map_err(anyhow::Error::from)?
        }
        "run_sql" => {
            let max = a.get("max_rows").and_then(Value::as_u64).unwrap_or(200).clamp(1, 1000) as usize;
            serde_json::to_value(op_analytics::sql::run(ctx, need_str(a, "query")?, max).await?).map_err(anyhow::Error::from)?
        }
        other => return Err(ApiError::bad_request(format!("Unknown tool {other}."))),
    })
}

async fn propose(ctx: &Ctx, a: &Value, client: Option<String>) -> Result<Value, ApiError> {
    let herd = herd_arg(ctx, a).await?;
    let reasoning = need_str(a, "reasoning")?.to_owned();
    let to_paddock_id = arg_str(a, "to_paddock_id").map(str::to_owned);
    let geometry: Option<Polygon> = match a.get("geometry").filter(|g| !g.is_null()) {
        Some(g) => Some(serde_json::from_value(g.clone()).map_err(|e| ApiError::bad_request(format!("geometry must be a GeoJSON Polygon: {e}")))?),
        None => None,
    };
    if to_paddock_id.is_none() && geometry.is_none() {
        return Err(ApiError::bad_request("Give to_paddock_id, geometry, or both."));
    }
    if let Some(g) = &geometry {
        cycle::check_geometry(g)?;
    }
    if let Some(p) = &to_paddock_id
        && ctx.store().get_paddock(p).await?.is_none()
    {
        return Err(ApiError::not_found(format!("No paddock {p}.")));
    }
    let paddocks = ctx.store().list_paddocks().await?;
    let (current, source, _, _) = context::locate(ctx, &herd, &paddocks).await?;
    let d = Decision {
        id: id::new_id(id::DECISION),
        herd_id: herd.id.clone(),
        source: DecisionSource::Brain,
        brain: None,
        model: client,
        status: DecisionStatus::Running,
        action: Some(DecisionAction::Move),
        to_paddock_id,
        geometry,
        reasoning: Some(reasoning),
        confidence: a.get("confidence").and_then(Value::as_f64).map(|c| c.clamp(0.0, 1.0)),
        need: None,
        inputs: json!({ "via": "mcp", "from_paddock_id": current, "position_source": source }),
        apply_at: None,
        boundary_id: None,
        error: None,
        created_at: time::now(),
        responded_at: None,
        outcome: None,
    };
    db::insert(ctx, &d).await?;
    let d = cycle::record(ctx, d, &herd).await?;
    let message = match (d.status, d.apply_at) {
        (DecisionStatus::Failed, _) => format!("Not recorded as a move: {}", d.error.clone().unwrap_or_default()),
        (DecisionStatus::Proposed, Some(at)) => format!("Proposed. It goes to the collars at {} unless the farmer stops it.", time::to_db(&at)),
        (DecisionStatus::Proposed, None) => "Proposed. It waits for the farmer to approve, change, or reject it.".into(),
        (DecisionStatus::Applied, _) => "Move started (the herd is on auto): the collars get the target, or a sweep of boundaries that walks the herd into it. Check get_boundary_status for the move and acks.".into(),
        (s, _) => format!("Decision is {}.", op_core::DbEnum::as_db(&s)),
    };
    Ok(json!({ "decision": d, "message": message }))
}
