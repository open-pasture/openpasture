//! Background work (kit `briefing/scheduler.py`): the daily decision for each
//! herd with collars at `settings.decision_time` in the farm's time zone,
//! timer applications, and outcome evaluation. (A farmer's boundary
//! supersedes proposals and moves the herd in op-ingest, not through the bus.)

use std::collections::BTreeMap;
use std::time::Duration;

use chrono::{NaiveTime, Utc};
use op_core::Ctx;
use sqlx::Row;

use crate::cycle;

const TICK: Duration = Duration::from_secs(10);
const EVALUATE_EVERY: u32 = 30; // ticks: five minutes
/// A daily decision missed by more than this (server was off) waits for tomorrow.
const DAILY_GRACE_MINUTES: i64 = 60;
const DAILY_KEY: &str = "engine.daily";

pub fn spawn(c: Ctx) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(TICK);
        let mut n: u32 = 0;
        loop {
            let woken = tokio::select! {
                _ = c.on_shutdown() => break,
                _ = tick.tick() => false,
                _ = c.scheduler_woken() => true,
            };
            if let Err(e) = cycle::apply_due(&c).await {
                tracing::warn!("timer decisions: {e:#}");
            }
            if woken {
                continue;
            }
            if let Err(e) = daily(&c).await {
                tracing::warn!("daily decision: {e:#}");
            }
            if n % EVALUATE_EVERY == 0
                && let Err(e) = cycle::evaluate_due(&c, None).await
            {
                tracing::warn!("outcome evaluation: {e:#}");
            }
            n = n.wrapping_add(1);
        }
    });
}

/// Start today's decision for each herd with collars once the farm's local
/// time passes `decision_time`.
async fn daily(ctx: &Ctx) -> anyhow::Result<()> {
    let Some(farm) = ctx.store().get_farm().await? else { return Ok(()) };
    let settings = ctx.settings().await?;
    let tz: chrono_tz::Tz = farm.timezone.parse().unwrap_or(chrono_tz::UTC);
    let local = Utc::now().with_timezone(&tz);
    let Ok(at) = NaiveTime::parse_from_str(&settings.decision_time, "%H:%M") else { return Ok(()) };
    if !daily_due(local.time(), at) {
        return Ok(());
    }
    let today = local.date_naive().to_string();
    let mut done: BTreeMap<String, String> = ctx.store().get_setting(DAILY_KEY).await?.unwrap_or_default();
    for herd in ctx.store().list_herds().await? {
        if done.get(&herd.id) == Some(&today) {
            continue;
        }
        let collars: i64 = sqlx::query("SELECT COUNT(*) FROM collars WHERE herd_id = ?").bind(&herd.id).fetch_one(ctx.db()).await?.get(0);
        done.insert(herd.id.clone(), today.clone());
        ctx.store().set_setting(DAILY_KEY, &done).await?;
        if collars == 0 {
            continue;
        }
        match cycle::start_decision(ctx, &herd.id).await {
            Ok(d) => tracing::info!(herd = %herd.id, decision = %d.id, "daily decision started"),
            Err(e) => tracing::warn!(herd = %herd.id, "daily decision: {}", e.message),
        }
    }
    Ok(())
}

/// Whether `now` is at or after `at` and within the grace period. Whole
/// seconds, so 13:36:01 is not yet 13:37 (whole minutes would round the last
/// minute before `at` down to zero).
fn daily_due(now: NaiveTime, at: NaiveTime) -> bool {
    (0..DAILY_GRACE_MINUTES * 60).contains(&(now - at).num_seconds())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn daily_is_due_from_the_minute_for_an_hour() {
        let t = |s: &str| NaiveTime::parse_from_str(s, "%H:%M:%S").unwrap();
        let at = t("13:37:00");
        assert!(!daily_due(t("13:36:01"), at), "a minute early");
        assert!(!daily_due(t("13:36:59"), at));
        assert!(daily_due(t("13:37:00"), at));
        assert!(daily_due(t("14:36:59"), at));
        assert!(!daily_due(t("14:37:00"), at), "missed by an hour waits for tomorrow");
        assert!(!daily_due(t("00:10:00"), t("23:50:00")));
    }
}
