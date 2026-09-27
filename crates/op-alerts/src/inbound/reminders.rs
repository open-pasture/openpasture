//! LATER: the approval prompt again an hour on, to the person who asked, on
//! the channel they asked from, if the decision is still waiting then. The
//! reminder goes as a `reply` (it answers their LATER), so it doesn't open
//! the code-less reply window: it carries the decision's code when there is
//! one. Nothing goes when, by then, the person texted STOP, was switched
//! off, is no longer a manager, or that number isn't their verified phone
//! any more.

use std::time::Duration;

use chrono::{DateTime, Utc};
use op_core::alert::MessageLog;
use op_core::messages::{self, Outbound};
use op_core::time::{now, to_db};
use op_core::{Ctx, DecisionStatus, Role};
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
        let user_id: String = r.try_get("user_id")?;
        let address: String = r.try_get("address")?;
        if let Some(d) = d.filter(|d| d.status == DecisionStatus::Proposed)
            && may_remind(ctx, &user_id, &address).await?
        {
            let text = act::prompt(ctx, &d, now).await?;
            out.push(
                messages::enqueue(
                    ctx,
                    Outbound {
                        idempotency_key: format!("later:{id}"),
                        channel: r.try_get("channel")?,
                        to: address.clone(),
                        text,
                        subject: None,
                        kind: "reply".into(),
                        alert_id: None,
                        decision_id: Some(d.id.clone()),
                        user_id: Some(user_id.clone()),
                    },
                )
                .await?,
            );
        }
        sqlx::query("UPDATE text_reminders SET done_at = ? WHERE id = ?").bind(to_db(&now)).bind(id).execute(ctx.db()).await?;
    }
    Ok(out)
}

/// The person may still get this reminder at `address`: switched on, a
/// manager or owner, `address` still their verified phone, and no STOP.
async fn may_remind(ctx: &Ctx, user_id: &str, address: &str) -> anyhow::Result<bool> {
    let Some(u) = op_core::users::get_user(ctx, user_id).await? else { return Ok(false) };
    if u.disabled_at.is_some() || u.phone_verified_at.is_none() || u.phone.as_deref() != Some(address) || !super::can(&u, Role::Manager) {
        return Ok(false);
    }
    let (_, opted_out) = crate::routing::prefs::get(ctx, user_id).await?;
    Ok(!opted_out)
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
