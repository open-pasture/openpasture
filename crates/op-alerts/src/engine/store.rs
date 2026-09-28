//! The `alerts` table.

use chrono::{DateTime, Utc};
use op_core::alert::{Alert, AlertStatus};
use op_core::time::{from_db, opt_from_db, to_db};
use op_core::{Actor, ApiError, ApiResult, Ctx, DbEnum, Event, Severity};
use serde_json::Value;
use sqlx::Row as _;
use sqlx::sqlite::SqliteRow;

/// `alr_…`
pub const ALERT: &str = "alr";

/// An alert with the engine's bookkeeping.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    pub alert: Alert,
    pub seen_at: DateTime<Utc>,
    pub notify: bool,
    pub batch_at: Option<DateTime<Utc>>,
    pub routed_at: Option<DateTime<Utc>>,
    pub tier: u32,
    pub escalated_at: Option<DateTime<Utc>>,
    pub renotified: u32,
    pub renotified_at: Option<DateTime<Utc>>,
    /// The collars it had when people were last told about it (a text went
    /// out, or someone acked or closed it); `None` before that.
    pub announced: Option<Vec<String>>,
}

/// SQL: the collar ids in the row's own targets, as a JSON array (what
/// `announced` takes when people are told).
pub const TARGET_COLLARS: &str =
    "(SELECT json_group_array(json_extract(value, '$[1]')) FROM json_each(alerts.targets) WHERE json_extract(value, '$[0]') = 'collar')";

/// `data` keys that never leave the server: the approval code goes out only
/// in the approval text (and A3 reads it from the table).
pub(crate) const PRIVATE_DATA: &[&str] = &["code"];

/// An alert as the API, the MCP tools and live events show it: without the
/// approval code.
pub fn public(mut a: Alert) -> Alert {
    if let Some(d) = a.data.as_object_mut() {
        for k in PRIVATE_DATA {
            d.remove(*k);
        }
    }
    a
}

fn actor(v: Option<String>) -> anyhow::Result<Option<Actor>> {
    Ok(v.map(|s| serde_json::from_str(&s)).transpose()?)
}

pub fn alert_from_row(r: &SqliteRow) -> anyhow::Result<Alert> {
    let lon: Option<f64> = r.try_get("at_lon")?;
    let lat: Option<f64> = r.try_get("at_lat")?;
    Ok(Alert {
        id: r.try_get("id")?,
        kind: r.try_get("kind")?,
        key: r.try_get("key")?,
        severity: Severity::from_db(&r.try_get::<String, _>("severity")?)?,
        status: AlertStatus::from_db(&r.try_get::<String, _>("status")?)?,
        herd_id: r.try_get("herd_id")?,
        title: r.try_get("title")?,
        body: r.try_get("body")?,
        at: lon.zip(lat).map(|(a, b)| [a, b]),
        targets: serde_json::from_str(&r.try_get::<String, _>("targets")?)?,
        data: serde_json::from_str(&r.try_get::<String, _>("data")?)?,
        opened_at: from_db(&r.try_get::<String, _>("opened_at")?)?,
        updated_at: from_db(&r.try_get::<String, _>("updated_at")?)?,
        acked_at: opt_from_db(r.try_get("acked_at")?)?,
        acked_by: actor(r.try_get("acked_by")?)?,
        resolved_at: opt_from_db(r.try_get("resolved_at")?)?,
        resolved_by: actor(r.try_get("resolved_by")?)?,
        rolled_into: r.try_get("rolled_into")?,
    })
}

pub fn row_from(r: &SqliteRow) -> anyhow::Result<Row> {
    Ok(Row {
        alert: alert_from_row(r)?,
        seen_at: from_db(&r.try_get::<String, _>("seen_at")?)?,
        notify: r.try_get("notify")?,
        batch_at: opt_from_db(r.try_get("batch_at")?)?,
        routed_at: opt_from_db(r.try_get("routed_at")?)?,
        tier: r.try_get::<i64, _>("tier")?.max(0) as u32,
        escalated_at: opt_from_db(r.try_get("escalated_at")?)?,
        renotified: r.try_get::<i64, _>("renotified")?.max(0) as u32,
        renotified_at: opt_from_db(r.try_get("renotified_at")?)?,
        announced: r.try_get::<Option<String>, _>("announced")?.map(|s| serde_json::from_str(&s)).transpose()?,
    })
}

pub async fn get(ctx: &Ctx, id: &str) -> anyhow::Result<Option<Alert>> {
    let row = sqlx::query("SELECT * FROM alerts WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?;
    row.map(|r| alert_from_row(&r)).transpose()
}

/// Which alerts `GET /api/alerts` lists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filter {
    /// Open and acked: everything not resolved.
    Active,
    Open,
    Acked,
    Resolved,
    All,
}

impl Filter {
    pub fn parse(s: Option<&str>) -> ApiResult<Self> {
        Ok(match s {
            None | Some("") | Some("active") => Filter::Active,
            Some("open") => Filter::Open,
            Some("acked") => Filter::Acked,
            Some("resolved") => Filter::Resolved,
            Some("all") => Filter::All,
            Some(o) => return Err(ApiError::bad_request(format!("status is open, acked, resolved or all, not {o}."))),
        })
    }
}

pub struct ListQuery<'a> {
    pub filter: Filter,
    pub herd_id: Option<&'a str>,
    pub from: Option<DateTime<Utc>>,
    pub to: Option<DateTime<Utc>>,
    pub limit: i64,
}

/// Newest first; unresolved lists put critical first.
pub async fn list(ctx: &Ctx, q: &ListQuery<'_>) -> anyhow::Result<Vec<Alert>> {
    let status = match q.filter {
        Filter::Active => "status != 'resolved'",
        Filter::Open => "status = 'open'",
        Filter::Acked => "status = 'acked'",
        Filter::Resolved => "status = 'resolved'",
        Filter::All => "1",
    };
    let order = match q.filter {
        Filter::Active | Filter::Open | Filter::Acked => "CASE severity WHEN 'critical' THEN 0 WHEN 'warning' THEN 1 ELSE 2 END, opened_at DESC, id DESC",
        _ => "opened_at DESC, id DESC",
    };
    let sql = format!(
        "SELECT * FROM alerts WHERE {status} AND (?1 IS NULL OR herd_id = ?1) AND (?2 IS NULL OR opened_at >= ?2) AND (?3 IS NULL OR opened_at < ?3)
         ORDER BY {order} LIMIT ?4"
    );
    let rows = sqlx::query(&sql).bind(q.herd_id).bind(q.from.as_ref().map(to_db)).bind(q.to.as_ref().map(to_db)).bind(q.limit).fetch_all(ctx.db()).await?;
    rows.iter().map(alert_from_row).collect()
}

/// Unresolved alerts of one kind.
pub async fn unresolved(ctx: &Ctx, kind: &str) -> anyhow::Result<Vec<Row>> {
    let rows = sqlx::query("SELECT * FROM alerts WHERE kind = ? AND status != 'resolved'").bind(kind).fetch_all(ctx.db()).await?;
    rows.iter().map(row_from).collect()
}

/// The newest resolved alert for a key.
pub async fn last_resolved(ctx: &Ctx, key: &str) -> anyhow::Result<Option<Row>> {
    let row = sqlx::query("SELECT * FROM alerts WHERE key = ? AND status = 'resolved' ORDER BY resolved_at DESC, id DESC LIMIT 1")
        .bind(key)
        .fetch_optional(ctx.db())
        .await?;
    row.map(|r| row_from(&r)).transpose()
}

fn json(v: &impl serde::Serialize) -> anyhow::Result<String> {
    Ok(serde_json::to_string(v)?)
}

/// Insert a new alert. `false` when another unresolved row holds the key
/// (it was opened meanwhile).
pub async fn insert(ctx: &Ctx, r: &Row) -> anyhow::Result<bool> {
    let a = &r.alert;
    let res = sqlx::query(
        "INSERT INTO alerts (id, kind, key, severity, status, herd_id, title, body, at_lon, at_lat, targets, data, opened_at, updated_at,
                             seen_at, notify, batch_at)
         VALUES (?, ?, ?, ?, 'open', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT DO NOTHING",
    )
    .bind(&a.id)
    .bind(&a.kind)
    .bind(&a.key)
    .bind(a.severity.as_db())
    .bind(&a.herd_id)
    .bind(&a.title)
    .bind(&a.body)
    .bind(a.at.map(|p| p[0]))
    .bind(a.at.map(|p| p[1]))
    .bind(json(&a.targets)?)
    .bind(json(&a.data)?)
    .bind(to_db(&a.opened_at))
    .bind(to_db(&a.updated_at))
    .bind(to_db(&r.seen_at))
    .bind(r.notify)
    .bind(r.batch_at.as_ref().map(to_db))
    .execute(ctx.db())
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Store a changed title, place, severity, targets, body or data. Only while
/// unresolved. Returns the stored alert.
pub async fn update(ctx: &Ctx, a: &Alert, seen_at: DateTime<Utc>) -> anyhow::Result<Option<Alert>> {
    let row = sqlx::query(
        "UPDATE alerts SET severity = ?, title = ?, body = ?, at_lon = ?, at_lat = ?, targets = ?, data = ?, updated_at = ?, seen_at = ?
         WHERE id = ? AND status != 'resolved' RETURNING *",
    )
    .bind(a.severity.as_db())
    .bind(&a.title)
    .bind(&a.body)
    .bind(a.at.map(|p| p[0]))
    .bind(a.at.map(|p| p[1]))
    .bind(json(&a.targets)?)
    .bind(json(&a.data)?)
    .bind(to_db(&a.updated_at))
    .bind(to_db(&seen_at))
    .bind(&a.id)
    .fetch_optional(ctx.db())
    .await?;
    row.map(|r| alert_from_row(&r)).transpose()
}

/// Still there at `at`: nothing else changed.
pub async fn touch(ctx: &Ctx, ids: &[String], at: DateTime<Utc>) -> anyhow::Result<()> {
    for chunk in ids.chunks(500) {
        let marks = vec!["?"; chunk.len()].join(", ");
        let sql = format!("UPDATE alerts SET seen_at = ? WHERE id IN ({marks})");
        let mut q = sqlx::query(&sql).bind(to_db(&at));
        for id in chunk {
            q = q.bind(id);
        }
        q.execute(ctx.db()).await?;
    }
    Ok(())
}

/// Resolve an unresolved alert: by someone, by itself (`by` None) or into a
/// rollup. Publishes it. `None` when it was already resolved. Closed by
/// someone, its collars count as told (`announced`).
pub async fn resolve(ctx: &Ctx, id: &str, by: Option<&Actor>, rolled_into: Option<&str>, at: DateTime<Utc>) -> anyhow::Result<Option<Alert>> {
    let sql = format!(
        "UPDATE alerts SET status = 'resolved', resolved_at = ?1, resolved_by = ?2, rolled_into = ?3, updated_at = ?1,
             announced = CASE WHEN ?2 IS NULL THEN announced ELSE {TARGET_COLLARS} END
         WHERE id = ?4 AND status != 'resolved' RETURNING *"
    );
    let row = sqlx::query(&sql).bind(to_db(&at)).bind(by.map(json).transpose()?).bind(rolled_into).bind(id).fetch_optional(ctx.db()).await?;
    let a = row.map(|r| alert_from_row(&r)).transpose()?;
    if let Some(a) = &a {
        ctx.publish(Event::Alert { alert: public(a.clone()) });
    }
    Ok(a)
}

/// Someone saw it: no more re-notification or escalation, and its collars
/// count as told (`announced`). Acking an acked alert returns it unchanged;
/// a resolved one is 409.
pub async fn ack(ctx: &Ctx, id: &str, by: &Actor, at: DateTime<Utc>) -> ApiResult<Alert> {
    let sql = format!(
        "UPDATE alerts SET status = 'acked', acked_at = ?1, acked_by = ?2, updated_at = ?1, announced = {TARGET_COLLARS}
         WHERE id = ?3 AND status = 'open' RETURNING *"
    );
    let row = sqlx::query(&sql).bind(to_db(&at)).bind(json(by)?).bind(id).fetch_optional(ctx.db()).await?;
    if let Some(r) = row {
        let a = alert_from_row(&r)?;
        ctx.publish(Event::Alert { alert: public(a.clone()) });
        return Ok(a);
    }
    match get(ctx, id).await? {
        None => Err(ApiError::not_found("No such alert.")),
        Some(a) if a.status == AlertStatus::Resolved => Err(ApiError::conflict("This alert is resolved.")),
        Some(a) => Ok(a),
    }
}

/// Close it by hand. While its condition lasts it stays closed; once the
/// condition clears and comes back it opens a new alert.
pub async fn resolve_by(ctx: &Ctx, id: &str, by: &Actor, at: DateTime<Utc>) -> ApiResult<Alert> {
    if let Some(a) = resolve(ctx, id, Some(by), None, at).await? {
        return Ok(a);
    }
    match get(ctx, id).await? {
        None => Err(ApiError::not_found("No such alert.")),
        Some(_) => Err(ApiError::conflict("This alert is resolved.")),
    }
}

pub fn data_str<'a>(a: &'a Alert, key: &str) -> Option<&'a str> {
    a.data.get(key).and_then(Value::as_str)
}
