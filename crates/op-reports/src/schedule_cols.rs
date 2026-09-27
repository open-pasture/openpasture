//! S's columns in the grazing records (`paddock_record`, `nrcs_528`):
//! planned days next to the actual ones, and the residual height when the
//! herd left.
//!
//! - **Planned days**: a strip schedule the herd ran in that paddock during
//!   the stay planned it to end at the schedule's `planned_end` (the time its
//!   last strip was done with, as made). Planned days = that end less the
//!   stay's start. Stays without a schedule have none.
//! - **Residual at exit**: what the herd left, from `paddock_heights`: a
//!   `residual_cm` recorded within two days of the stay's end (never before
//!   the stay began), or a plain `height_cm` measured after the herd left,
//!   within two days. The row nearest the end wins. A height taken before the
//!   herd left (to size strips, say) is the grass it was about to eat, not a
//!   residual. Stays still running have none.
//!
//! Each column shows only when some event in the report has a value.

use chrono::{DateTime, Duration, Utc};
use op_core::Ctx;
use op_core::time::{from_db, to_db};
use op_core::units::Fmt;
use serde_json::Value;
use sqlx::Row;

use crate::Column;
use crate::history::n;
use crate::paddock_record::Event;

/// How far from the exit a measured height counts as the residual at exit.
const RESIDUAL_WINDOW: Duration = Duration::days(2);

/// What S adds to one grazing event.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Extra {
    pub planned_days: Option<f64>,
    pub residual_cm: Option<f64>,
}

/// One [`Extra`] per event, in the same order.
pub async fn load(ctx: &Ctx, events: &[Event<'_>], now: DateTime<Utc>) -> anyhow::Result<Vec<Extra>> {
    let mut out = Vec::with_capacity(events.len());
    for e in events {
        let s = e.stay;
        let end = s.end.unwrap_or(now);
        let planned: Option<String> = sqlx::query_scalar(
            "SELECT planned_end FROM schedules WHERE herd_id = ? AND paddock_id = ? AND planned_end IS NOT NULL
                 AND created_at <= ? AND COALESCE(ended_at, ?) >= ? ORDER BY created_at DESC LIMIT 1",
        )
        .bind(&s.herd_id)
        .bind(&s.paddock_id)
        .bind(to_db(&end))
        .bind(to_db(&now))
        .bind(to_db(&s.start))
        .fetch_optional(ctx.db())
        .await?
        .flatten();
        let planned_days = planned.as_deref().map(from_db).transpose()?.map(|p| ((p - s.start).num_seconds() as f64 / 86_400.0).max(0.0));
        let residual_cm = match s.end {
            Some(end) => {
                let rows = sqlx::query("SELECT at, height_cm, residual_cm FROM paddock_heights WHERE paddock_id = ? AND at >= ? AND at <= ?")
                    .bind(&s.paddock_id)
                    .bind(to_db(&(end - RESIDUAL_WINDOW).max(s.start)))
                    .bind(to_db(&(end + RESIDUAL_WINDOW)))
                    .fetch_all(ctx.db())
                    .await?;
                let mut best: Option<(i64, f64)> = None;
                for r in &rows {
                    let at = from_db(&r.try_get::<String, _>("at")?)?;
                    let cm = match r.try_get::<Option<f64>, _>("residual_cm")? {
                        Some(residual) => residual,
                        // A plain height is the residual only once the herd has left.
                        None if at >= end => r.try_get::<f64, _>("height_cm")?,
                        None => continue,
                    };
                    let gap = (at - end).num_seconds().abs();
                    if best.is_none_or(|(g, _)| gap < g) {
                        best = Some((gap, cm));
                    }
                }
                best.map(|(_, cm)| cm)
            }
            None => None,
        };
        out.push(Extra { planned_days, residual_cm });
    }
    Ok(out)
}

/// Which of the two columns have something to show.
pub fn shown(extra: &[Extra]) -> (bool, bool) {
    (extra.iter().any(|x| x.planned_days.is_some()), extra.iter().any(|x| x.residual_cm.is_some()))
}

/// The columns, placed after the actual days by the caller.
pub fn columns(f: &Fmt, (planned, residual): (bool, bool)) -> Vec<Column> {
    let mut c = Vec::new();
    if planned {
        c.push(Column::new("planned_days", "Planned days").dp(1));
    }
    if residual {
        c.push(Column::unit("residual_exit", "Residual at exit", f.unit_label("height")).dp(1));
    }
    c
}

/// The cells for one event, matching [`columns`].
pub fn cells(f: &Fmt, x: &Extra, (planned, residual): (bool, bool)) -> Vec<Value> {
    let mut c = Vec::new();
    if planned {
        c.push(x.planned_days.map_or(Value::Null, |d| n(d, 1)));
    }
    if residual {
        c.push(x.residual_cm.map_or(Value::Null, |cm| n(f.convert("height", cm), 1)));
    }
    c
}

/// Method lines for the columns shown.
pub fn notes((planned, residual): (bool, bool)) -> Vec<String> {
    let mut n = Vec::new();
    if planned {
        n.push("Planned days: from the day in to the end of the strip schedule the herd ran there, as it was made.".into());
    }
    if residual {
        n.push("Residual at exit: the residual recorded nearest the day out, or a height measured in the two days after it.".into());
    }
    n
}
