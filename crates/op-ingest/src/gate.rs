//! The safety gate: one check every herd boundary passes before it can reach
//! the collars, whoever sends it (docs/API.md "Safety gate").
//!
//! **Hard findings** are the critical ones [`crate::prepare`] finds: a road
//! (`crosses_road`) or the farm boundary (`crosses_farm_boundary`) crossed or
//! touched, a no-go zone (`enters_no_go`) or a hazard marked hard
//! (`overlaps_hazard`) taken in or touched, all of it in an exclusion
//! (`overlaps_exclusion`), and no water inside when the farm requires it
//! (`no_water`, also when no water is mapped). A shape that can't be sent at
//! all (self-crossing, too small, nowhere clear of the warning zone) never
//! gets this far: `prepare` refuses it.
//!
//! **The exact shapes.** The gate judges every shape a collar would enforce:
//! the stored one and its fit to each kind of collar in the herd (a collar
//! without holes enforces the outer ring alone, fitted to its corners). The
//! download checks the command for that one collar again
//! ([`deliverable`]), against the map as it is then.
//!
//! **Who may go past one.** Nobody may send a boundary into a no-go zone. A
//! person, signed in (an owner session or a person's own token, not the app
//! token, an MCP agent or a station), may send one with any other hard
//! finding by naming each code in `confirm` together with the
//! `confirm_token` the refusal gave: it binds the confirmation to that exact
//! shape and map revision. Every other sender (the app token, the brain, an
//! MCP agent, the timer, auto, a strip schedule, a sweep step, an escape pen,
//! the server) is refused on any hard finding. Copies of a stored boundary
//! (for one collar, a reissue of the same shape) keep what was confirmed for
//! it; a new hard finding refuses them. Sweep steps and escape pens are
//! passing shapes: water isn't asked of them.
//!
//! **One way in.** A boundary is stored only by [`insert`], with the
//! [`Authorized`] [`pass`] gave for that exact shape. Map changes bump the
//! policy revision (`gate_policy`, kept by triggers in the same
//! transaction); `insert` judges the shape again inside its write
//! transaction when the revision moved since the check.
//!
//! Every check is logged, allowed or refused, with the stage, sender,
//! credential, the hard findings, what was confirmed, the shape's hash and
//! the revision; the boundary it became is linked in the transaction that
//! stored it (`GET /api/gate/log`).

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::check::Finding;
use op_core::features::{FeatureKind, MapFeature};
use op_core::time::{from_db, now, to_db};
use op_core::{Actor, ApiError, ApiJson, ApiResult, Boundary, Ctx, DbEnum, Identity, Polygon, Severity, Via, id};
use op_geo::CollarLimits;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::{Row, SqliteConnection};

use crate::prepare::{Local, MapRules, map_findings};
use crate::shape::CollarCaps;

/// `gat_…`
pub const GATE: &str = "gat";
/// Hard codes nobody may go past.
pub const NEVER: [&str; 1] = ["enters_no_go"];
/// Where the gate's settings are kept (the settings table).
const SETTINGS_KEY: &str = "gate";
/// Codes the map findings make: judged afresh on every exact shape.
const MAP_CODES: [&str; 7] = ["enters_no_go", "overlaps_hazard", "crosses_road", "crosses_neighbour_line", "crosses_farm_boundary", "water_inside", "no_water"];

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/gate/log", get(get_log)).route("/api/gate/settings", get(get_settings).put(put_settings))
}

/// Who is sending a boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SenderKind {
    /// A person, signed in: the app's owner session or their own token.
    Farmer,
    /// A REST client with the app token: a program, not a person.
    Api,
    /// A decision an MCP agent proposed.
    Mcp,
    /// A decision the farm's brain made.
    Brain,
    /// A proposal applied when its timer ran out.
    Timer,
    /// A proposal applied at once (autonomy auto).
    Auto,
    /// A strip schedule's move.
    Schedule,
    /// A step of a move's sweep.
    Sweep,
    /// An escaped animal's pen.
    Escape,
    /// The server itself (a reissue).
    #[default]
    System,
}

impl DbEnum for SenderKind {}

impl SenderKind {
    /// A person, who may confirm hard findings.
    pub fn is_person(self) -> bool {
        matches!(self, Self::Farmer)
    }

    /// Shapes on the way somewhere: water isn't asked of them.
    fn passing(self) -> bool {
        matches!(self, Self::Sweep | Self::Escape)
    }
}

/// What a gate row was.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Before a boundary is stored.
    #[default]
    Send,
    /// A person's approval or change, before anything is claimed.
    Preview,
    /// A stored boundary copied for one collar, or sent again as it is.
    Copy,
    /// A command refused at download.
    Delivery,
    /// A staged boundary checked again after the map changed.
    Recheck,
    /// A staged boundary checked as it took effect.
    Activation,
}

impl DbEnum for Stage {}

/// A sender and what they confirmed.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Sender {
    pub kind: SenderKind,
    pub by: Option<Actor>,
    /// Hard finding codes a person confirmed (`confirm` in the request).
    pub confirm: Vec<String>,
    /// The `confirm_token` a refusal gave for the shape and revision the person saw.
    pub token: Option<String>,
}

impl Sender {
    pub fn of(kind: SenderKind) -> Self {
        Self { kind, ..Default::default() }
    }

    /// Whoever is behind a request (see [`Sender::answered_by`]).
    pub fn person(identity: &Identity, confirm: Vec<String>) -> Self {
        Self::answered_by(identity.actor(), confirm)
    }

    /// [`Sender::person`] from who answered (a decision's responder). Only a
    /// signed-in person (an owner session, a person's token, a verified
    /// phone) is `farmer`; the app token is `api`, a brain token `mcp`, the
    /// rest `system`. None but `farmer` may confirm.
    pub fn answered_by(by: Actor, confirm: Vec<String>) -> Self {
        let kind = match by.via {
            Via::Local | Via::UserToken | Via::Text => SenderKind::Farmer,
            Via::AppToken => SenderKind::Api,
            Via::Brain => SenderKind::Mcp,
            Via::System | Via::Anonymous => SenderKind::System,
        };
        Self { kind, by: Some(by), confirm, token: None }
    }

    /// With the `confirm_token` the person sends back.
    pub fn with_token(mut self, token: Option<String>) -> Self {
        self.token = token.map(|t| t.trim().to_owned()).filter(|t| !t.is_empty());
        self
    }

    /// The sender as it counts for `hash` at `revision`: a person's
    /// confirmations stand only with the token for exactly that shape and revision.
    fn bound(&self, hash: &str, revision: i64) -> Self {
        let mut s = self.clone();
        if !(s.kind.is_person() && s.token.as_deref() == Some(confirm_token(revision, hash).as_str())) {
            s.confirm.clear();
        }
        s
    }
}

/// What a person sends back with `confirm`: the revision and the exact shape they saw.
pub fn confirm_token(revision: i64, hash: &str) -> String {
    format!("{revision}:{hash}")
}

/// SHA-256 (hex) of a shape as stored: what an authorization is for.
pub fn geometry_hash(g: &Polygon) -> String {
    op_core::keys::hash_key(&serde_json::to_string(g).unwrap_or_default())
}

/// What the gate made of a boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verdict {
    pub allowed: bool,
    /// The hard findings, as found.
    pub hard: Vec<Finding>,
    /// Hard codes nobody may go past.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub never: Vec<String>,
    /// Hard codes a person still has to confirm (none for other senders).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub confirm: Vec<String>,
    /// Why it was refused, one sentence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// For a person: send it back with `confirm` to go ahead (this shape, this revision of the map).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confirm_token: Option<String>,
}

/// The hard findings for a sender: the critical ones, less water for passing shapes.
pub fn hard(findings: &[Finding], kind: SenderKind) -> Vec<Finding> {
    findings.iter().filter(|f| f.severity == Severity::Critical && !(kind.passing() && f.code == "no_water")).cloned().collect()
}

/// Judge `findings` for `sender` (see the module docs).
pub fn judge(findings: &[Finding], sender: &Sender) -> Verdict {
    let hard = hard(findings, sender.kind);
    let mut codes: Vec<String> = Vec::new();
    for f in &hard {
        if !codes.contains(&f.code) {
            codes.push(f.code.clone());
        }
    }
    let never: Vec<String> = codes.iter().filter(|c| NEVER.contains(&c.as_str())).cloned().collect();
    let open: Vec<String> = codes.iter().filter(|c| !NEVER.contains(&c.as_str())).cloned().collect();
    let confirm: Vec<String> = if sender.kind.is_person() { open.iter().filter(|c| !sender.confirm.contains(c)).cloned().collect() } else { vec![] };
    let allowed = never.is_empty() && (if sender.kind.is_person() { confirm.is_empty() } else { open.is_empty() });
    let reason = (!allowed).then(|| {
        let said = sentence(&hard);
        if !never.is_empty() {
            format!("{said}. No boundary may take in a no-go zone; change the shape or remove the zone first.")
        } else if sender.kind.is_person() {
            format!("{said}. Send it again confirming {} to go ahead.", confirm.join(", "))
        } else {
            format!("{said}. Only a person can send this, after seeing it on the map.")
        }
    });
    Verdict { allowed, hard, never, confirm, reason, confirm_token: None }
}

/// "Crosses County Road; goes past the farm boundary"
fn sentence(hard: &[Finding]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for f in hard {
        let t = if parts.is_empty() { f.text.clone() } else { lower_first(&f.text) };
        if !parts.contains(&t) {
            parts.push(t);
        }
    }
    parts.join("; ")
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default()
}

/// The refusal: 409 with the hard findings, the codes a person may confirm
/// (with the `confirm_token` to send them with) and those nobody may.
pub fn refusal(v: &Verdict) -> ApiError {
    ApiError::new(StatusCode::CONFLICT, v.reason.clone().unwrap_or_else(|| "Refused by the safety gate.".into()))
        .with_details(json!({ "findings": v.hard, "confirm": v.confirm, "never": v.never, "confirm_token": v.confirm_token }))
}

/// One boundary at the gate.
pub struct Check<'a> {
    pub herd_id: &'a str,
    /// The shape as it would be stored.
    pub geometry: &'a Polygon,
    /// Its warning band (the collars' fit depends on it).
    pub warn_m: f64,
    /// What [`crate::shape::prepare`] found; the map findings are judged afresh.
    pub findings: &'a [Finding],
    pub sender: &'a Sender,
    pub decision_id: Option<&'a str>,
    /// A boundary of one collar's own (a pen, a copy).
    pub collar_id: Option<&'a str>,
    /// When it takes effect (the map then).
    pub at: DateTime<Utc>,
    pub stage: Stage,
    /// The sender's confirmations were given for this shape before (a copy,
    /// a reissue of the same shape): they stand without a token.
    pub inherited: bool,
}

/// A shape the gate let through, for [`insert`] alone. Only this module makes one.
#[derive(Debug, Clone)]
pub struct Authorized {
    gate_id: String,
    revision: i64,
    hash: String,
    herd_id: String,
    collar_id: Option<String>,
    warn_m: f64,
    at: DateTime<Utc>,
    sender: Sender,
    /// Prepare's findings that aren't the map's (an exclusion over all of it).
    base: Vec<Finding>,
    caps: Vec<CollarCaps>,
    decision_id: Option<String>,
    stage: Stage,
}

impl Authorized {
    /// The gate row that let it through.
    pub fn gate_id(&self) -> &str {
        &self.gate_id
    }
    /// The map revision it was judged at.
    pub fn revision(&self) -> i64 {
        self.revision
    }
    /// The hash of the shape it is for.
    pub fn hash(&self) -> &str {
        &self.hash
    }
}

/// Judge and log a boundary (see the module docs). A refusal is [`refusal`] (409).
pub async fn pass(ctx: &Ctx, c: &Check<'_>) -> ApiResult<Authorized> {
    let mut conn = ctx.db().acquire().await?;
    // One read transaction: the revision and the map it reads are one snapshot.
    sqlx::query("BEGIN").execute(&mut *conn).await?;
    let read = judged(&mut conn, c).await;
    let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
    let (v, revision, hash, sender, caps) = read?;
    let gate_id = log(&mut conn, c, &v, &sender, revision, &hash).await?;
    drop(conn);
    finish(c, v, gate_id, revision, hash, sender, caps)
}

/// [`pass`] on a connection holding the caller's write transaction (copies
/// made while a report or a handover holds the database): logged there too.
pub(crate) async fn pass_in(conn: &mut SqliteConnection, c: &Check<'_>) -> ApiResult<Authorized> {
    let (v, revision, hash, sender, caps) = judged(conn, c).await?;
    let gate_id = log(conn, c, &v, &sender, revision, &hash).await?;
    finish(c, v, gate_id, revision, hash, sender, caps)
}

/// [`pass`] with nothing stored after: a person's approval checked before
/// it is claimed, logged as `preview`. A refusal is [`refusal`] (409).
pub async fn preview(ctx: &Ctx, c: &Check<'_>) -> ApiResult<()> {
    let c = Check { stage: Stage::Preview, ..*c };
    pass(ctx, &c).await.map(|_| ())
}

fn finish(c: &Check<'_>, v: Verdict, gate_id: String, revision: i64, hash: String, sender: Sender, caps: Vec<CollarCaps>) -> ApiResult<Authorized> {
    if !v.allowed {
        tracing::warn!(herd = %c.herd_id, sender = %c.sender.kind.as_db(), stage = %c.stage.as_db(), codes = ?v.hard.iter().map(|f| f.code.as_str()).collect::<Vec<_>>(), "boundary refused by the safety gate");
        return Err(refusal(&v));
    }
    Ok(Authorized {
        gate_id,
        revision,
        hash,
        herd_id: c.herd_id.to_owned(),
        collar_id: c.collar_id.map(str::to_owned),
        warn_m: c.warn_m,
        at: c.at,
        sender,
        base: base_findings(c.findings),
        caps,
        decision_id: c.decision_id.map(str::to_owned),
        stage: c.stage,
    })
}

type Judged = (Verdict, i64, String, Sender, Vec<CollarCaps>);

async fn judged(conn: &mut SqliteConnection, c: &Check<'_>) -> ApiResult<Judged> {
    let revision = revision_in(conn).await?;
    let caps = caps_in(conn, c.herd_id, c.collar_id).await?;
    let found = exact_findings(conn, c.geometry, c.warn_m, c.findings, &caps, c.at).await?;
    let hash = geometry_hash(c.geometry);
    let sender = if c.inherited { c.sender.clone() } else { c.sender.bound(&hash, revision) };
    let mut v = judge(&found, &sender);
    if !v.allowed && sender.kind.is_person() && v.never.is_empty() {
        v.confirm_token = Some(confirm_token(revision, &hash));
    }
    Ok((v, revision, hash, sender, caps))
}

fn base_findings(found: &[Finding]) -> Vec<Finding> {
    found.iter().filter(|f| !MAP_CODES.contains(&f.code.as_str())).cloned().collect()
}

/// Prepare's findings that aren't the map's, and the map's on every exact
/// shape the collars would enforce, as the map is at `at`.
async fn exact_findings(
    conn: &mut SqliteConnection,
    g: &Polygon,
    warn_m: f64,
    found: &[Finding],
    caps: &[CollarCaps],
    at: DateTime<Utc>,
) -> anyhow::Result<Vec<Finding>> {
    let features = features_in(conn, at).await?;
    let rules = MapRules { holeless: false, require_water: settings_in(conn).await?.require_water };
    let mut out = base_findings(found);
    for shape in exact_shapes(g, warn_m, caps) {
        let Some(loc) = Local::new(&shape) else { continue };
        for f in map_findings(&loc, &shape, &features, rules) {
            if !out.iter().any(|o| o.code == f.code && o.targets == f.targets) {
                out.push(f);
            }
        }
    }
    crate::prepare::sort(&mut out);
    Ok(out)
}

/// The stored shape and the shape each kind of collar in `caps` enforces for it.
pub(crate) fn exact_shapes(g: &Polygon, warn_m: f64, caps: &[CollarCaps]) -> Vec<Polygon> {
    let mut seen: Vec<CollarLimits> = Vec::new();
    let mut out = vec![g.clone()];
    for c in caps {
        let l = c.fit_limits();
        if seen.contains(&l) {
            continue;
        }
        seen.push(l);
        if let Some(p) = crate::shape::command_shape(g, warn_m, c)
            && !out.contains(&p)
        {
            out.push(p);
        }
    }
    out
}

/// The map's safety policy revision (bumped by every feature or gate-setting change).
pub async fn revision(db: &sqlx::SqlitePool) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar("SELECT revision FROM gate_policy WHERE id = 1").fetch_optional(db).await?.unwrap_or(0))
}

pub(crate) async fn revision_in(conn: &mut SqliteConnection) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar("SELECT revision FROM gate_policy WHERE id = 1").fetch_optional(&mut *conn).await?.unwrap_or(0))
}

/// Features active at `at`, read on `conn`.
async fn features_in(conn: &mut SqliteConnection, at: DateTime<Utc>) -> anyhow::Result<Vec<MapFeature>> {
    let t = to_db(&at);
    let rows = sqlx::query(
        "SELECT * FROM features WHERE (active_from IS NULL OR active_from <= ?1) AND (active_until IS NULL OR active_until > ?1) ORDER BY created_at, rowid",
    )
    .bind(t)
    .fetch_all(&mut *conn)
    .await?;
    let all = rows.iter().map(op_core::features::feature_from_row).collect::<anyhow::Result<Vec<_>>>()?;
    // No-go zones are always in effect.
    Ok(all.into_iter().filter(|f| f.kind != FeatureKind::Exclusion).collect())
}

async fn settings_in(conn: &mut SqliteConnection) -> anyhow::Result<GateSettings> {
    let v: Option<String> = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?").bind(SETTINGS_KEY).fetch_optional(&mut *conn).await?;
    Ok(v.map(|v| serde_json::from_str(&v)).transpose()?.unwrap_or_default())
}

/// What the collars a boundary goes to can take: the herd's unparked collars, or the one collar.
async fn caps_in(conn: &mut SqliteConnection, herd_id: &str, collar_id: Option<&str>) -> anyhow::Result<Vec<CollarCaps>> {
    let rows = match collar_id {
        Some(c) => sqlx::query("SELECT fw, caps, limits FROM collars WHERE id = ?").bind(c).fetch_all(&mut *conn).await?,
        None => sqlx::query("SELECT fw, caps, limits FROM collars WHERE herd_id = ? AND parked_at IS NULL").bind(herd_id).fetch_all(&mut *conn).await?,
    };
    rows.iter().map(CollarCaps::from_row).collect()
}

async fn log(conn: &mut SqliteConnection, c: &Check<'_>, v: &Verdict, sender: &Sender, revision: i64, hash: &str) -> anyhow::Result<String> {
    let row = LogRow { herd_id: c.herd_id, collar_id: c.collar_id, sender, decision_id: c.decision_id, stage: c.stage, geometry: c.geometry, revision, hash };
    row.write(conn, v).await
}

struct LogRow<'a> {
    herd_id: &'a str,
    collar_id: Option<&'a str>,
    sender: &'a Sender,
    decision_id: Option<&'a str>,
    stage: Stage,
    geometry: &'a Polygon,
    revision: i64,
    hash: &'a str,
}

impl LogRow<'_> {
    async fn write(&self, conn: &mut SqliteConnection, v: &Verdict) -> anyhow::Result<String> {
        let id = id::new_id(GATE);
        let codes: Vec<&str> = v.hard.iter().map(|f| f.code.as_str()).collect();
        let confirmed: Vec<&String> = self.sender.confirm.iter().filter(|x| codes.contains(&x.as_str())).collect();
        let credential = self.sender.by.as_ref().map(|a| serde_json::to_value(a.via)).transpose()?.and_then(|v| v.as_str().map(str::to_owned));
        sqlx::query(
            "INSERT INTO gate_log (id, at, herd_id, collar_id, sender, actor, decision_id, allowed, codes, confirmed, findings, geometry, reason,
                 stage, revision, geometry_hash, credential)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(to_db(&now()))
        .bind(self.herd_id)
        .bind(self.collar_id)
        .bind(self.sender.kind.as_db())
        .bind(self.sender.by.as_ref().map(serde_json::to_string).transpose()?)
        .bind(self.decision_id)
        .bind(v.allowed)
        .bind(serde_json::to_string(&codes)?)
        .bind(serde_json::to_string(&confirmed)?)
        .bind(serde_json::to_string(&v.hard)?)
        .bind(serde_json::to_string(self.geometry)?)
        .bind(&v.reason)
        .bind(self.stage.as_db())
        .bind(self.revision)
        .bind(self.hash)
        .bind(credential)
        .execute(&mut *conn)
        .await?;
        Ok(id)
    }
}

// ---- storing ---------------------------------------------------------------------------------

/// A boundary to store.
pub(crate) struct NewBoundary<'a> {
    pub herd_id: &'a str,
    pub geometry: &'a Polygon,
    pub warn_m: f64,
    pub hysteresis_m: f64,
    pub effective_at: Option<DateTime<Utc>>,
    pub decision_id: &'a str,
    pub created_at: DateTime<Utc>,
    /// One collar's own boundary (an escape) instead of the herd's.
    pub collar_id: Option<&'a str>,
    /// The herd version whose shape a collar's own boundary carries.
    pub copy_of: Option<u32>,
}

/// Store a boundary the gate let through, with the next version (one
/// sequence across herds, so a collar moved to another herd always finds
/// that herd's next boundary newer than what it holds). The caller holds a
/// write transaction (`begin_immediate`). The shape must be the one `auth`
/// is for. When the map changed since the check, it is judged again here,
/// where no map change can land before the commit; a refusal then is 409
/// (logged once the caller's transaction is gone).
pub(crate) async fn insert(tx: &mut SqliteConnection, ctx: &Ctx, auth: Authorized, b: &NewBoundary<'_>) -> ApiResult<Boundary> {
    if geometry_hash(b.geometry) != auth.hash || b.herd_id != auth.herd_id || b.collar_id != auth.collar_id.as_deref() || b.warn_m != auth.warn_m {
        return Err(ApiError::internal("A boundary is stored only exactly as the safety gate let it through."));
    }
    let mut revision = auth.revision;
    let current = revision_in(tx).await?;
    if current != auth.revision {
        // The map changed between the check and now: judged again on what it is now.
        // A person's confirmation was for the map they saw; it doesn't carry over.
        let found = exact_findings(tx, b.geometry, b.warn_m, &auth.base, &auth.caps, auth.at).await?;
        let sender = if auth.stage == Stage::Copy { auth.sender.clone() } else { auth.sender.bound(&auth.hash, current) };
        let mut v = judge(&found, &sender);
        if !v.allowed {
            if sender.kind.is_person() && v.never.is_empty() {
                v.confirm_token = Some(confirm_token(current, &auth.hash));
            }
            v.reason = v.reason.map(|r| format!("The map changed while it was being checked. {r}"));
            tracing::warn!(herd = %auth.herd_id, revision = current, "boundary refused: the map changed while it was being checked");
            let pool = ctx.db().clone();
            let (verdict, geometry) = (v.clone(), b.geometry.clone());
            tokio::spawn(async move {
                let Ok(mut conn) = pool.acquire().await else { return };
                let row = LogRow {
                    herd_id: &auth.herd_id,
                    collar_id: auth.collar_id.as_deref(),
                    sender: &sender,
                    decision_id: auth.decision_id.as_deref(),
                    stage: auth.stage,
                    geometry: &geometry,
                    revision: current,
                    hash: &auth.hash,
                };
                if let Err(e) = row.write(&mut conn, &verdict).await {
                    tracing::warn!("gate log: {e:#}");
                }
            });
            return Err(refusal(&v));
        }
        revision = current;
    }
    let id = id::new_id(id::BOUNDARY);
    // The next version comes from the version index (db::sql::NEXT_VERSION).
    let (next,): (i64,) = sqlx::query_as(crate::db::sql::NEXT_VERSION).fetch_one(&mut *tx).await?;
    let (version,): (i64,) = sqlx::query_as(
        "INSERT INTO boundaries (id, herd_id, version, geometry, warn_m, hysteresis_m, effective_at, decision_id, created_at, collar_id, copy_of, gate_id, policy_revision)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?) RETURNING version",
    )
    .bind(&id)
    .bind(b.herd_id)
    .bind(next)
    .bind(serde_json::to_string(b.geometry).map_err(anyhow::Error::from)?)
    .bind(b.warn_m)
    .bind(b.hysteresis_m)
    .bind(b.effective_at.as_ref().map(to_db))
    .bind(b.decision_id)
    .bind(to_db(&b.created_at))
    .bind(b.collar_id)
    .bind(b.copy_of.map(i64::from))
    .bind(&auth.gate_id)
    .bind(revision)
    .fetch_one(&mut *tx)
    .await?;
    sqlx::query("UPDATE gate_log SET boundary_id = ?, version = ?, revision = ? WHERE id = ?")
        .bind(&id)
        .bind(version)
        .bind(revision)
        .bind(&auth.gate_id)
        .execute(&mut *tx)
        .await?;
    Ok(Boundary {
        id,
        herd_id: b.herd_id.to_owned(),
        version: version as u32,
        geometry: b.geometry.clone(),
        warn_m: b.warn_m,
        hysteresis_m: b.hysteresis_m,
        effective_at: b.effective_at,
        decision_id: b.decision_id.to_owned(),
        created_at: b.created_at,
        collar_id: b.collar_id.map(str::to_owned),
    })
}

// ---- stored boundaries ----------------------------------------------------------------------

/// Who a stored boundary went out under: its gate row's sender and what was
/// confirmed for it. One stored before the gate linked rows is the server's
/// (nothing confirmed).
async fn origin_in(conn: &mut SqliteConnection, boundary_id: &str) -> anyhow::Result<Sender> {
    let row: Option<(String, Option<String>, String)> =
        sqlx::query_as("SELECT g.sender, g.actor, g.confirmed FROM boundaries b JOIN gate_log g ON g.id = b.gate_id WHERE b.id = ?")
            .bind(boundary_id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((kind, actor, confirmed)) = row else { return Ok(Sender::of(SenderKind::System)) };
    Ok(Sender {
        kind: SenderKind::from_db(&kind)?,
        by: actor.map(|a| serde_json::from_str(&a)).transpose()?,
        confirm: serde_json::from_str(&confirmed)?,
        token: None,
    })
}

/// When a stored boundary takes effect, for the map it is judged on: now, or its time when that is ahead.
fn effect_time(b: &Boundary) -> DateTime<Utc> {
    let t = now();
    b.effective_at.filter(|e| *e > t).unwrap_or(t)
}

/// A stored boundary again, for one collar (`collar_id`) or the herd, under
/// what it went out with: judged on the map now, on the caller's write
/// transaction. For a copy ([`insert`] it with `collar_id` and the same shape).
pub(crate) async fn reauthorize_in(conn: &mut SqliteConnection, b: &Boundary, collar_id: Option<&str>, stage: Stage) -> ApiResult<Authorized> {
    let sender = origin_in(conn, &b.id).await?;
    let c = Check {
        herd_id: &b.herd_id,
        geometry: &b.geometry,
        warn_m: b.warn_m,
        findings: &[],
        sender: &sender,
        decision_id: Some(&b.decision_id),
        collar_id,
        at: effect_time(b),
        stage,
        inherited: true,
    };
    pass_in(conn, &c).await
}

/// [`reauthorize_in`] on a connection of its own (nothing stored after it): `Err` is the refusal.
pub async fn recheck(ctx: &Ctx, b: &Boundary, stage: Stage) -> ApiResult<()> {
    let mut conn = ctx.db().acquire().await?;
    let sender = origin_in(&mut conn, &b.id).await?;
    drop(conn);
    let c = Check {
        herd_id: &b.herd_id,
        geometry: &b.geometry,
        warn_m: b.warn_m,
        findings: &[],
        sender: &sender,
        decision_id: Some(&b.decision_id),
        collar_id: b.collar_id.as_deref(),
        at: effect_time(b),
        stage,
        inherited: true,
    };
    pass(ctx, &c).await.map(|_| ())
}

/// Delivery verdicts: (boundary, the collar's fit, revision) → refused why.
type DeliveryCache = HashMap<(String, CollarLimits, i64), Option<String>>;
static DELIVERED: LazyLock<Mutex<DeliveryCache>> = LazyLock::new(Default::default);
const DELIVERY_CACHE_MAX: usize = 4096;

/// Whether one collar may download `b` now: the exact command it would get
/// (fitted to its caps), on the map as it is now, under what `b` went out
/// with. Refused is 409, logged (`delivery`) and told in the activity once
/// per boundary, kind of collar and map revision.
pub async fn deliverable(ctx: &Ctx, b: &Boundary, collar_id: &str, caps: &CollarCaps) -> ApiResult<()> {
    let revision = revision(ctx.db()).await?;
    let key = (b.id.clone(), caps.fit_limits(), revision);
    if let Some(known) = DELIVERED.lock().unwrap_or_else(|e| e.into_inner()).get(&key).cloned() {
        return match known {
            None => Ok(()),
            Some(why) => Err(ApiError::new(StatusCode::CONFLICT, why)),
        };
    }
    let mut conn = ctx.db().acquire().await?;
    let sender = origin_in(&mut conn, &b.id).await?;
    let shape = crate::shape::command_shape(&b.geometry, b.warn_m, caps).unwrap_or_else(|| b.geometry.clone());
    let at = effect_time(b);
    let features = features_in(&mut conn, at).await?;
    let rules = MapRules { holeless: false, require_water: settings_in(&mut conn).await?.require_water };
    let found = Local::new(&shape).map(|loc| map_findings(&loc, &shape, &features, rules)).unwrap_or_default();
    let v = judge(&found, &sender);
    let refused =
        (!v.allowed).then(|| format!("Boundary v{} can't go to this collar as it would enforce it: {}", b.version, v.reason.clone().unwrap_or_default()));
    if let Some(why) = &refused {
        let hash = geometry_hash(&b.geometry);
        let row = LogRow {
            herd_id: &b.herd_id,
            collar_id: Some(collar_id),
            sender: &sender,
            decision_id: Some(&b.decision_id),
            stage: Stage::Delivery,
            geometry: &shape,
            revision,
            hash: &hash,
        };
        row.write(&mut conn, &v).await?;
        drop(conn);
        tracing::warn!(collar = %collar_id, version = b.version, "{why}");
        crate::withdraw::tell(ctx, "boundary.refused", &b.herd_id, Some(collar_id), b, why).await;
    }
    let mut cache = DELIVERED.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= DELIVERY_CACHE_MAX {
        cache.clear();
    }
    cache.insert(key, refused.clone());
    match refused {
        None => Ok(()),
        Some(why) => Err(refusal(&Verdict { reason: Some(why), ..v })),
    }
}

/// What a person confirmed for a decision the last time the gate let it
/// through: a move's sweep steps carry it.
pub async fn confirmed_for(ctx: &Ctx, decision_id: &str) -> anyhow::Result<Vec<String>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT confirmed FROM gate_log WHERE decision_id = ? AND allowed = 1 AND sender = 'farmer' ORDER BY at DESC, rowid DESC LIMIT 1")
            .bind(decision_id)
            .fetch_optional(ctx.db())
            .await?;
    Ok(row.map(|(c,)| serde_json::from_str(&c)).transpose()?.unwrap_or_default())
}

// ---- settings -------------------------------------------------------------------------------

/// The farm's gate settings.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GateSettings {
    /// A boundary with none of the farm's mapped water inside is refused.
    #[serde(default)]
    pub require_water: bool,
}

pub async fn settings(ctx: &Ctx) -> anyhow::Result<GateSettings> {
    Ok(ctx.store().get_setting::<GateSettings>(SETTINGS_KEY).await?.unwrap_or_default())
}

async fn get_settings(State(ctx): State<Ctx>) -> ApiResult<Json<GateSettings>> {
    Ok(Json(settings(&ctx).await?))
}

async fn put_settings(State(ctx): State<Ctx>, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<GateSettings>> {
    let current = settings(&ctx).await?;
    let next: GateSettings = op_core::patch::apply(&current, &body, &[])?;
    ctx.store().set_setting(SETTINGS_KEY, &next).await?;
    Ok(Json(next))
}

// ---- the log --------------------------------------------------------------------------------

/// One logged gate decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GateEntry {
    pub id: String,
    pub at: DateTime<Utc>,
    pub herd_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
    pub sender: SenderKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<Actor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    pub allowed: bool,
    pub codes: Vec<String>,
    pub confirmed: Vec<String>,
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<Polygon>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub stage: Stage,
    /// The map revision it was judged at.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry_hash: Option<String>,
    /// How the sender signed in (`local`, `user_token`, `app_token`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credential: Option<String>,
    /// The boundary it became, once stored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct LogQuery {
    pub herd_id: Option<String>,
    pub decision_id: Option<String>,
    /// Only refusals.
    #[serde(default)]
    pub refused: bool,
    pub limit: Option<i64>,
}

/// Newest first: at most `limit` (1 to 500, default 100).
pub async fn entries(ctx: &Ctx, q: &LogQuery) -> anyhow::Result<Vec<GateEntry>> {
    let rows = sqlx::query(
        "SELECT * FROM gate_log WHERE (?1 IS NULL OR herd_id = ?1) AND (?2 IS NULL OR decision_id = ?2) AND (?3 = 0 OR allowed = 0)
         ORDER BY at DESC, rowid DESC LIMIT ?4",
    )
    .bind(&q.herd_id)
    .bind(&q.decision_id)
    .bind(q.refused)
    .bind(q.limit.unwrap_or(100).clamp(1, 500))
    .fetch_all(ctx.db())
    .await?;
    rows.iter()
        .map(|r| {
            let actor: Option<String> = r.try_get("actor")?;
            let geometry: Option<String> = r.try_get("geometry")?;
            Ok(GateEntry {
                id: r.try_get("id")?,
                at: from_db(&r.try_get::<String, _>("at")?)?,
                herd_id: r.try_get("herd_id")?,
                collar_id: r.try_get("collar_id")?,
                sender: SenderKind::from_db(&r.try_get::<String, _>("sender")?)?,
                by: actor.map(|a| serde_json::from_str(&a)).transpose()?,
                decision_id: r.try_get("decision_id")?,
                allowed: r.try_get("allowed")?,
                codes: serde_json::from_str(&r.try_get::<String, _>("codes")?)?,
                confirmed: serde_json::from_str(&r.try_get::<String, _>("confirmed")?)?,
                findings: serde_json::from_str(&r.try_get::<String, _>("findings")?)?,
                geometry: geometry.map(|g| serde_json::from_str(&g)).transpose()?,
                reason: r.try_get("reason")?,
                stage: Stage::from_db(&r.try_get::<String, _>("stage")?)?,
                revision: r.try_get("revision")?,
                geometry_hash: r.try_get("geometry_hash")?,
                credential: r.try_get("credential")?,
                boundary_id: r.try_get("boundary_id")?,
                version: r.try_get::<Option<i64>, _>("version")?.map(|v| v as u32),
            })
        })
        .collect()
}

async fn get_log(State(ctx): State<Ctx>, Query(q): Query<LogQuery>) -> ApiResult<Json<Vec<GateEntry>>> {
    Ok(Json(entries(&ctx, &q).await?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(code: &str, severity: Severity) -> Finding {
        Finding { code: code.into(), severity, text: format!("{code} text"), geometry: None, targets: vec![] }
    }

    #[test]
    fn people_confirm_hard_findings_and_nobody_else_can() {
        let found = [f("crosses_road", Severity::Critical), f("no_water", Severity::Warning), f("simplified", Severity::Info)];
        for kind in [
            SenderKind::Api,
            SenderKind::Mcp,
            SenderKind::Brain,
            SenderKind::Timer,
            SenderKind::Auto,
            SenderKind::Schedule,
            SenderKind::Sweep,
            SenderKind::Escape,
            SenderKind::System,
        ] {
            let v = judge(&found, &Sender { kind, confirm: vec!["crosses_road".into()], ..Default::default() });
            assert!(!v.allowed, "{kind:?}");
            assert!(v.confirm.is_empty(), "only a person is asked to confirm");
            assert!(v.reason.unwrap().contains("Only a person"));
        }
        let person = |confirm: &[&str]| Sender { kind: SenderKind::Farmer, confirm: confirm.iter().map(|s| s.to_string()).collect(), ..Default::default() };
        let v = judge(&found, &person(&[]));
        assert!(!v.allowed);
        assert_eq!(v.confirm, ["crosses_road"]);
        assert!(judge(&found, &person(&["crosses_road"])).allowed);
        assert!(!judge(&found, &person(&["crosses_farm_boundary"])).allowed, "the code named must be the one found");
        // Warnings and info never stop anyone.
        assert!(judge(&found[1..], &Sender::of(SenderKind::Auto)).allowed);
    }

    #[test]
    fn nobody_goes_into_a_no_go_zone() {
        let found = [f("enters_no_go", Severity::Critical)];
        let v = judge(&found, &Sender { kind: SenderKind::Farmer, confirm: vec!["enters_no_go".into()], ..Default::default() });
        assert!(!v.allowed);
        assert_eq!(v.never, ["enters_no_go"]);
        assert!(v.confirm.is_empty());
        assert!(v.reason.unwrap().contains("no-go zone"));
    }

    #[test]
    fn passing_shapes_are_not_asked_for_water() {
        let found = [f("no_water", Severity::Critical)];
        assert!(judge(&found, &Sender::of(SenderKind::Sweep)).allowed);
        assert!(judge(&found, &Sender::of(SenderKind::Escape)).allowed);
        assert!(!judge(&found, &Sender::of(SenderKind::Schedule)).allowed);
    }

    #[test]
    fn the_app_token_is_api_and_a_person_is_farmer() {
        assert_eq!(Sender::person(&Identity::owner(Via::AppToken), vec![]).kind, SenderKind::Api);
        assert_eq!(Sender::person(&Identity::owner(Via::Local), vec![]).kind, SenderKind::Farmer);
        assert_eq!(Sender::person(&Identity::owner(Via::Brain), vec![]).kind, SenderKind::Mcp);
        assert_eq!(SenderKind::Escape.as_db(), "escape");
    }

    /// Astra 02 F06: the app token is a program; a person's confirmation is
    /// bound to the exact shape and map revision they saw.
    #[test]
    fn only_a_person_confirms_and_only_for_what_they_saw() {
        let found = [f("crosses_road", Severity::Critical)];
        let api = Sender::person(&Identity::owner(Via::AppToken), vec!["crosses_road".into()]).with_token(Some(confirm_token(3, "abc")));
        assert!(!judge(&found, &api.bound("abc", 3)).allowed, "the app token can't confirm");
        let person = Sender::person(&Identity::owner(Via::Local), vec!["crosses_road".into()]);
        assert!(!judge(&found, &person.bound("abc", 3)).allowed, "no token: nothing confirmed");
        let with = person.clone().with_token(Some(confirm_token(3, "abc")));
        assert!(judge(&found, &with.bound("abc", 3)).allowed);
        assert!(!judge(&found, &with.bound("abc", 4)).allowed, "the map changed since");
        assert!(!judge(&found, &with.bound("abd", 3)).allowed, "another shape");
    }
}
