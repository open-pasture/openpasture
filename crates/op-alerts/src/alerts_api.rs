//! `/api/alerts*` and the alert MCP tools (`list_alerts`, `ack_alert`,
//! `resolve_alert`). docs/API.md "Alerts". Alerts go out as
//! [`store::public`] shows them: never with the approval code.

use axum::extract::{Path, Query, State};
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::alert::Alert;
use op_core::time::{from_db, now};
use op_core::tools::{ToolCall, ToolSpec};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Identity, Role};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::engine::config::{self, Change, Policy};
use crate::engine::store::{self, Filter, ListQuery};
use crate::routing::{self, prefs};
use crate::rules::{RuleConfig, rules};

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/alerts", get(list))
        .route("/api/alerts/rules", get(get_rules).put(put_rules))
        .route("/api/alerts/prefs", get(all_prefs))
        .route("/api/alerts/prefs/me", get(my_prefs).put(put_my_prefs))
        .route("/api/alerts/prefs/{id}", put(put_prefs))
        .route("/api/alerts/{id}", get(get_one))
        .route("/api/alerts/{id}/ack", post(post_ack))
        .route("/api/alerts/{id}/resolve", post(post_resolve))
}

#[derive(Debug, Default, Deserialize)]
pub struct ListParams {
    pub status: Option<String>,
    pub herd_id: Option<String>,
    pub limit: Option<i64>,
    /// Opened at or after (RFC 3339).
    pub from: Option<String>,
    /// Opened before (RFC 3339).
    pub to: Option<String>,
}

fn time_param(name: &str, v: Option<&str>) -> ApiResult<Option<DateTime<Utc>>> {
    v.filter(|s| !s.is_empty()).map(|s| from_db(s).map_err(|_| ApiError::bad_request(format!("{name} must be an RFC 3339 time.")))).transpose()
}

pub async fn list_alerts(ctx: &Ctx, p: &ListParams) -> ApiResult<Vec<Alert>> {
    let limit = p.limit.unwrap_or(100);
    if !(1..=1000).contains(&limit) {
        return Err(ApiError::bad_request("limit must be 1 to 1000."));
    }
    let q = ListQuery {
        filter: Filter::parse(p.status.as_deref())?,
        herd_id: p.herd_id.as_deref().filter(|h| !h.is_empty()),
        from: time_param("from", p.from.as_deref())?,
        to: time_param("to", p.to.as_deref())?,
        limit,
    };
    Ok(store::list(ctx, &q).await?.into_iter().map(store::public).collect())
}

async fn list(State(ctx): State<Ctx>, Query(p): Query<ListParams>) -> ApiResult<Json<Vec<Alert>>> {
    Ok(Json(list_alerts(&ctx, &p).await?))
}

async fn get_one(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<Json<Alert>> {
    store::get(&ctx, &id).await?.map(|a| Json(store::public(a))).ok_or_else(|| ApiError::not_found("No such alert."))
}

async fn post_ack(State(ctx): State<Ctx>, id: Identity, Path(alert): Path<String>) -> ApiResult<Json<Alert>> {
    id.require(Role::Hand)?;
    Ok(Json(store::public(store::ack(&ctx, &alert, &id.actor(), now()).await?)))
}

async fn post_resolve(State(ctx): State<Ctx>, id: Identity, Path(alert): Path<String>) -> ApiResult<Json<Alert>> {
    id.require(Role::Hand)?;
    Ok(Json(store::public(store::resolve_by(&ctx, &alert, &id.actor(), now()).await?)))
}

// ---- rules and policy -----------------------------------------------------------------

/// One rule as Settings shows it.
#[derive(Debug, Serialize)]
pub struct RuleView {
    pub kind: &'static str,
    pub sentence: &'static str,
    pub unit: &'static str,
    pub cadence_s: u32,
    pub default: RuleConfig,
    #[serde(flatten)]
    pub config: RuleConfig,
}

#[derive(Debug, Serialize)]
pub struct RulesView {
    pub rules: Vec<RuleView>,
    pub policy: Policy,
    /// Channels that can send now (`op_core::notify_config`).
    pub configured: Vec<&'static str>,
    /// Channels a person can choose, given those.
    pub person_channels: Vec<&'static str>,
}

pub async fn rules_view(ctx: &Ctx) -> ApiResult<RulesView> {
    let mut configs = config::rule_configs(ctx).await?;
    let configured = op_core::notify_config::configured_channels(ctx).await?;
    Ok(RulesView {
        rules: rules()
            .iter()
            .map(|r| {
                let d = r.descriptor();
                let config = configs.remove(d.kind).unwrap_or_else(|| d.default.clone());
                RuleView { kind: d.kind, sentence: d.sentence, unit: d.unit, cadence_s: d.cadence_s, default: d.default, config }
            })
            .collect(),
        policy: config::policy(ctx).await?,
        person_channels: routing::person_channels(&crate::notify::alert_channels(ctx).await?),
        configured,
    })
}

async fn get_rules(State(ctx): State<Ctx>) -> ApiResult<Json<RulesView>> {
    Ok(Json(rules_view(&ctx).await?))
}

async fn put_rules(State(ctx): State<Ctx>, id: Identity, ApiJson(change): ApiJson<Change>) -> ApiResult<Json<RulesView>> {
    id.require(Role::Manager)?;
    config::update(&ctx, change).await?;
    crate::engine::wake(&ctx);
    Ok(Json(rules_view(&ctx).await?))
}

// ---- prefs ----------------------------------------------------------------------------

async fn person(ctx: &Ctx, user_id: &str) -> ApiResult<prefs::PersonPrefs> {
    prefs::all(ctx).await?.into_iter().find(|p| p.user_id == user_id).ok_or_else(|| ApiError::not_found("No such person."))
}

fn me(id: &Identity) -> ApiResult<&str> {
    id.user_id.as_deref().ok_or_else(|| ApiError::not_found("This sign-in isn't a person on the farm."))
}

async fn all_prefs(State(ctx): State<Ctx>, id: Identity) -> ApiResult<Json<Vec<prefs::PersonPrefs>>> {
    id.require(Role::Manager)?;
    Ok(Json(prefs::all(&ctx).await?))
}

async fn my_prefs(State(ctx): State<Ctx>, id: Identity) -> ApiResult<Json<prefs::PersonPrefs>> {
    id.require(Role::Viewer)?;
    Ok(Json(person(&ctx, me(&id)?).await?))
}

async fn put_my_prefs(State(ctx): State<Ctx>, id: Identity, ApiJson(patch): ApiJson<Value>) -> ApiResult<Json<prefs::PersonPrefs>> {
    id.require(Role::Hand)?;
    let uid = me(&id)?.to_owned();
    prefs::put(&ctx, &uid, &patch).await?;
    Ok(Json(person(&ctx, &uid).await?))
}

async fn put_prefs(State(ctx): State<Ctx>, id: Identity, Path(user_id): Path<String>, ApiJson(patch): ApiJson<Value>) -> ApiResult<Json<prefs::PersonPrefs>> {
    id.require(Role::Owner)?;
    prefs::put(&ctx, &user_id, &patch).await?;
    Ok(Json(person(&ctx, &user_id).await?))
}

// ---- MCP tools ------------------------------------------------------------------------

fn id_arg(c: &ToolCall) -> Result<String, ApiError> {
    c.args.get("id").and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned).ok_or_else(|| ApiError::bad_request("id is required."))
}

pub fn list_alerts_tool() -> ToolSpec {
    ToolSpec {
        name: "list_alerts",
        description: "Alerts on the farm, most urgent first: animals outside or escaped, silent collars, low batteries, boundaries not applied, decisions waiting, stalled moves. status: open (unacked), acked, resolved, all; default open and acked.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "status": { "type": "string", "enum": ["open", "acked", "resolved", "all"] },
                "herd_id": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1, "maximum": 1000 }
            },
            "required": [],
            "additionalProperties": false
        }),
        read: true,
        brain: false,
        min_role: Role::Viewer,
        run: ToolSpec::run_fn(|c: ToolCall| async move {
            let p = ListParams {
                status: c.args.get("status").and_then(Value::as_str).map(str::to_owned),
                herd_id: c.args.get("herd_id").and_then(Value::as_str).map(str::to_owned),
                limit: c.args.get("limit").and_then(Value::as_i64),
                from: None,
                to: None,
            };
            Ok(json!({ "alerts": list_alerts(&c.ctx, &p).await? }))
        }),
    }
}

pub fn ack_alert_tool() -> ToolSpec {
    ToolSpec {
        name: "ack_alert",
        description: "Acknowledge an alert: someone has it in hand, so it stops re-notifying and escalating.",
        input_schema: json!({ "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"], "additionalProperties": false }),
        read: false,
        brain: false,
        min_role: Role::Hand,
        run: ToolSpec::run_fn(|c: ToolCall| async move {
            let id = id_arg(&c)?;
            Ok(json!(store::public(store::ack(&c.ctx, &id, &c.identity.actor(), now()).await?)))
        }),
    }
}

pub fn resolve_alert_tool() -> ToolSpec {
    ToolSpec {
        name: "resolve_alert",
        description: "Resolve an alert by hand. It stays closed while its cause lasts; if the cause clears and comes back, a new alert opens.",
        input_schema: json!({ "type": "object", "properties": { "id": { "type": "string" } }, "required": ["id"], "additionalProperties": false }),
        read: false,
        brain: false,
        min_role: Role::Hand,
        run: ToolSpec::run_fn(|c: ToolCall| async move {
            let id = id_arg(&c)?;
            Ok(json!(store::public(store::resolve_by(&c.ctx, &id, &c.identity.actor(), now()).await?)))
        }),
    }
}
