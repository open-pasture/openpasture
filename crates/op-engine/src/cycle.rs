//! The decision cycle (kit `briefing/engine.py`): assemble context, ask the
//! brain, record the decision, apply the herd's autonomy, send the boundary,
//! handle the farmer's answer, and evaluate outcomes later.
//!
//! Nothing reaches a collar without a recorded decision, and the autonomy
//! setting (or the farmer) allowing it.

use std::time::Duration;

use chrono::DateTime;
use chrono::Utc;
use op_core::{
    ActivityEvent, ApiError, ApiResult, Autonomy, BrainId, Ctx, DbEnum, Decision, DecisionAction, DecisionSource, DecisionStatus, Event, Herd, Paddock,
    Polygon, id, time,
};
use serde_json::{Value, json};
use sqlx::Row;

use crate::{context, db, knowledge, signals, skills};

/// A brain gets this long before the decision fails.
const BRAIN_TIMEOUT: Duration = Duration::from_secs(20 * 60);
/// Share of fixes inside the boundary for "held the line" (kit).
const HELD_BOUNDARY_SHARE: f64 = 0.95;

fn short(e: &anyhow::Error) -> String {
    let s = format!("{e:#}");
    let s = s.lines().next().unwrap_or_default().to_owned();
    if s.chars().count() > 300 { format!("{}…", s.chars().take(300).collect::<String>()) } else { s }
}

async fn activity(ctx: &Ctx, kind: &str, source: &str, title: String, d: &Decision) {
    let now = time::now();
    let e = ActivityEvent {
        id: id::new_id(id::EVENT),
        kind: kind.into(),
        source: source.into(),
        occurred_at: now,
        recorded_at: now,
        title,
        body: d.reasoning.clone(),
        payload: json!({ "decision_id": d.id, "status": d.status, "action": d.action, "to_paddock_id": d.to_paddock_id }),
        targets: vec![("herd".into(), d.herd_id.clone()), ("decision".into(), d.id.clone())],
    };
    if let Err(e) = ctx.store().record_event(&e).await {
        tracing::warn!("activity log: {e:#}");
    }
}

/// The URL a brain's MCP client uses: the brain tools only
/// (`ctx.tools().brain_tools()`). On a loopback URL the request is local and
/// needs no token. Otherwise the URL carries a token minted for this run only
/// (`/mcp?scope=brain` and nothing else, listing and calling only the brain
/// tools, gone when the returned guard drops); the CLI brains take it out of
/// the URL and hand it to the CLI through the environment or a 0600 file,
/// never argv.
pub fn brain_mcp_url(ctx: &Ctx) -> (String, Option<op_core::BrainToken>) {
    crate::tools::register_tools(ctx);
    let base = ctx.local_url();
    let loopback = ["http://127.0.0.1", "http://localhost", "http://[::1]"].iter().any(|p| base.starts_with(p));
    if loopback {
        return (format!("{base}/mcp?scope=brain"), None);
    }
    let token = ctx.mint_brain_token(BRAIN_TIMEOUT + Duration::from_secs(60), ctx.tools().brain_tools());
    (format!("{base}/mcp?scope=brain&token={}", token.as_str()), Some(token))
}

/// `POST /api/herds/{id}/decide`: record a running decision and run the cycle
/// in the background. A decision already running for the herd is returned as is.
pub async fn start_decision(ctx: &Ctx, herd_id: &str) -> ApiResult<Decision> {
    let herd = ctx.store().get_herd(herd_id).await?.ok_or_else(|| ApiError::not_found("No such herd."))?;
    if ctx.store().get_farm().await?.is_none() {
        return Err(ApiError::bad_request("Set up the farm first."));
    }
    let brain = ctx.settings().await?.brain;
    let d = Decision {
        id: id::new_id(id::DECISION),
        herd_id: herd.id.clone(),
        source: if brain.id == BrainId::Heuristic { DecisionSource::Heuristic } else { DecisionSource::Brain },
        brain: Some(brain.id),
        model: brain.model,
        status: DecisionStatus::Running,
        action: None,
        to_paddock_id: None,
        geometry: None,
        reasoning: None,
        confidence: None,
        need: None,
        inputs: json!({}),
        apply_at: None,
        boundary_id: None,
        error: None,
        created_at: time::now(),
        responded_at: None,
        outcome: None,
    };
    // One running decision per herd (unique index): a second start gets the first.
    if let Err(e) = db::insert(ctx, &d).await {
        if e.downcast_ref::<sqlx::Error>().and_then(|e| e.as_database_error()).is_some_and(|d| d.is_unique_violation()) {
            let row = sqlx::query("SELECT * FROM decisions WHERE herd_id = ? AND status = 'running' AND brain IS NOT NULL")
                .bind(herd_id)
                .fetch_optional(ctx.db())
                .await?;
            if let Some(r) = row {
                return Ok(op_core::store::decision_from_row(&r)?);
            }
        }
        return Err(e.into());
    }
    let (ctx2, d2) = (ctx.clone(), d.clone());
    tokio::spawn(async move {
        let id = d2.id.clone();
        if let Err(e) = run(&ctx2, d2, herd).await {
            tracing::warn!(decision = %id, "decision failed: {e:#}");
            let row = sqlx::query("UPDATE decisions SET status = 'failed', error = ? WHERE id = ? AND status = 'running' RETURNING *")
                .bind(short(&e))
                .bind(&id)
                .fetch_optional(ctx2.db())
                .await;
            if let Ok(Some(r)) = row
                && let Ok(d) = op_core::store::decision_from_row(&r)
            {
                ctx2.publish(Event::Decision { decision: d });
            }
        }
    });
    Ok(d)
}

async fn run(ctx: &Ctx, mut d: Decision, herd: Herd) -> anyhow::Result<()> {
    let log = {
        let (ctx, id) = (ctx.clone(), d.id.clone());
        move |line: String| ctx.publish(Event::DecisionLog { decision_id: id.clone(), line })
    };
    if let Err(e) = evaluate_due(ctx, Some(&herd.id)).await {
        tracing::warn!("outcome evaluation: {e:#}");
    }
    log("Assembling context.".into());
    let a = context::assemble(ctx, &herd, true, &log).await?;

    let brain = op_brain::resolve(ctx).await?;
    d.brain = Some(brain.id());
    d.source = if brain.id() == BrainId::Heuristic { DecisionSource::Heuristic } else { DecisionSource::Brain };
    log(format!("Asking the {} brain.", brain.id().as_db()));

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let fwd = tokio::spawn({
        let log = log.clone();
        async move {
            while let Some(line) = rx.recv().await {
                log(line);
            }
        }
    });
    // The token (if any) lives until this run ends.
    let (mcp_url, _mcp_token) = brain_mcp_url(ctx);
    let req = op_brain::DecisionRequest {
        herd_id: herd.id.clone(),
        context: a.context.clone(),
        instructions: skills::daily_grazing_decision(ctx),
        mcp_url,
        tools: ctx.tools().brain_tools(),
        log: tx,
    };
    let out = tokio::time::timeout(BRAIN_TIMEOUT, brain.decide(req)).await;
    let _ = fwd.await;
    let out = match out {
        Ok(r) => r?,
        Err(_) => anyhow::bail!("The brain took longer than {} minutes.", BRAIN_TIMEOUT.as_secs() / 60),
    };

    d.action = Some(out.action);
    d.to_paddock_id = out.to_paddock_id.clone().filter(|s| !s.trim().is_empty());
    d.geometry = out.geometry.clone();
    d.reasoning = Some(out.reasoning.trim().to_owned()).filter(|s| !s.is_empty());
    d.confidence = Some(out.confidence.clamp(0.0, 1.0));
    d.need = out.need.clone().filter(|s| !s.trim().is_empty());
    if out.model.is_some() {
        d.model = out.model.clone();
    }
    d.inputs = json!({
        "from_paddock_id": a.current_paddock_id,
        "position_source": a.context["position_source"],
        "land_report_ids": a.land_report_ids,
        "knowledge_entry_ids": a.knowledge_ids,
        "collars": a.context["collars"],
        "signals": a.context["signals"],
    });
    let d = record(ctx, d, &herd).await?;
    log(format!("Decision: {}.", describe(&d)));
    Ok(())
}

fn describe(d: &Decision) -> String {
    let action = d.action.map(|a| a.as_db()).unwrap_or_default();
    match d.status {
        DecisionStatus::Proposed if d.apply_at.is_some() => format!("{action}, applies at {} unless stopped", time::to_db(&d.apply_at.unwrap_or_default())),
        s => format!("{action}, {}", s.as_db()),
    }
}

/// Validate a boundary the way a collar will: 3-64 vertices, sane ranges, no
/// crossings, no holes.
pub fn check_geometry(g: &Polygon) -> ApiResult<Polygon> {
    let g = g.validated()?;
    let ring = g.outer_ring();
    let n = if ring.first() == ring.last() { ring.len().saturating_sub(1) } else { ring.len() };
    if !(3..=64).contains(&n) {
        return Err(ApiError::bad_request("A boundary needs 3 to 64 corners."));
    }
    if g.holes().next().is_some() {
        return Err(ApiError::bad_request("A boundary can't have holes."));
    }
    Ok(g)
}

fn paddock_for(paddocks: &[Paddock], g: &Polygon) -> Option<String> {
    g.centroid().and_then(|c| signals::paddock_at(paddocks, c)).map(|p| p.id.clone())
}

/// Write a brain (or MCP) decision: fill a MOVE's geometry from its paddock,
/// validate, supersede older proposals, and apply the herd's autonomy.
pub async fn record(ctx: &Ctx, mut d: Decision, herd: &Herd) -> anyhow::Result<Decision> {
    let paddocks = ctx.store().list_paddocks().await?;
    let fail = |mut d: Decision, msg: String| async move {
        d.status = DecisionStatus::Failed;
        d.error = Some(msg);
        db::update(ctx, &d).await?;
        anyhow::Ok(d)
    };
    if d.action == Some(DecisionAction::Move) {
        if let Some(pid) = &d.to_paddock_id
            && !paddocks.iter().any(|p| &p.id == pid)
        {
            return fail(d.clone(), format!("Paddock {pid} does not exist.")).await;
        }
        if d.geometry.is_none() {
            d.geometry = d.to_paddock_id.as_ref().and_then(|pid| paddocks.iter().find(|p| &p.id == pid)).map(|p| p.geometry.clone());
        }
        let Some(g) = d.geometry.clone() else {
            return fail(d, "A MOVE needs a paddock or a boundary.".into()).await;
        };
        match check_geometry(&g) {
            Ok(g) => d.geometry = Some(g),
            Err(e) => return fail(d, format!("The boundary is not valid: {}", e.message)).await,
        }
        if d.to_paddock_id.is_none() {
            d.to_paddock_id = d.geometry.as_ref().and_then(|g| paddock_for(&paddocks, g));
        }
    } else {
        d.to_paddock_id = None;
        d.geometry = None;
    }

    d.status = DecisionStatus::Proposed;
    d.error = None;
    if d.action == Some(DecisionAction::Move) && herd.autonomy == Autonomy::Timer {
        d.apply_at = Some(time::now() + chrono::Duration::minutes(herd.timer_minutes as i64));
    }
    db::update(ctx, &d).await?;
    supersede(ctx, &d.herd_id, &d.id).await?;
    activity(ctx, "decision.proposed", d.source.as_db().as_str(), format!("Decision: {}", d.action.map(|a| a.as_db()).unwrap_or_default()), &d).await;

    if d.action == Some(DecisionAction::Move) && herd.autonomy == Autonomy::Auto {
        return apply_claimed(ctx, &d.id, None).await;
    }
    Ok(d)
}

/// Older proposals for the herd give way to a newer decision. Conditional:
/// a proposal the timer or the farmer already claimed is left alone.
pub async fn supersede(ctx: &Ctx, herd_id: &str, keep: &str) -> anyhow::Result<()> {
    op_ingest::supersede_proposals(ctx, herd_id, keep).await?;
    Ok(())
}

/// Atomically take a proposed decision for applying, so the timer and the
/// farmer can't both send it. Marks it approved.
async fn claim(ctx: &Ctx, id: &str, farmer: Option<(&Value, DateTime<Utc>)>) -> anyhow::Result<bool> {
    let n = match farmer {
        Some((resp, at)) => {
            sqlx::query("UPDATE decisions SET status = 'approved', apply_at = NULL, responded_at = ?, inputs = json_set(inputs, '$.farmer_response', json(?)) WHERE id = ? AND status = 'proposed'")
                .bind(time::to_db(&at))
                .bind(resp.to_string())
                .bind(id)
                .execute(ctx.db())
                .await?
        }
        None => sqlx::query("UPDATE decisions SET status = 'approved' WHERE id = ? AND status = 'proposed'").bind(id).execute(ctx.db()).await?,
    };
    Ok(n.rows_affected() == 1)
}

/// Claim a proposed decision and send its boundary.
async fn apply_claimed(ctx: &Ctx, id: &str, farmer: Option<(&Value, DateTime<Utc>)>) -> anyhow::Result<Decision> {
    if !claim(ctx, id, farmer).await? {
        let d = db::get(ctx, id).await?.ok_or_else(|| anyhow::anyhow!("decision {id} vanished"))?;
        anyhow::bail!(ApiError::conflict(format!("This decision is {} now.", d.status.as_db())).message);
    }
    let d = db::get(ctx, id).await?.ok_or_else(|| anyhow::anyhow!("decision {id} vanished"))?;
    ctx.publish(Event::Decision { decision: d.clone() });
    apply(ctx, d).await
}

/// Start a move toward the decision's boundary (the target) and mark the
/// decision applied. The move's steps go out under this decision. Failures
/// leave it failed with a short error.
pub async fn apply(ctx: &Ctx, mut d: Decision) -> anyhow::Result<Decision> {
    let Some(geometry) = d.geometry.clone() else {
        d.status = DecisionStatus::Failed;
        d.error = Some("There is no boundary to send.".into());
        db::update(ctx, &d).await?;
        return Ok(d);
    };
    match op_ingest::start_move(ctx, &d.herd_id, geometry, op_ingest::SendOpts::default(), &d.id).await {
        Ok(started) => {
            // Only the status: the move's steps write `boundary_id` as they go.
            let row = sqlx::query("UPDATE decisions SET status = 'applied', apply_at = NULL, error = NULL WHERE id = ? RETURNING *")
                .bind(&d.id)
                .fetch_one(ctx.db())
                .await?;
            d = op_core::store::decision_from_row(&row)?;
            ctx.publish(Event::Decision { decision: d.clone() });
            if let Err(e) = move_herd(ctx, &d).await {
                tracing::warn!("updating herd position: {e:#}");
            }
            let m = &started.r#move;
            let title = match (&started.boundary, m.status) {
                (Some(b), op_core::MoveStatus::Done) => format!("Boundary v{} sent", b.version),
                (Some(b), _) => format!("Move started, step v{} sent", b.version),
                (None, _) => "Move started".into(),
            };
            activity(ctx, "decision.applied", "system", title, &d).await;
        }
        Err(e) => {
            d.status = DecisionStatus::Failed;
            d.error = Some(format!("The boundary was not sent: {}", short(&e)));
            db::update(ctx, &d).await?;
        }
    }
    Ok(d)
}

/// The farm record follows an applied move: herd in the new paddock, the old
/// one resting from now.
pub async fn move_herd(ctx: &Ctx, d: &Decision) -> anyhow::Result<()> {
    op_ingest::move_herd_on_record(ctx, &d.herd_id, d.to_paddock_id.as_deref(), signals::from_paddock(d)).await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Response {
    Approve,
    Reject,
    Modify,
}

/// `POST /api/decisions/{id}/respond`. Approve sends a MOVE; modify records the
/// farmer's boundary (the brain's stays in `inputs.proposed_geometry`) and
/// sends it; reject stops it. A farmer's note becomes a farm lesson.
pub async fn respond(ctx: &Ctx, id: &str, action: Response, geometry: Option<Polygon>, note: Option<String>) -> ApiResult<Decision> {
    let d = db::get(ctx, id).await?.ok_or_else(|| ApiError::not_found("No such decision."))?;
    if d.status != DecisionStatus::Proposed {
        return Err(ApiError::conflict(format!("This decision is {}; there is nothing to answer.", d.status.as_db())));
    }
    let note = note.map(|n| n.trim().to_owned()).filter(|n| !n.is_empty());
    let at = time::now();
    let paddocks = ctx.store().list_paddocks().await?;
    let mut resp = json!({ "action": match action { Response::Approve => "approve", Response::Reject => "reject", Response::Modify => "modify" }, "at": time::to_db(&at) });
    if let Some(n) = &note {
        resp["note"] = json!(n);
    }

    let out = match action {
        Response::Reject => {
            let d = answer(ctx, id, DecisionStatus::Rejected, &resp, at).await?;
            activity(ctx, "decision.rejected", "farmer", "Decision rejected".into(), &d).await;
            d
        }
        Response::Approve if d.action != Some(DecisionAction::Move) => {
            let d = answer(ctx, id, DecisionStatus::Approved, &resp, at).await?;
            activity(ctx, "decision.approved", "farmer", "Decision approved".into(), &d).await;
            d
        }
        Response::Approve => apply_claimed(ctx, id, Some((&resp, at))).await.map_err(|e| ApiError::conflict(short(&e)))?,
        Response::Modify => {
            let g = check_geometry(&geometry.ok_or_else(|| ApiError::bad_request("A change needs the farmer's boundary (geometry)."))?)?;
            resp["geometry"] = json!(g);
            // Record the farmer's boundary on the decision before claiming it.
            let mut m = d.clone();
            if let Some(orig) = &d.geometry {
                m.inputs["proposed_geometry"] = json!(orig);
            }
            if let Some(orig) = &d.to_paddock_id {
                m.inputs["proposed_to_paddock_id"] = json!(orig);
            }
            m.action = Some(DecisionAction::Move);
            m.to_paddock_id = paddock_for(&paddocks, &g).or(d.to_paddock_id.clone());
            m.geometry = Some(g);
            let n = sqlx::query("UPDATE decisions SET action = 'MOVE', geometry = ?, to_paddock_id = ?, inputs = ? WHERE id = ? AND status = 'proposed'")
                .bind(serde_json::to_string(&m.geometry).map_err(anyhow::Error::from)?)
                .bind(&m.to_paddock_id)
                .bind(m.inputs.to_string())
                .bind(id)
                .execute(ctx.db())
                .await?;
            if n.rows_affected() != 1 {
                return Err(ApiError::conflict("This decision changed while you were answering."));
            }
            apply_claimed(ctx, id, Some((&resp, at))).await.map_err(|e| ApiError::conflict(short(&e)))?
        }
    };

    if let Some(n) = note {
        // A rejection is usually about the target; an approval or change about where the herd was.
        let pid = match action {
            Response::Reject => out.to_paddock_id.clone().or_else(|| signals::from_paddock(&out)),
            _ => signals::from_paddock(&out).or_else(|| out.to_paddock_id.clone()),
        };
        let title = match &pid {
            Some(p) => format!("Farmer's note on {}", context::name_of(&paddocks, p)),
            None => "Farmer's note".into(),
        };
        if let Err(e) = knowledge::add_lesson(ctx, &title, &n, "farmer", &format!("decision {}", out.id), Some(&out.id), pid.as_deref()).await {
            tracing::warn!("saving farmer note: {e:#}");
        }
    }
    Ok(out)
}

/// Close a proposal with the farmer's answer, only if it is still proposed
/// (the timer may have claimed it a moment ago): 409 otherwise, and nothing changes.
async fn answer(ctx: &Ctx, id: &str, status: DecisionStatus, resp: &Value, at: DateTime<Utc>) -> ApiResult<Decision> {
    let row = sqlx::query(
        "UPDATE decisions SET status = ?, apply_at = NULL, responded_at = ?, inputs = json_set(inputs, '$.farmer_response', json(?))
         WHERE id = ? AND status = 'proposed' RETURNING *",
    )
    .bind(status.as_db())
    .bind(time::to_db(&at))
    .bind(resp.to_string())
    .bind(id)
    .fetch_optional(ctx.db())
    .await?;
    let Some(row) = row else {
        let now = db::get(ctx, id).await?.map(|d| d.status.as_db()).unwrap_or_else(|| "gone".into());
        return Err(ApiError::conflict(format!("This decision is {now} now; there is nothing to answer.")));
    };
    let d = op_core::store::decision_from_row(&row)?;
    ctx.publish(Event::Decision { decision: d.clone() });
    Ok(d)
}

/// Apply timer decisions whose time has come.
pub async fn apply_due(ctx: &Ctx) -> anyhow::Result<()> {
    let now = time::now();
    for d in db::with_status(ctx, DecisionStatus::Proposed).await? {
        if d.action == Some(DecisionAction::Move) && d.apply_at.is_some_and(|t| t <= now) {
            match apply_claimed(ctx, &d.id, None).await {
                Ok(d) => tracing::info!(decision = %d.id, status = %d.status.as_db(), "timer decision applied"),
                Err(e) => tracing::debug!("timer apply skipped: {e:#}"),
            }
        }
    }
    Ok(())
}

/// Decisions still marked running from before a restart can't finish.
pub async fn fail_interrupted(ctx: &Ctx) -> anyhow::Result<()> {
    for mut d in db::with_status(ctx, DecisionStatus::Running).await? {
        d.status = DecisionStatus::Failed;
        d.error = Some("Interrupted by a restart.".into());
        db::update(ctx, &d).await?;
    }
    Ok(())
}

/// Write outcomes onto applied moves whose day is over (kit `evaluate`):
/// share of fixes inside the boundary, cues, and NDVI change on the paddock
/// the herd left.
pub async fn evaluate_due(ctx: &Ctx, herd_id: Option<&str>) -> anyhow::Result<usize> {
    let now = time::now();
    let window = chrono::Duration::hours(signals::WINDOW_HOURS);
    let rows = sqlx::query(
        "SELECT id FROM decisions WHERE status = 'applied' AND action = 'MOVE' AND outcome IS NULL AND boundary_id IS NOT NULL AND (? IS NULL OR herd_id = ?)",
    )
    .bind(herd_id)
    .bind(herd_id)
    .fetch_all(ctx.db())
    .await?;
    let mut n = 0;
    for r in rows {
        let Some(mut d) = db::get(ctx, &r.get::<String, _>(0)).await? else { continue };
        let Some(b) = sqlx::query("SELECT * FROM boundaries WHERE id = ?").bind(&d.boundary_id).fetch_optional(ctx.db()).await? else { continue };
        let b = op_core::store::boundary_from_row(&b)?;
        let start = b.effective_at.unwrap_or(b.created_at).max(b.created_at);
        if start + window > now {
            continue;
        }
        let end = start + window + chrono::Duration::seconds(1);
        let points = herd_points(ctx, &d.herd_id, start, end).await?;
        let cues = signals::herd_cues(ctx, &d.herd_id, start, end).await?;
        let mut notes: Vec<String> = Vec::new();
        let mut held = Value::Null;
        if !points.is_empty() {
            let inside = points.iter().filter(|p| b.geometry.contains(**p)).count();
            let share = inside as f64 / points.len() as f64;
            held = json!(share >= HELD_BOUNDARY_SHARE);
            notes.push(format!("{:.0}% of {} collar fixes were inside the boundary.", share * 100.0, points.len()));
        }
        if let Some(from) = signals::from_paddock(&d) {
            notes.extend(ndvi_change(ctx, &d, &from).await?);
        }
        d.outcome = Some(json!({
            "evaluated_at": time::to_db(&now),
            "herd_held_boundary": held,
            "cue_count": if points.is_empty() && cues.is_empty() { Value::Null } else { json!(cues.len()) },
            "residual_estimate_inches": null,
            "notes": notes,
        }));
        db::update(ctx, &d).await?;
        if held == json!(false) {
            let paddocks = ctx.store().list_paddocks().await?;
            let name = d.to_paddock_id.as_deref().map(|p| context::name_of(&paddocks, p)).unwrap_or_else(|| "the new".into());
            let body = format!("After the move to {name}, {} {} cues in the first day.", notes.join(" "), cues.len());
            let _ = knowledge::add_lesson(
                ctx,
                &format!("Herd tested the {name} boundary"),
                &body,
                "outcome",
                &format!("decision {}", d.id),
                Some(&d.id),
                d.to_paddock_id.as_deref(),
            )
            .await;
        }
        n += 1;
    }
    Ok(n)
}

/// Sampled fix positions for a herd in `[from, to)`.
async fn herd_points(ctx: &Ctx, herd_id: &str, from: DateTime<Utc>, to: DateTime<Utc>) -> anyhow::Result<Vec<[f64; 2]>> {
    let (f, t) = (time::unix_ms(&from), time::unix_ms(&to));
    let n: i64 =
        sqlx::query("SELECT COUNT(*) FROM fixes WHERE herd_id = ? AND t >= ? AND t < ?").bind(herd_id).bind(f).bind(t).fetch_one(ctx.db()).await?.get(0);
    let stride = (n / 20_000).max(1);
    let rows = sqlx::query("SELECT lon, lat FROM fixes WHERE herd_id = ? AND t >= ? AND t < ? AND id % ? = 0")
        .bind(herd_id)
        .bind(f)
        .bind(t)
        .bind(stride)
        .fetch_all(ctx.db())
        .await?;
    Ok(rows.iter().map(|r| [r.get::<f64, _>(0), r.get::<f64, _>(1)]).collect())
}

async fn ndvi_change(ctx: &Ctx, d: &Decision, paddock_id: &str) -> anyhow::Result<Vec<String>> {
    fn ndvi(r: &Value) -> Option<f64> {
        crate::land::ok_section(r, "imagery")?.get("ndvi_stats")?.get("mean")?.as_f64()
    }
    let ids: Vec<String> = d.inputs["land_report_ids"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect();
    let mut before = None;
    for id in &ids {
        if let Some(r) =
            sqlx::query("SELECT report FROM land_reports WHERE id = ? AND paddock_id = ?").bind(id).bind(paddock_id).fetch_optional(ctx.db()).await?
        {
            before = ndvi(&serde_json::from_str(&r.get::<String, _>(0))?);
            break;
        }
    }
    let Some(latest) = crate::land::latest(ctx, paddock_id).await? else { return Ok(vec![]) };
    let latest_id = latest["report_id"].as_str().unwrap_or_default();
    match (before, ndvi(&latest)) {
        (Some(b), Some(a)) if !ids.iter().any(|i| i == latest_id) => {
            let name = context::name_of(&ctx.store().list_paddocks().await?, paddock_id);
            Ok(vec![format!("{name} NDVI {b:.2} at decision time, {a:.2} in the latest imagery.")])
        }
        _ => Ok(vec![]),
    }
}
