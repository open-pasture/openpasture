//! Per-person alert preferences (`alert_prefs`).

use chrono::{DateTime, Utc};
use op_core::time::{from_db, now, to_db};
use op_core::users::User;
use op_core::{ApiError, ApiResult, Ctx, DbEnum, Role, Severity};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::engine::config::valid_hhmm;

/// Channels a person can choose (the farm webhook is farm-level). `push`
/// reaches every browser they turned alerts on in.
pub const PERSON_CHANNELS: [&str; 4] = ["sms", "whatsapp", "email", "push"];

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Prefs {
    /// Of `sms`, `whatsapp`, `email`, `push`.
    pub channels: Vec<String>,
    pub min_severity: Severity,
    /// Only these herds; absent = every herd.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herds: Option<Vec<String>>,
    #[serde(default)]
    pub muted_kinds: Vec<String>,
    /// Farm time HH:MM; absent = the farm's quiet hours.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_start: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quiet_end: Option<String>,
    /// Critical alerts still come in quiet hours.
    pub critical_in_quiet: bool,
    /// When anyone matching is on duty, first sends go only to them.
    pub on_duty: bool,
}

impl Default for Prefs {
    fn default() -> Self {
        Self {
            channels: vec!["sms".into()],
            min_severity: Severity::Warning,
            herds: None,
            muted_kinds: vec![],
            quiet_start: None,
            quiet_end: None,
            critical_in_quiet: true,
            on_duty: false,
        }
    }
}

/// A person with their prefs, for `GET /api/alerts/prefs`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PersonPrefs {
    pub user_id: String,
    pub name: String,
    pub role: Role,
    #[serde(flatten)]
    pub prefs: Prefs,
    /// Texted STOP to the farm's number: gets no texts until START.
    pub sms_opt_out: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
}

fn json_list(v: Option<String>) -> anyhow::Result<Option<Vec<String>>> {
    Ok(v.map(|s| serde_json::from_str(&s)).transpose()?)
}

fn from_row(r: &SqliteRow) -> anyhow::Result<(Prefs, bool, DateTime<Utc>)> {
    Ok((
        Prefs {
            channels: json_list(r.try_get("channels")?)?.unwrap_or_default(),
            min_severity: Severity::from_db(&r.try_get::<String, _>("min_severity")?)?,
            herds: json_list(r.try_get("herds")?)?,
            muted_kinds: json_list(r.try_get("muted_kinds")?)?.unwrap_or_default(),
            quiet_start: r.try_get("quiet_start")?,
            quiet_end: r.try_get("quiet_end")?,
            critical_in_quiet: r.try_get("critical_in_quiet")?,
            on_duty: r.try_get("on_duty")?,
        },
        r.try_get("sms_opt_out")?,
        from_db(&r.try_get::<String, _>("updated_at")?)?,
    ))
}

/// A person's prefs (the defaults without a row) and whether they opted out of texts.
pub async fn get(ctx: &Ctx, user_id: &str) -> anyhow::Result<(Prefs, bool)> {
    let row = sqlx::query("SELECT * FROM alert_prefs WHERE user_id = ?").bind(user_id).fetch_optional(ctx.db()).await?;
    Ok(match row {
        Some(r) => {
            let (p, o, _) = from_row(&r)?;
            (p, o)
        }
        None => (Prefs::default(), false),
    })
}

/// Everyone's prefs, enabled people first.
pub async fn all(ctx: &Ctx) -> anyhow::Result<Vec<PersonPrefs>> {
    let users = op_core::users::list_users(ctx).await?;
    let rows = sqlx::query("SELECT * FROM alert_prefs").fetch_all(ctx.db()).await?;
    let mut stored = std::collections::HashMap::new();
    for r in &rows {
        stored.insert(r.try_get::<String, _>("user_id")?, from_row(r)?);
    }
    Ok(users
        .into_iter()
        .map(|u: User| {
            let (prefs, sms_opt_out, updated_at) = match stored.remove(&u.id) {
                Some((p, o, t)) => (p, o, Some(t)),
                None => (Prefs::default(), false, None),
            };
            PersonPrefs { user_id: u.id, name: u.name, role: u.role, prefs, sms_opt_out, updated_at }
        })
        .collect())
}

/// Change a person's prefs with a JSON merge patch; `null` clears `herds`
/// (every herd) and the quiet hours (the farm's).
pub async fn put(ctx: &Ctx, user_id: &str, patch: &Value) -> ApiResult<Prefs> {
    if op_core::users::get_user(ctx, user_id).await?.is_none() {
        return Err(ApiError::not_found("No such person."));
    }
    let (current, _) = get(ctx, user_id).await?;
    let mut next: Prefs = op_core::patch::apply(&current, patch, &[])?;
    check(ctx, &mut next).await?;
    sqlx::query(
        "INSERT INTO alert_prefs (user_id, channels, min_severity, herds, muted_kinds, quiet_start, quiet_end, critical_in_quiet, on_duty, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(user_id) DO UPDATE SET channels = excluded.channels, min_severity = excluded.min_severity, herds = excluded.herds,
             muted_kinds = excluded.muted_kinds, quiet_start = excluded.quiet_start, quiet_end = excluded.quiet_end,
             critical_in_quiet = excluded.critical_in_quiet, on_duty = excluded.on_duty, updated_at = excluded.updated_at",
    )
    .bind(user_id)
    .bind(serde_json::to_string(&next.channels).map_err(anyhow::Error::from)?)
    .bind(next.min_severity.as_db())
    .bind(next.herds.as_ref().map(serde_json::to_string).transpose().map_err(anyhow::Error::from)?)
    .bind(serde_json::to_string(&next.muted_kinds).map_err(anyhow::Error::from)?)
    .bind(&next.quiet_start)
    .bind(&next.quiet_end)
    .bind(next.critical_in_quiet)
    .bind(next.on_duty)
    .bind(to_db(&now()))
    .execute(ctx.db())
    .await?;
    Ok(next)
}

async fn check(ctx: &Ctx, p: &mut Prefs) -> ApiResult<()> {
    let mut seen = Vec::new();
    for c in &p.channels {
        if !PERSON_CHANNELS.contains(&c.as_str()) {
            return Err(ApiError::bad_request(format!("Channels are sms, whatsapp, email or push, not {c}.")));
        }
        if !seen.contains(c) {
            seen.push(c.clone());
        }
    }
    p.channels = seen;
    if let Some(herds) = &p.herds {
        let known = ctx.store().list_herds().await?;
        if let Some(h) = herds.iter().find(|h| !known.iter().any(|k| &k.id == *h)) {
            return Err(ApiError::bad_request(format!("There is no herd {h}.")));
        }
    }
    let kinds: Vec<&str> = crate::rules::rules().iter().map(|r| r.descriptor().kind).collect();
    if let Some(k) = p.muted_kinds.iter().find(|k| !kinds.contains(&k.as_str())) {
        return Err(ApiError::bad_request(format!("There is no alert kind {k}.")));
    }
    match (&p.quiet_start, &p.quiet_end) {
        (None, None) => Ok(()),
        (Some(a), Some(b)) if valid_hhmm(a) && valid_hhmm(b) => Ok(()),
        _ => Err(ApiError::bad_request("Quiet hours need a start and an end, as HH:MM.")),
    }
}

/// Mirror a STOP (`true`) or START (`false`) texted from a person's phone.
pub async fn set_sms_opt_out(ctx: &Ctx, user_id: &str, opt_out: bool) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO alert_prefs (user_id, sms_opt_out, updated_at) VALUES (?, ?, ?)
         ON CONFLICT(user_id) DO UPDATE SET sms_opt_out = excluded.sms_opt_out, updated_at = excluded.updated_at",
    )
    .bind(user_id)
    .bind(opt_out)
    .bind(to_db(&now()))
    .execute(ctx.db())
    .await?;
    Ok(())
}
