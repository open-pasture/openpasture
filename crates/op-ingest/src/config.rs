//! Collar configuration (protocol v1 §3.4): each collar's signed config
//! command, carried in the reply to its report.
//!
//! A collar's config is worked out from the farm's state: its herd, the
//! endpoint (the collar address, `server.collar_url`), the report and poll
//! cadence from the `collars.config` setting, and a fast window while its
//! herd's move sweeps or its own escape runs. When that differs from the
//! stored one it gets a new version: on a herd change, a collar address change,
//! a move or escape starting (and running on past its fast window), and a
//! change to `collars.config`. The routes that cause those refresh the
//! affected collars at once; every report from a collar with the `config`
//! cap checks again, so nothing is missed (a collar address change shows up on
//! the next report).
//!
//! A collar reports the version it holds (`device.config_version`) and gets
//! the current one while it is lower. One it refused (`device.config_reject`)
//! isn't sent again; the next change makes a new version.

use axum::extract::State;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use op_core::time::{now, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, id};
use op_protocol::config::{MAX_ENDPOINT_BYTES, MAX_INTERVAL_S, MIN_INTERVAL_S};
use op_protocol::{ConfigCommand, ConfigReject, caps, sign_config, wire_time};
use serde::{Deserialize, Serialize};
use sqlx::Row;

/// Id prefix of config commands.
pub const CONFIG: &str = "cfg";
/// Settings key of [`CollarsConfig`].
pub const KEY: &str = "collars.config";

/// Assumed pace of a sweep's back line, for the fast window.
const PACE_M_PER_MIN: f64 = 4.0;
/// The fast window covers the estimated end plus this.
const FAST_TAIL: Duration = Duration::minutes(10);
/// Estimates are kept between these, and extended while the move runs.
const FAST_MIN: Duration = Duration::minutes(5);
const FAST_MAX: Duration = Duration::minutes(30);
/// A running move or escape extends the window once less than this is left.
const EXTEND_LEFT: Duration = Duration::minutes(5);

/// Report and poll cadence, seconds (setting `collars.config`). The fast
/// pair applies while the collar's herd is being moved or it is out on an escape.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct CollarsConfig {
    pub report_s: u32,
    pub poll_s: u32,
    pub fast_report_s: u32,
    pub fast_poll_s: u32,
}

impl Default for CollarsConfig {
    fn default() -> Self {
        Self { report_s: 60, poll_s: 60, fast_report_s: 10, fast_poll_s: 10 }
    }
}

impl CollarsConfig {
    fn validate(&self) -> ApiResult<()> {
        for (v, name) in [(self.report_s, "report_s"), (self.poll_s, "poll_s"), (self.fast_report_s, "fast_report_s"), (self.fast_poll_s, "fast_poll_s")] {
            if !(MIN_INTERVAL_S..=MAX_INTERVAL_S).contains(&v) {
                return Err(ApiError::bad_request(format!("{name} must be between {MIN_INTERVAL_S} and {MAX_INTERVAL_S} seconds.")));
            }
        }
        Ok(())
    }
}

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/collars/config", get(get_config).put(put_config))
}

pub async fn load(ctx: &Ctx) -> anyhow::Result<CollarsConfig> {
    Ok(ctx.store().get_setting::<CollarsConfig>(KEY).await?.unwrap_or_default())
}

async fn get_config(State(ctx): State<Ctx>) -> ApiResult<Json<CollarsConfig>> {
    Ok(Json(load(&ctx).await?))
}

/// Save the cadence; every collar that takes configs gets a new version
/// (now, or on its next report if that fails).
async fn put_config(State(ctx): State<Ctx>, ApiJson(body): ApiJson<CollarsConfig>) -> ApiResult<Json<CollarsConfig>> {
    body.validate()?;
    ctx.store().set_setting(KEY, &body).await?;
    refresh_quietly(&ctx, Scope::All).await;
    Ok(Json(body))
}

/// Why a collar gets no endpoint: there is no collar address yet.
pub const NO_COLLAR_ADDRESS: &str =
    "Collars need an https address that reaches this server, and this one has none yet. Set the collar address in Settings, Server.";

/// Where collars reach this server: the collar address (`server.collar_url`)
/// plus `/collar/v1`. Never this machine's own URL: a collar can't reach
/// loopback and talks only to https. `None` until a collar address is set
/// (configs then keep the endpoint a collar has; links and cards say why,
/// [`NO_COLLAR_ADDRESS`]).
pub async fn collar_endpoint(ctx: &Ctx) -> anyhow::Result<Option<String>> {
    let url = ctx.settings().await?.server.collar_url;
    Ok(url
        .filter(|u| op_core::settings::collar_url_problem(u).is_none())
        .map(|u| format!("{}/collar/v1", u.trim_end_matches('/')))
        .filter(|e| e.len() <= MAX_ENDPOINT_BYTES))
}

/// A new collar address goes into every collar that takes configs, now (or
/// on its next report if that fails).
pub async fn collar_address_changed(ctx: &Ctx) {
    refresh_quietly(ctx, Scope::All).await;
}

/// How long a collar tries a new endpoint before it goes back to its old one
/// (firmware `CONFIG_ENDPOINT_FALLBACK_S`).
pub const ENDPOINT_FALLBACK: chrono::Duration = chrono::Duration::hours(24);

/// A collar reporting that it holds its current config (`reported` at or
/// above its version) has moved to that config's endpoint: the report came
/// through it. Unless the config went out a day or more ago: by then the
/// collar may have given up on it and be back on its old one.
pub(crate) async fn confirm_endpoint(conn: &mut sqlx::SqliteConnection, collar_id: &str, reported: Option<u32>, at: DateTime<Utc>) -> anyhow::Result<()> {
    let Some(r) = reported else { return Ok(()) };
    sqlx::query(
        "UPDATE collars SET endpoint = (SELECT json_extract(body, '$.endpoint') FROM collar_config WHERE collar_id = ?1)
         WHERE id = ?1 AND EXISTS (
             SELECT 1 FROM collar_config WHERE collar_id = ?1 AND version <= ?2 AND json_extract(body, '$.endpoint') IS NOT NULL
                 AND (sent_at IS NULL OR sent_at > ?3)
                 AND json_extract(body, '$.endpoint') IS NOT collars.endpoint)",
    )
    .bind(collar_id)
    .bind(i64::from(r))
    .bind(to_db(&(at - ENDPOINT_FALLBACK)))
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// Collars not yet on the current collar endpoint, by the endpoint they are
/// known to use (`None`: not known yet).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Moving {
    pub endpoint: Option<String>,
    pub collars: u32,
}

/// Where collars stand on the collar address: those that take configs and
/// haven't moved to the current one yet, and those that take no configs and
/// so keep the address they were set up with for good.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct AddressStatus {
    pub moving: Vec<Moving>,
    pub fixed: u32,
}

pub async fn address_status(ctx: &Ctx) -> anyhow::Result<AddressStatus> {
    let current = collar_endpoint(ctx).await?;
    let takes = "caps IS NOT NULL AND EXISTS (SELECT 1 FROM json_each(collars.caps) WHERE value = 'config')";
    let rows: Vec<(Option<String>, i64)> = sqlx::query_as(&format!(
        "SELECT endpoint, COUNT(*) FROM collars WHERE {takes} AND endpoint IS NOT ? GROUP BY endpoint ORDER BY COUNT(*) DESC, endpoint"
    ))
    .bind(&current)
    .fetch_all(ctx.db())
    .await?;
    let fixed: i64 = sqlx::query_scalar(&format!("SELECT COUNT(*) FROM collars WHERE caps IS NOT NULL AND NOT ({takes})")).fetch_one(ctx.db()).await?;
    Ok(AddressStatus {
        // With no collar address, nobody is moving anywhere.
        moving: if current.is_some() { rows.into_iter().map(|(endpoint, n)| Moving { endpoint, collars: n as u32 }).collect() } else { vec![] },
        fixed: fixed as u32,
    })
}

/// When the fast window should reach at least, for something `remaining_m`
/// from done: the estimated end (kept between 5 and 30 minutes) plus 10.
fn fast_until_for(remaining_m: f64, at: DateTime<Utc>) -> DateTime<Utc> {
    let est = Duration::seconds((remaining_m.max(0.0) / PACE_M_PER_MIN * 60.0) as i64).clamp(FAST_MIN, FAST_MAX);
    at + est + FAST_TAIL
}

/// The config a collar should hold now, before versions and signatures.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Desired {
    pub collar_id: String,
    pub herd_id: String,
    pub endpoint: Option<String>,
    pub cadence: CollarsConfig,
    /// Fast until at least this (a running move or escape).
    pub fast_until: Option<DateTime<Utc>>,
}

/// The next config body, or `None` when the stored one still holds. `reported`
/// is the version the collar says it has: one above ours (our database is
/// older than the collar's config) is overtaken by a new version.
///
/// The command policy (Astra 02 F11): a config goes out only with a cadence
/// that passes [`CollarsConfig::validate`], and a collar being moved or
/// walked back (a fast window wanted) keeps the endpoint it has until that is
/// over: a new collar address never lands mid-move or mid-escape, where
/// losing contact matters most.
pub(crate) fn next_body(stored: Option<&ConfigCommand>, reported: Option<u32>, d: &Desired, at: DateTime<Utc>) -> Option<ConfigCommand> {
    let c = &d.cadence;
    if let Err(e) = c.validate() {
        tracing::error!(collar = %d.collar_id, "collar config not issued, its cadence is out of bounds: {}", e.message);
        return None;
    }
    let endpoint = match stored {
        Some(s) if d.fast_until.is_some() && s.endpoint.is_some() && s.endpoint != d.endpoint => {
            tracing::info!(collar = %d.collar_id, "new collar address held back until its move or escape is over");
            s.endpoint.clone()
        }
        _ => d.endpoint.clone(),
    };
    let fast_running = |s: &ConfigCommand| s.fast_until.is_some_and(|t| t > at) && s.fast_report_s.is_some() && s.fast_poll_s.is_some();
    let due = match stored {
        None => true,
        Some(s) => {
            s.collar_id != d.collar_id
                || s.herd_id.as_deref() != Some(d.herd_id.as_str())
                || s.endpoint != endpoint
                || (s.report_s, s.poll_s) != (c.report_s, c.poll_s)
                || reported.is_some_and(|r| r > s.version)
                || (d.fast_until.is_some()
                    && (s.fast_until.is_none_or(|t| t < at + EXTEND_LEFT) || (s.fast_report_s, s.fast_poll_s) != (Some(c.fast_report_s), Some(c.fast_poll_s))))
        }
    };
    if !due {
        return None;
    }
    let running = stored.filter(|s| fast_running(s));
    let (fast_report_s, fast_poll_s, fast_until) = match (d.fast_until, running) {
        (Some(t), r) => {
            let until = r.and_then(|s| s.fast_until).map_or(t, |u| u.max(t));
            (Some(c.fast_report_s), Some(c.fast_poll_s), Some(wire_time::trunc_secs(until)))
        }
        // A fast window already running is left to run out on its own.
        (None, Some(s)) => (s.fast_report_s, s.fast_poll_s, s.fast_until),
        (None, None) => (None, None, None),
    };
    Some(ConfigCommand {
        command_id: String::new(),
        collar_id: d.collar_id.clone(),
        version: stored.map_or(0, |s| s.version).max(reported.unwrap_or(0)) + 1,
        herd_id: Some(d.herd_id.clone()),
        endpoint,
        report_s: c.report_s,
        poll_s: c.poll_s,
        fast_report_s,
        fast_poll_s,
        fast_until,
        sig: None,
    })
}

/// A collar's stored config and the version it refused, if any.
pub(crate) struct Stored {
    pub cmd: ConfigCommand,
    pub reject_version: Option<u32>,
}

pub(crate) async fn stored(conn: &mut sqlx::SqliteConnection, collar_id: &str) -> anyhow::Result<Option<Stored>> {
    let row = sqlx::query("SELECT body, reject_version FROM collar_config WHERE collar_id = ?").bind(collar_id).fetch_optional(&mut *conn).await?;
    row.map(|r| {
        Ok(Stored {
            cmd: serde_json::from_str(&r.try_get::<String, _>("body")?)?,
            reject_version: r.try_get::<Option<i64>, _>("reject_version")?.map(|v| v as u32),
        })
    })
    .transpose()
}

/// Bring one collar's stored config up to `d` inside the caller's write
/// transaction, note a refusal, and return the config to send a collar that
/// holds `reported` (`None` when it is current or refused this version).
pub(crate) async fn sync(
    conn: &mut sqlx::SqliteConnection,
    ctx: &Ctx,
    d: &Desired,
    reported: Option<u32>,
    reject: Option<&ConfigReject>,
    at: DateTime<Utc>,
) -> anyhow::Result<Option<ConfigCommand>> {
    let mut current = stored(conn, &d.collar_id).await?;
    if let (Some(r), Some(s)) = (reject, current.as_mut())
        && s.reject_version != Some(r.version)
    {
        sqlx::query("UPDATE collar_config SET reject_version = ?, reject_code = ? WHERE collar_id = ?")
            .bind(r.version as i64)
            .bind(r.code.map(|c| c.as_str()))
            .bind(&d.collar_id)
            .execute(&mut *conn)
            .await?;
        tracing::info!(collar = %d.collar_id, version = r.version, code = r.code.map(|c| c.as_str()), "config refused");
        s.reject_version = Some(r.version);
    }
    if let Some(mut next) = next_body(current.as_ref().map(|s| &s.cmd), reported, d, at) {
        next.command_id = id::new_id(CONFIG);
        sign_config(&mut next, ctx.signing_key());
        sqlx::query(
            "INSERT INTO collar_config (collar_id, version, body, updated_at, reject_version, reject_code) VALUES (?, ?, ?, ?, NULL, NULL)
             ON CONFLICT(collar_id) DO UPDATE SET version = excluded.version, body = excluded.body, updated_at = excluded.updated_at,
                 reject_version = NULL, reject_code = NULL",
        )
        .bind(&d.collar_id)
        .bind(next.version as i64)
        .bind(serde_json::to_string(&next)?)
        .bind(to_db(&at))
        .execute(&mut *conn)
        .await?;
        tracing::info!(collar = %d.collar_id, version = next.version, herd = ?next.herd_id, endpoint = ?next.endpoint, fast_until = ?next.fast_until, "collar config");
        current = Some(Stored { cmd: next, reject_version: None });
    }
    Ok(current.filter(|s| reported.is_none_or(|r| s.cmd.version > r) && s.reject_version != Some(s.cmd.version)).map(|s| s.cmd))
}

/// The collar's current config goes out in a report reply now.
pub(crate) async fn mark_sent(conn: &mut sqlx::SqliteConnection, collar_id: &str, at: DateTime<Utc>) -> anyhow::Result<()> {
    sqlx::query("UPDATE collar_config SET sent_at = ? WHERE collar_id = ?").bind(to_db(&at)).bind(collar_id).execute(&mut *conn).await?;
    Ok(())
}

/// A collar that says it holds config version `reported` holds our current
/// one: its `wrong_herd` and `bad_sig` refusals from before that config last
/// went out came from before it checked our signature on it and knew its
/// herd, so they go (and those versions are offered again). Returns them.
pub(crate) async fn clear_stale_refusals(conn: &mut sqlx::SqliteConnection, collar_id: &str, reported: Option<u32>) -> anyhow::Result<Vec<u32>> {
    let Some(r) = reported else { return Ok(vec![]) };
    let gone: Vec<i64> = sqlx::query_scalar(
        "DELETE FROM collar_slots WHERE collar_id = ?1 AND status = 'rejected' AND code IN ('wrong_herd', 'bad_sig')
             AND reported_at <= (SELECT sent_at FROM collar_config WHERE collar_id = ?1 AND version <= ?2)
         RETURNING version",
    )
    .bind(collar_id)
    .bind(i64::from(r))
    .fetch_all(&mut *conn)
    .await?;
    Ok(gone.into_iter().map(|v| v as u32).collect())
}

/// What a set of collars should hold: shared inputs read once.
pub(crate) struct Inputs {
    cadence: CollarsConfig,
    endpoint: Option<String>,
}

impl Inputs {
    pub async fn read(ctx: &Ctx) -> anyhow::Result<Self> {
        Ok(Self { cadence: load(ctx).await?, endpoint: collar_endpoint(ctx).await? })
    }

    /// `d` for one collar, given its herd's running move and its own open escape.
    fn desired(&self, collar_id: &str, herd_id: &str, move_left_m: Option<f64>, escape_left_m: Option<f64>, at: DateTime<Utc>) -> Desired {
        let fast_until = [move_left_m, escape_left_m].into_iter().flatten().map(|m| fast_until_for(m, at)).max();
        Desired { collar_id: collar_id.to_owned(), herd_id: herd_id.to_owned(), endpoint: self.endpoint.clone(), cadence: self.cadence, fast_until }
    }
}

/// `d` for one collar, read from the database (before a write lock).
pub(crate) async fn desired(ctx: &Ctx, collar_id: &str, herd_id: &str, at: DateTime<Utc>) -> anyhow::Result<Desired> {
    let inputs = Inputs::read(ctx).await?;
    let mv: Option<(f64,)> =
        sqlx::query_as("SELECT remaining_m FROM moves WHERE herd_id = ? AND status = 'sweeping'").bind(herd_id).fetch_optional(ctx.db()).await?;
    let esc: Option<(f64,)> = sqlx::query_as("SELECT remaining_m FROM escapes WHERE collar_id = ? AND status = 'returning' AND herd_id = ?")
        .bind(collar_id)
        .bind(herd_id)
        .fetch_optional(ctx.db())
        .await?;
    Ok(inputs.desired(collar_id, herd_id, mv.map(|m| m.0), esc.map(|e| e.0), at))
}

/// Which collars to refresh.
pub(crate) enum Scope<'a> {
    All,
    Herd(&'a str),
    Collar(&'a str),
}

/// Bring the stored config of every collar in `scope` that takes configs up
/// to date, in one write transaction. Collars pick it up on their next report.
pub(crate) async fn refresh(ctx: &Ctx, scope: Scope<'_>) -> anyhow::Result<()> {
    let at = now();
    let rows = match scope {
        Scope::All => sqlx::query("SELECT id, herd_id, caps FROM collars WHERE caps IS NOT NULL").fetch_all(ctx.db()).await?,
        Scope::Herd(h) => sqlx::query("SELECT id, herd_id, caps FROM collars WHERE herd_id = ? AND caps IS NOT NULL").bind(h).fetch_all(ctx.db()).await?,
        Scope::Collar(c) => sqlx::query("SELECT id, herd_id, caps FROM collars WHERE id = ? AND caps IS NOT NULL").bind(c).fetch_all(ctx.db()).await?,
    };
    let mut targets = Vec::new();
    for r in &rows {
        let caps: Vec<String> = serde_json::from_str(&r.try_get::<String, _>("caps")?).unwrap_or_default();
        if caps.iter().any(|c| c == caps::CONFIG) {
            targets.push((r.try_get::<String, _>("id")?, r.try_get::<String, _>("herd_id")?));
        }
    }
    if targets.is_empty() {
        return Ok(());
    }
    let inputs = Inputs::read(ctx).await?;
    let moves: Vec<(String, f64)> = sqlx::query_as("SELECT herd_id, remaining_m FROM moves WHERE status = 'sweeping'").fetch_all(ctx.db()).await?;
    let escapes: Vec<(String, String, f64)> =
        sqlx::query_as("SELECT collar_id, herd_id, remaining_m FROM escapes WHERE status = 'returning'").fetch_all(ctx.db()).await?;
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    for (collar_id, herd_id) in &targets {
        let mv = moves.iter().find(|m| &m.0 == herd_id).map(|m| m.1);
        let esc = escapes.iter().find(|e| &e.0 == collar_id && &e.1 == herd_id).map(|e| e.2);
        let d = inputs.desired(collar_id, herd_id, mv, esc, at);
        sync(&mut tx, ctx, &d, None, None, at).await?;
    }
    tx.commit().await?;
    Ok(())
}

/// [`refresh`], logging instead of failing: the caller's own work is done,
/// and the next report catches up anyway.
pub(crate) async fn refresh_quietly(ctx: &Ctx, scope: Scope<'_>) {
    if let Err(e) = refresh(ctx, scope).await {
        tracing::warn!("collar config: {e:#}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t0() -> DateTime<Utc> {
        wire_time::parse("2026-09-27T12:00:00Z").unwrap()
    }
    fn want(fast: Option<DateTime<Utc>>) -> Desired {
        Desired { collar_id: "col_a".into(), herd_id: "herd_1".into(), endpoint: None, cadence: CollarsConfig::default(), fast_until: fast }
    }

    /// Astra 02 F11: the command policy keeps a collar's endpoint through a
    /// move or escape, and issues nothing with a cadence out of bounds.
    #[test]
    fn a_new_address_waits_for_the_move_and_a_bad_cadence_goes_nowhere() {
        let old = Desired { endpoint: Some("https://old.example/collar/v1".into()), ..want(None) };
        let first = next_body(None, None, &old, t0()).unwrap();
        let moved = Desired { endpoint: Some("https://new.example/collar/v1".into()), ..want(Some(t0() + Duration::minutes(20))) };
        let during = next_body(Some(&first), Some(first.version), &moved, t0()).unwrap();
        assert_eq!(during.endpoint.as_deref(), Some("https://old.example/collar/v1"), "kept through the move");
        assert!(during.fast_until.is_some());
        assert!(next_body(Some(&during), Some(during.version), &moved, t0()).is_none(), "no churn while it waits");
        let after = Desired { fast_until: None, ..moved.clone() };
        assert_eq!(next_body(Some(&during), Some(during.version), &after, t0()).unwrap().endpoint.as_deref(), Some("https://new.example/collar/v1"));
        let mut bad = want(None);
        bad.cadence.report_s = 0;
        assert!(next_body(None, None, &bad, t0()).is_none());
    }

    #[test]
    fn first_config_then_only_changes() {
        let first = next_body(None, None, &want(None), t0()).unwrap();
        assert_eq!((first.version, first.report_s, first.fast_until), (1, 60, None));
        assert!(ConfigCommand { command_id: "cfg_1".into(), ..first.clone() }.check("col_a", None).is_ok());
        assert_eq!(next_body(Some(&first), Some(1), &want(None), t0()), None, "nothing changed");
        let moved = Desired { herd_id: "herd_2".into(), ..want(None) };
        assert_eq!(next_body(Some(&first), Some(1), &moved, t0()).unwrap().version, 2);
        let url = Desired { endpoint: Some("https://farm.example.com/collar/v1".into()), ..want(None) };
        assert_eq!(next_body(Some(&first), Some(1), &url, t0()).unwrap().endpoint.as_deref(), Some("https://farm.example.com/collar/v1"));
        let slower = Desired { cadence: CollarsConfig { report_s: 120, ..Default::default() }, ..want(None) };
        assert_eq!(next_body(Some(&first), Some(1), &slower, t0()).unwrap().report_s, 120);
    }

    #[test]
    fn a_collar_ahead_of_us_gets_a_version_above_its_own() {
        let first = next_body(None, None, &want(None), t0()).unwrap();
        assert_eq!(next_body(Some(&first), Some(7), &want(None), t0()).unwrap().version, 8);
        assert_eq!(next_body(None, Some(3), &want(None), t0()).unwrap().version, 4);
    }

    #[test]
    fn a_move_opens_a_fast_window_and_extends_it_while_it_runs() {
        let base = next_body(None, None, &want(None), t0()).unwrap();
        // 200 m to go: 50 min, capped at 30, plus 10.
        let need = fast_until_for(200.0, t0());
        assert_eq!(need, t0() + Duration::minutes(40));
        assert_eq!(fast_until_for(3.0, t0()), t0() + Duration::minutes(15), "at least 5 + 10");
        let fast = next_body(Some(&base), Some(1), &want(Some(need)), t0()).unwrap();
        assert_eq!((fast.version, fast.fast_report_s, fast.fast_poll_s, fast.fast_until), (2, Some(10), Some(10), Some(need)));
        assert!(ConfigCommand { command_id: "cfg_2".into(), ..fast.clone() }.check("col_a", Some(1)).is_ok());
        // Still running 20 min later: plenty left, no new version.
        let later = t0() + Duration::minutes(20);
        assert_eq!(next_body(Some(&fast), Some(2), &want(Some(fast_until_for(100.0, later))), later), None);
        // 36 min in, 4 min left: extended.
        let late = t0() + Duration::minutes(36);
        let ext = next_body(Some(&fast), Some(2), &want(Some(fast_until_for(100.0, late))), late).unwrap();
        assert!(ext.fast_until.unwrap() > fast.fast_until.unwrap());
        // The move ended: the window runs out by itself, no version for that.
        assert_eq!(next_body(Some(&ext), Some(3), &want(None), late), None);
        // Another change meanwhile keeps the running window.
        let moved = Desired { herd_id: "herd_2".into(), ..want(None) };
        assert_eq!(next_body(Some(&ext), Some(3), &moved, late).unwrap().fast_until, ext.fast_until);
        // Once it is over, a later change drops it.
        let after = ext.fast_until.unwrap() + Duration::seconds(1);
        assert_eq!(next_body(Some(&ext), Some(3), &moved, after).unwrap().fast_until, None);
    }

    #[test]
    fn cadence_limits_are_checked() {
        assert!(CollarsConfig::default().validate().is_ok());
        assert!(CollarsConfig { report_s: 9, ..Default::default() }.validate().is_err());
        assert!(CollarsConfig { fast_poll_s: 3601, ..Default::default() }.validate().is_err());
    }
}
