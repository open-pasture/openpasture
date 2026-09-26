//! Decision rows. Every write publishes a `decision` event.

use op_core::store::decision_from_row;
use op_core::{Ctx, DbEnum, Decision, DecisionStatus, Event, time};

pub async fn insert(ctx: &Ctx, d: &Decision) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO decisions (id, herd_id, source, brain, model, status, action, to_paddock_id, geometry, reasoning, confidence, need,
         inputs, apply_at, boundary_id, error, created_at, responded_at, outcome) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&d.id)
    .bind(&d.herd_id)
    .bind(d.source.as_db())
    .bind(d.brain.map(|b| b.as_db()))
    .bind(&d.model)
    .bind(d.status.as_db())
    .bind(d.action.map(|a| a.as_db()))
    .bind(&d.to_paddock_id)
    .bind(d.geometry.as_ref().map(serde_json::to_string).transpose()?)
    .bind(&d.reasoning)
    .bind(d.confidence)
    .bind(&d.need)
    .bind(d.inputs.to_string())
    .bind(d.apply_at.as_ref().map(time::to_db))
    .bind(&d.boundary_id)
    .bind(&d.error)
    .bind(time::to_db(&d.created_at))
    .bind(d.responded_at.as_ref().map(time::to_db))
    .bind(d.outcome.as_ref().map(|o| o.to_string()))
    .execute(ctx.db())
    .await?;
    ctx.publish(Event::Decision { decision: d.clone() });
    Ok(())
}

pub async fn update(ctx: &Ctx, d: &Decision) -> anyhow::Result<()> {
    sqlx::query(
        "UPDATE decisions SET source = ?, brain = ?, model = ?, status = ?, action = ?, to_paddock_id = ?, geometry = ?, reasoning = ?,
         confidence = ?, need = ?, inputs = ?, apply_at = ?, boundary_id = ?, error = ?, responded_at = ?, outcome = ? WHERE id = ?",
    )
    .bind(d.source.as_db())
    .bind(d.brain.map(|b| b.as_db()))
    .bind(&d.model)
    .bind(d.status.as_db())
    .bind(d.action.map(|a| a.as_db()))
    .bind(&d.to_paddock_id)
    .bind(d.geometry.as_ref().map(serde_json::to_string).transpose()?)
    .bind(&d.reasoning)
    .bind(d.confidence)
    .bind(&d.need)
    .bind(d.inputs.to_string())
    .bind(d.apply_at.as_ref().map(time::to_db))
    .bind(&d.boundary_id)
    .bind(&d.error)
    .bind(d.responded_at.as_ref().map(time::to_db))
    .bind(d.outcome.as_ref().map(|o| o.to_string()))
    .bind(&d.id)
    .execute(ctx.db())
    .await?;
    ctx.publish(Event::Decision { decision: d.clone() });
    Ok(())
}

pub async fn get(ctx: &Ctx, id: &str) -> anyhow::Result<Option<Decision>> {
    let row = sqlx::query("SELECT * FROM decisions WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?;
    row.map(|r| decision_from_row(&r)).transpose()
}

/// Newest first.
pub async fn list(ctx: &Ctx, herd_id: Option<&str>, limit: i64) -> anyhow::Result<Vec<Decision>> {
    let rows = match herd_id {
        Some(h) => {
            sqlx::query("SELECT * FROM decisions WHERE herd_id = ? ORDER BY created_at DESC, id DESC LIMIT ?").bind(h).bind(limit).fetch_all(ctx.db()).await?
        }
        None => sqlx::query("SELECT * FROM decisions ORDER BY created_at DESC, id DESC LIMIT ?").bind(limit).fetch_all(ctx.db()).await?,
    };
    rows.iter().map(decision_from_row).collect()
}

pub async fn with_status(ctx: &Ctx, status: DecisionStatus) -> anyhow::Result<Vec<Decision>> {
    let rows = sqlx::query("SELECT * FROM decisions WHERE status = ? ORDER BY created_at").bind(status.as_db()).fetch_all(ctx.db()).await?;
    rows.iter().map(decision_from_row).collect()
}
