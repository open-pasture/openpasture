//! The morning brief's `attention` line (order 50): what doesn't text but
//! wants seeing to, per herd. "Battery low: 031 14%, 118 16%. GPS weak: 207.
//! Fit check due: 5".

use chrono::{DateTime, Utc};
use op_core::Ctx;
use op_core::brief::BriefLine;
use serde_json::Value;
use sqlx::Row;

use super::{gsm7, septets};
use crate::engine::store;

/// Items listed per kind before "+n".
const MAX_ITEMS: usize = 6;
/// Longest line, so the brief stays within a few texts per herd.
const MAX_LINE: usize = 240;

pub fn register(ctx: &Ctx) {
    if ctx.brief_lines().has("attention") {
        return;
    }
    ctx.brief_lines().register(BriefLine {
        name: "attention",
        order: 50,
        run: BriefLine::run_fn(|ctx, herd_id, now| async move { Ok(attention(&ctx, &herd_id, now).await?.into_iter().collect()) }),
    });
}

/// "Battery low", "GPS weak", "Behind", "Fit check due", else the kind in words.
fn heading(kind: &str) -> String {
    match kind {
        "low_battery" => "Battery low".into(),
        "gps_degraded" => "GPS weak".into(),
        "stragglers" => "Behind".into(),
        "fit_check_due" => "Fit check due".into(),
        other => {
            let words = other.replace('_', " ");
            let mut c = words.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        }
    }
}

/// Kinds that are counted, not listed.
const COUNTED: &[&str] = &["fit_check_due"];

fn item(d: &Value) -> Option<String> {
    let l = d.get("label")?.as_str()?;
    Some(match d.get("pct").and_then(Value::as_f64) {
        Some(p) => format!("{l} {}%", p as i64),
        None => l.to_owned(),
    })
}

/// The line for one herd from its open alerts that don't notify (info, or
/// the rule's texts are off). `None` when there is nothing.
pub async fn attention(ctx: &Ctx, herd_id: &str, _now: DateTime<Utc>) -> anyhow::Result<Option<String>> {
    let rows = sqlx::query("SELECT * FROM alerts WHERE herd_id = ? AND status != 'resolved' AND (notify = 0 OR severity = 'info') ORDER BY opened_at, key")
        .bind(herd_id)
        .fetch_all(ctx.db())
        .await?;
    let mut by_kind: Vec<(String, Vec<String>, usize)> = Vec::new();
    for r in &rows {
        let a = store::alert_from_row(r)?;
        let kind: String = r.try_get("kind")?;
        let items: Vec<String> = match a.data.get("members").and_then(Value::as_array) {
            Some(m) => m.iter().filter_map(item).collect(),
            None => match a.data.get("labels").and_then(Value::as_array) {
                Some(l) => l.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect(),
                None => item(&a.data).into_iter().collect(),
            },
        };
        let n = a.data.get("count").and_then(Value::as_u64).map_or(items.len().max(1), |c| c as usize);
        match by_kind.iter_mut().find(|(k, _, _)| *k == kind) {
            Some(e) => {
                e.1.extend(items);
                e.2 += n;
            }
            None => by_kind.push((kind, items, n)),
        }
    }
    let rank = |k: &str| ["low_battery", "gps_degraded", "stragglers", "fit_check_due"].iter().position(|x| *x == k).unwrap_or(9);
    by_kind.sort_by(|a, b| rank(&a.0).cmp(&rank(&b.0)).then_with(|| a.0.cmp(&b.0)));
    let parts: Vec<String> = by_kind
        .into_iter()
        .map(|(kind, items, n)| {
            if COUNTED.contains(&kind.as_str()) || items.is_empty() {
                return format!("{}: {n}", heading(&kind));
            }
            let shown: Vec<&str> = items.iter().take(MAX_ITEMS).map(String::as_str).collect();
            let more = if items.len() > MAX_ITEMS { format!(" +{}", items.len() - MAX_ITEMS) } else { String::new() };
            format!("{}: {}{more}", heading(&kind), shown.join(", "))
        })
        .collect();
    if parts.is_empty() {
        return Ok(None);
    }
    let mut line = gsm7(&parts.join(". "));
    while septets(&line) > MAX_LINE {
        line.pop();
    }
    Ok(Some(line))
}
