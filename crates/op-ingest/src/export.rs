//! Collar history export for cross-farm handover.
//!
//! **Scope:** only the named collar's own rows — its fixes, cues, episodes,
//! and activity entries that target that collar. Never other collars' or
//! animals' telemetry from the same herd, and never another farm's data
//! (this server is one farm). Import on the receiving farm is not
//! implemented here (document only).

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use op_core::{ActivityEvent, ApiError, ApiResult, Ctx, Identity, Role};
use serde::Deserialize;
use serde_json::{Value, json};
use sqlx::{Column, Row};

use crate::db;

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/collars/{id}/export", get(export))
}

#[derive(Deserialize)]
struct ExportQuery {
    /// `json` (default) or `ndjson`.
    #[serde(default)]
    format: Option<String>,
}

async fn export(State(ctx): State<Ctx>, identity: Identity, Path(id): Path<String>, Query(q): Query<ExportQuery>) -> ApiResult<axum::response::Response> {
    identity.require(Role::Manager)?;
    let collar = db::get_collar(ctx.db(), &id).await?.ok_or_else(|| ApiError::not_found("No such collar."))?;

    // Always filter by this collar_id only — never by herd_id alone.
    let fixes = table_json(ctx.db(), "SELECT * FROM fixes WHERE collar_id = ? ORDER BY at, COALESCE(t, 0)", &id).await?;
    let cues = table_json(ctx.db(), "SELECT * FROM cues WHERE collar_id = ? ORDER BY at, COALESCE(t, 0)", &id).await?;
    let episodes = table_json(ctx.db(), "SELECT * FROM episodes WHERE collar_id = ? ORDER BY start_at, id", &id).await?;
    let audit = collar_audit(&ctx, &id).await?;
    let serial: Option<String> = sqlx::query_scalar("SELECT serial FROM fleet_units WHERE collar_id = ?").bind(&id).fetch_optional(ctx.db()).await?;

    debug_assert!(fixes.iter().all(|r| r.get("collar_id") == Some(&json!(id))));
    debug_assert!(cues.iter().all(|r| r.get("collar_id") == Some(&json!(id))));
    debug_assert!(episodes.iter().all(|r| r.get("collar_id") == Some(&json!(id))));

    let body = json!({
        "version": 1,
        "exported_at": op_core::time::now(),
        "collar": collar,
        "serial": serial,
        "fixes": fixes,
        "cues": cues,
        "episodes": episodes,
        "audit": audit,
        "scope": "collar_only",
        "import": "not_supported",
        "note": "Contains only this collar's fixes, cues, episodes and audit. Import on another farm is not automatic.",
    });

    if q.format.as_deref() == Some("ndjson") {
        let mut out = String::new();
        out.push_str(
            &json!({"type":"meta","version":1,"scope":"collar_only","exported_at": body["exported_at"], "serial": serial, "collar": collar}).to_string(),
        );
        out.push('\n');
        for (kind, rows) in [("fix", &fixes), ("cue", &cues), ("episode", &episodes), ("audit", &audit)] {
            for row in rows.iter() {
                out.push_str(&json!({"type": kind, "row": row}).to_string());
                out.push('\n');
            }
        }
        let mut headers = HeaderMap::new();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/x-ndjson"));
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_str(&format!("attachment; filename=\"{id}.ndjson\"")).unwrap_or(HeaderValue::from_static("attachment")),
        );
        return Ok((StatusCode::OK, headers, out).into_response());
    }

    Ok(Json(body).into_response())
}

/// Activity lines that target this collar, excluding any that also target
/// another collar (herd-wide digests must not leak neighbours).
async fn collar_audit(ctx: &Ctx, collar_id: &str) -> anyhow::Result<Vec<Value>> {
    let events = ctx.store().list_events(Some(("collar", collar_id)), 1000).await?;
    let mut out = Vec::with_capacity(events.len());
    for e in events {
        if targets_other_collar(&e, collar_id) {
            continue;
        }
        if let Ok(v) = serde_json::to_value(e) {
            out.push(v);
        }
    }
    Ok(out)
}

fn targets_other_collar(e: &ActivityEvent, collar_id: &str) -> bool {
    e.targets.iter().any(|(kind, id)| kind == "collar" && id != collar_id)
}

async fn table_json(db: &sqlx::SqlitePool, sql: &str, collar_id: &str) -> anyhow::Result<Vec<Value>> {
    let rows = sqlx::query(sql).bind(collar_id).fetch_all(db).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let mut map = serde_json::Map::new();
        for col in r.columns() {
            let name = col.name();
            let v = if let Ok(s) = r.try_get::<String, _>(name) {
                Value::String(s)
            } else if let Ok(i) = r.try_get::<i64, _>(name) {
                json!(i)
            } else if let Ok(f) = r.try_get::<f64, _>(name) {
                json!(f)
            } else if let Ok(b) = r.try_get::<bool, _>(name) {
                json!(b)
            } else if let Ok(None) = r.try_get::<Option<String>, _>(name) {
                Value::Null
            } else {
                Value::Null
            };
            map.insert(name.to_owned(), v);
        }
        // Defence in depth: drop any row that somehow is not this collar.
        if map.get("collar_id").and_then(|v| v.as_str()) != Some(collar_id) {
            continue;
        }
        out.push(Value::Object(map));
    }
    Ok(out)
}
