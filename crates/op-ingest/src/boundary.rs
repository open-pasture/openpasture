//! Boundaries per herd: versions, staging, dispatch and status.

use std::time::Duration;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::store::decision_from_row;
use op_core::time::{now, to_db};
use op_core::{ActivityEvent, ApiError, ApiJson, ApiResult, Boundary, BoundaryStatus, Ctx, Event, Move, ProposedBoundary, id};
use op_geo::{GeofenceConfig, Polygon};
use op_protocol::{BoundaryCommand, sign_command, wire_time};
use serde::Deserialize;

use crate::db;
use crate::moves::{self, FarmerDecision};

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/herds/{id}/boundary", get(get_status).post(post_boundary))
}

/// Options for [`send_boundary`]. Missing margins use the firmware defaults
/// (warn 5 m, hysteresis 1 m). `effective_at` in the future stages the boundary.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct SendOpts {
    pub warn_m: Option<f64>,
    pub hysteresis_m: Option<f64>,
    pub effective_at: Option<DateTime<Utc>>,
}

/// Store the herd's next boundary (the next version of the one sequence all
/// herds share) and make it available to its
/// collars. Collars pick it up on their next boundary poll. Also points the
/// decision at the boundary and publishes a `boundary` event.
pub async fn send_boundary(ctx: &Ctx, herd_id: &str, geometry: Polygon, opts: SendOpts, decision_id: &str) -> anyhow::Result<Boundary> {
    send(ctx, herd_id, geometry, opts, decision_id).await.map_err(|e| anyhow::anyhow!(e.message))
}

/// Check a boundary before anything is recorded. Returns the normalised
/// geometry and margins.
pub(crate) fn check(geometry: &Polygon, opts: &SendOpts) -> ApiResult<(Polygon, f64, f64)> {
    let geometry = geometry.validated()?;
    // What a collar will check: 3-64 vertices, ranges, no crossings, no holes.
    BoundaryCommand::from_polygon("check", 1, &geometry, None)?;
    let d = GeofenceConfig::default();
    let warn_m = opts.warn_m.unwrap_or(d.warn_m);
    let hysteresis_m = opts.hysteresis_m.unwrap_or(d.hysteresis_m);
    for (v, name) in [(warn_m, "warn_m"), (hysteresis_m, "hysteresis_m")] {
        if !v.is_finite() || !(0.0..=1000.0).contains(&v) {
            return Err(ApiError::bad_request(format!("{name} must be between 0 and 1000 metres.")));
        }
    }
    Ok((geometry, warn_m, hysteresis_m))
}

async fn send(ctx: &Ctx, herd_id: &str, geometry: Polygon, opts: SendOpts, decision_id: &str) -> ApiResult<Boundary> {
    if ctx.store().get_herd(herd_id).await?.is_none() {
        return Err(ApiError::not_found("No such herd."));
    }
    let (geometry, warn_m, hysteresis_m) = check(&geometry, &opts)?;
    let created_at = now();
    // Wire times are whole seconds; a time already past means now.
    let effective_at = opts.effective_at.map(wire_time::trunc_secs).filter(|t| *t > created_at);
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let b = NewBoundary { herd_id, geometry: &geometry, warn_m, hysteresis_m, effective_at, decision_id, created_at, collar_id: None, copy_of: None };
    let boundary = insert_boundary(&mut tx, &b).await?;
    sqlx::query("UPDATE decisions SET boundary_id = ? WHERE id = ?").bind(&boundary.id).bind(decision_id).execute(&mut *tx).await?;
    tx.commit().await?;
    announce(ctx, &boundary).await;
    Ok(boundary)
}

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

/// Insert with the next version. Versions come from one sequence across all
/// herds, so a collar moved to another herd always finds that herd's next
/// boundary newer than what it holds. The caller holds a write transaction.
pub(crate) async fn insert_boundary(tx: &mut sqlx::SqliteConnection, b: &NewBoundary<'_>) -> ApiResult<Boundary> {
    let id = id::new_id(id::BOUNDARY);
    let (version,): (i64,) = sqlx::query_as(
        "INSERT INTO boundaries (id, herd_id, version, geometry, warn_m, hysteresis_m, effective_at, decision_id, created_at, collar_id, copy_of)
         SELECT ?, ?, COALESCE(MAX(version), 0) + 1, ?, ?, ?, ?, ?, ?, ?, ? FROM boundaries
         RETURNING version",
    )
    .bind(&id)
    .bind(b.herd_id)
    .bind(serde_json::to_string(b.geometry).map_err(anyhow::Error::from)?)
    .bind(b.warn_m)
    .bind(b.hysteresis_m)
    .bind(b.effective_at.as_ref().map(to_db))
    .bind(b.decision_id)
    .bind(to_db(&b.created_at))
    .bind(b.collar_id)
    .bind(b.copy_of.map(i64::from))
    .fetch_one(&mut *tx)
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

/// Log and publish a stored boundary.
pub(crate) async fn announce(ctx: &Ctx, boundary: &Boundary) {
    let herd_id = &boundary.herd_id;
    tracing::info!(herd = %herd_id, version = boundary.version, staged = boundary.effective_at.is_some(), "boundary sent");
    let _ = ctx
        .store()
        .record_event(&ActivityEvent {
            id: id::new_id(id::EVENT),
            kind: "boundary.sent".into(),
            source: "system".into(),
            occurred_at: boundary.created_at,
            recorded_at: boundary.created_at,
            title: format!("Boundary v{} sent", boundary.version),
            body: None,
            payload: serde_json::json!({ "boundary_id": boundary.id, "version": boundary.version, "effective_at": boundary.effective_at }),
            targets: vec![("herd".into(), herd_id.clone()), ("decision".into(), boundary.decision_id.clone())],
        })
        .await
        .inspect_err(|e| tracing::warn!("activity log: {e:#}"));
    ctx.publish(Event::Boundary { herd_id: herd_id.clone(), boundary: boundary.clone() });
}

/// A collar joined `herd_id` holding `held` from its old herd. If the herd's
/// boundaries are all at or below that version, the collar would never ask
/// for them, so they are stored again (same shapes, margins and times) with
/// new versions. Returns what was re-issued.
pub(crate) async fn reissue_for_moved_collar(ctx: &Ctx, herd_id: &str, held: u32) -> anyhow::Result<Vec<Boundary>> {
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let rows = sqlx::query("SELECT * FROM boundaries WHERE herd_id = ? AND collar_id IS NULL ORDER BY version").bind(herd_id).fetch_all(&mut *tx).await?;
    let all = rows.iter().map(op_core::store::boundary_from_row).collect::<anyhow::Result<Vec<_>>>()?;
    let split = db::split_boundaries(all, now());
    let current: Vec<Boundary> = split.active.into_iter().chain(split.staged).collect();
    if current.last().is_none_or(|b| b.version > held) {
        return Ok(vec![]);
    }
    let created_at = now();
    let mut out = Vec::new();
    for b in &current {
        let nb = NewBoundary {
            herd_id,
            geometry: &b.geometry,
            warn_m: b.warn_m,
            hysteresis_m: b.hysteresis_m,
            effective_at: b.effective_at.filter(|t| *t > created_at),
            decision_id: &b.decision_id,
            created_at,
            collar_id: None,
            copy_of: None,
        };
        out.push(insert_boundary(&mut tx, &nb).await.map_err(|e| anyhow::anyhow!(e.message))?);
    }
    tx.commit().await?;
    for b in &out {
        announce(ctx, b).await;
    }
    Ok(out)
}

/// Older proposals for the herd give way to a newer decision. Only rows still
/// `proposed` change, so a decision the timer or the farmer already claimed is
/// left alone. Publishes each superseded decision.
pub async fn supersede_proposals(ctx: &Ctx, herd_id: &str, keep: &str) -> anyhow::Result<Vec<op_core::Decision>> {
    let rows = sqlx::query("UPDATE decisions SET status = 'superseded', apply_at = NULL WHERE herd_id = ? AND status = 'proposed' AND id != ? RETURNING *")
        .bind(herd_id)
        .bind(keep)
        .fetch_all(ctx.db())
        .await?;
    let out = rows.iter().map(decision_from_row).collect::<anyhow::Result<Vec<_>>>()?;
    for d in &out {
        ctx.publish(Event::Decision { decision: d.clone() });
    }
    Ok(out)
}

/// The farm record follows an applied move: the herd in `to`, the paddock it
/// left (`from`, else its recorded paddock) resting from now, `to` grazing.
pub async fn move_herd_on_record(ctx: &Ctx, herd_id: &str, to: Option<&str>, from: Option<String>) -> anyhow::Result<()> {
    let store = ctx.store();
    let Some(to) = to.map(str::to_owned) else { return Ok(()) };
    let Some(mut herd) = store.get_herd(herd_id).await? else { return Ok(()) };
    let from = herd.paddock_id.clone().or(from);
    if from.as_deref() != Some(to.as_str())
        && let Some(f) = &from
        && let Some(mut p) = store.get_paddock(f).await?
    {
        p.status = op_core::PaddockStatus::Resting;
        p.grazed_until = Some(now());
        store.update_paddock(&p).await?;
    }
    if let Some(mut p) = store.get_paddock(&to).await? {
        p.status = op_core::PaddockStatus::Grazing;
        store.update_paddock(&p).await?;
    }
    herd.paddock_id = Some(to);
    store.update_herd(&herd).await?;
    Ok(())
}

/// Active and staged boundaries, the latest proposal awaiting the farmer, and
/// each collar's latest ack.
pub async fn boundary_status(ctx: &Ctx, herd_id: &str) -> anyhow::Result<BoundaryStatus> {
    let split = db::herd_boundaries(ctx.db(), herd_id, now()).await?;
    let row =
        sqlx::query("SELECT * FROM decisions WHERE herd_id = ? AND status = 'proposed' AND geometry IS NOT NULL ORDER BY created_at DESC, id DESC LIMIT 1")
            .bind(herd_id)
            .fetch_optional(ctx.db())
            .await?;
    let proposed = row.map(|r| decision_from_row(&r)).transpose()?.and_then(|d| d.geometry.map(|geometry| ProposedBoundary { decision_id: d.id, geometry }));
    Ok(BoundaryStatus {
        active: split.active,
        pending: split.staged.last().cloned(),
        proposed,
        acks: db::latest_acks(ctx.db(), herd_id).await?,
        r#move: crate::moves::current_move(ctx.db(), herd_id, now()).await?,
        escapes: crate::escapes::current_escapes(ctx.db(), herd_id, now()).await?,
    })
}

async fn get_status(State(ctx): State<Ctx>, Path(herd_id): Path<String>) -> ApiResult<Json<BoundaryStatus>> {
    if ctx.store().get_herd(&herd_id).await?.is_none() {
        return Err(ApiError::not_found("No such herd."));
    }
    Ok(Json(boundary_status(&ctx, &herd_id).await?))
}

#[derive(Deserialize)]
struct NewBoundaryBody {
    geometry: Polygon,
    #[serde(flatten)]
    opts: SendOpts,
}

/// The farmer drew a target: record it as an applied farmer decision and
/// start the move (decision, move and first boundary in one transaction),
/// then supersede open proposals and move the herd on the record.
async fn post_boundary(State(ctx): State<Ctx>, Path(herd_id): Path<String>, ApiJson(body): ApiJson<NewBoundaryBody>) -> ApiResult<(StatusCode, Json<Move>)> {
    if ctx.store().get_herd(&herd_id).await?.is_none() {
        return Err(ApiError::not_found("No such herd."));
    }
    let (geometry, _, _) = check(&body.geometry, &body.opts)?;
    let paddocks = ctx.store().list_paddocks().await?;
    let to_paddock = geometry.centroid().and_then(|c| db::paddock_for_point(&paddocks, c)).map(|p| p.id.clone());
    let decision_id = id::new_id(id::DECISION);
    let farmer = FarmerDecision { to_paddock_id: to_paddock.as_deref(), reasoning: "Boundary drawn by the farmer." };
    let started = moves::begin(&ctx, &herd_id, geometry, body.opts, &decision_id, Some(farmer)).await?;

    if let Some(r) = sqlx::query("SELECT * FROM decisions WHERE id = ?").bind(&decision_id).fetch_optional(ctx.db()).await? {
        ctx.publish(Event::Decision { decision: decision_from_row(&r)? });
    }
    // The farmer's target replaces any open proposal and moves the herd on the record.
    if let Err(e) = supersede_proposals(&ctx, &herd_id, &decision_id).await {
        tracing::warn!("superseding proposals: {e:#}");
    }
    if let Err(e) = move_herd_on_record(&ctx, &herd_id, to_paddock.as_deref(), None).await {
        tracing::warn!("updating herd position: {e:#}");
    }
    Ok((StatusCode::CREATED, Json(started.r#move)))
}

/// The signed command a collar downloads for a stored boundary. The command
/// id is the boundary id.
pub fn command_for(ctx: &Ctx, b: &Boundary) -> ApiResult<BoundaryCommand> {
    let mut cmd = BoundaryCommand::from_polygon(b.id.clone(), b.version, &b.geometry, b.effective_at)?;
    cmd.herd_id = Some(b.herd_id.clone());
    cmd.warn_m = Some(b.warn_m);
    cmd.hysteresis_m = Some(b.hysteresis_m);
    sign_command(&mut cmd, ctx.signing_key());
    Ok(cmd)
}

/// Publish a `boundary` event when a staged boundary takes effect, so the map
/// swaps it in without polling.
pub fn spawn_activation_watcher(ctx: Ctx) {
    tokio::spawn(async move {
        let mut last = now();
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tick.tick() => {}
            }
            let t = now();
            let rows = sqlx::query("SELECT * FROM boundaries WHERE effective_at > ? AND effective_at <= ? AND collar_id IS NULL ORDER BY version")
                .bind(to_db(&last))
                .bind(to_db(&t))
                .fetch_all(ctx.db())
                .await;
            last = t;
            match rows {
                Ok(rows) => {
                    for r in rows {
                        if let Ok(b) = op_core::store::boundary_from_row(&r) {
                            tracing::info!(herd = %b.herd_id, version = b.version, "staged boundary in effect");
                            ctx.publish(Event::Boundary { herd_id: b.herd_id.clone(), boundary: b });
                        }
                    }
                }
                Err(e) => tracing::warn!("boundary watcher: {e:#}"),
            }
        }
    });
}
