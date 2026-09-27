//! LATER: the approval prompt again an hour on, to the person who asked, on
//! the channel they asked from, if the decision is still waiting then. The
//! reminder goes as a `reply` (it answers their LATER), so it doesn't open
//! the code-less reply window: it carries the decision's code when there is
//! one.

use std::time::Duration;

use chrono::{DateTime, Utc};
use op_core::alert::MessageLog;
use op_core::messages::{self, Outbound};
use op_core::time::{now, to_db};
use op_core::{Ctx, DecisionStatus};
use sqlx::Row;

use super::act;

/// Queue every reminder due at `now` whose decision still waits; the rest
/// are done without a text. Returns what was queued.
pub async fn run_due(ctx: &Ctx, now: DateTime<Utc>) -> anyhow::Result<Vec<MessageLog>> {
    let rows = sqlx::query("SELECT * FROM text_reminders WHERE done_at IS NULL AND due_at <= ? ORDER BY due_at, id LIMIT 100")
        .bind(to_db(&now))
        .fetch_all(ctx.db())
        .await?;
    let mut out = Vec::new();
    for r in &rows {
        let id: i64 = r.try_get("id")?;
        let decision_id: String = r.try_get("decision_id")?;
        let row = sqlx::query("SELECT * FROM decisions WHERE id = ?").bind(&decision_id).fetch_optional(ctx.db()).await?;
        let d = row.map(|r| op_core::store::decision_from_row(&r)).transpose()?;
        if let Some(d) = d.filter(|d| d.status == DecisionStatus::Proposed) {
            let text = act::prompt(ctx, &d, now).await?;
            out.push(
                messages::enqueue(
                    ctx,
                    Outbound {
                        idempotency_key: format!("later:{id}"),
                        channel: r.try_get("channel")?,
                        to: r.try_get("address")?,
                        text,
                        subject: None,
                        kind: "reply".into(),
                        alert_id: None,
                        decision_id: Some(d.id.clone()),
                        user_id: Some(r.try_get("user_id")?),
                    },
                )
                .await?,
            );
        }
        sqlx::query("UPDATE text_reminders SET done_at = ? WHERE id = ?").bind(to_db(&now)).bind(id).execute(ctx.db()).await?;
    }
    Ok(out)
}

/// Check every 15 s.
pub fn spawn(ctx: Ctx) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(15));
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tick.tick() => {
                    if let Err(e) = run_due(&ctx, now()).await {
                        tracing::warn!("text reminders: {e:#}");
                    }
                }
            }
        }
    });
}
