//! Staged boundaries after the map changes (Astra 02 F04, server side).
//!
//! A staged boundary is on the collars before it takes effect, and a collar
//! opens it on its own clock. So when the map's safety policy revision moves
//! (a no-go zone, road, farm boundary, hazard, water or exclusion drawn,
//! changed or removed, or the gate's settings changed: [`crate::gate::revision`]),
//! every staged boundary still alive, the herd's and collars' own copies, is
//! judged again by the gate as each kind of collar would enforce it. For a
//! herd with one the gate now refuses:
//!
//! - The boundary in effect goes again at once as a new immediate version
//!   (prepared afresh, else as it is when the gate still passes it under what
//!   it went out with). A collar drops every staged slot below a newer
//!   version that takes effect, so the refused one is withdrawn from every
//!   collar that picks this up before its time. Staged boundaries of the
//!   herd that still pass go again above it at their times (a strip schedule
//!   stages its own again).
//! - The download never hands the refused one to a collar again
//!   ([`crate::gate::deliverable`]).
//! - The activity says what was withdrawn (`boundary.withdrawn`), or, when
//!   nothing the gate passes can replace it, that it couldn't be and which
//!   collars hold it (`boundary.unsafe`).
//!
//! A collar offline until past the staged time still opens what it holds:
//! only the collar can prevent that (permanent no-go zones held on the
//! collar, or an activation lease; see opencollar `protocol/v2`). The same
//! happens, and the same reply, when a staged boundary is found refused as it
//! takes effect ([`took_effect_refused`]).

use std::time::Duration;

use op_core::time::{now, to_db};
use op_core::{ActivityEvent, Boundary, Ctx, id};

use crate::boundary::{SendOpts, resend, send_boundary};
use crate::db;
use crate::gate::{self, Sender, SenderKind, Stage};

/// Re-check staged boundaries whenever the map's policy revision moves (and once at start).
pub fn spawn_policy_watcher(ctx: Ctx) {
    ctx.clone().supervise("ingest.policy", Some(Duration::from_secs(2)), |ctx| async move {
        let mut seen: Option<i64> = None;
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tick.tick() => {}
            }
            ctx.beat("ingest.policy");
            let rev = match gate::revision(ctx.db()).await {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("policy revision: {e:#}");
                    continue;
                }
            };
            if seen == Some(rev) {
                continue;
            }
            match recheck_staged(&ctx).await {
                Ok(_) => seen = Some(rev),
                Err(e) => tracing::warn!("staged boundaries after a map change: {e:#}"),
            }
        }
    });
}

/// What happened to one refused staged boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct Withdrawn {
    pub herd_id: String,
    pub version: u32,
    /// The immediate version that replaced the boundary in effect, so collars drop it.
    pub superseded_by: Option<u32>,
    /// Collars that hold it, as far as the server knows.
    pub holding: Vec<String>,
}

/// Judge every staged boundary still alive on the map as it is now; for each
/// herd with one refused, supersede (see the module docs). Returns the refused ones.
pub async fn recheck_staged(ctx: &Ctx) -> anyhow::Result<Vec<Withdrawn>> {
    let t = now();
    let herds: Vec<String> =
        sqlx::query_scalar("SELECT DISTINCT herd_id FROM boundaries WHERE effective_at > ? ORDER BY herd_id").bind(to_db(&t)).fetch_all(ctx.db()).await?;
    let mut out = Vec::new();
    for herd_id in herds {
        let split = db::herd_boundaries(ctx.db(), &herd_id, t).await?;
        let floor = split.active.as_ref().map_or(0, |a| a.version);
        let own: Vec<Boundary> =
            sqlx::query("SELECT * FROM boundaries WHERE herd_id = ? AND collar_id IS NOT NULL AND effective_at > ? AND version > ? ORDER BY version")
                .bind(&herd_id)
                .bind(to_db(&t))
                .bind(i64::from(floor))
                .fetch_all(ctx.db())
                .await?
                .iter()
                .map(op_core::store::boundary_from_row)
                .collect::<anyhow::Result<_>>()?;
        let mut refused = Vec::new();
        for b in split.staged.iter().chain(&own) {
            if let Err(e) = gate::recheck(ctx, b, Stage::Recheck).await {
                if e.status != axum::http::StatusCode::CONFLICT {
                    return Err(anyhow::anyhow!(e.message));
                }
                tracing::warn!(herd = %herd_id, version = b.version, "staged boundary refused after a map change: {}", e.message);
                refused.push((b.clone(), e.message));
            }
        }
        if !refused.is_empty() {
            out.extend(supersede(ctx, &herd_id, &split, &refused).await?);
        }
    }
    Ok(out)
}

/// A staged boundary took effect that the gate now refuses: the boundary in
/// effect goes again prepared afresh (the map's new zones cut out), or the
/// activity says it couldn't be replaced.
pub async fn took_effect_refused(ctx: &Ctx, b: &Boundary, why: &str) {
    tracing::warn!(herd = %b.herd_id, version = b.version, "a staged boundary took effect that the safety gate now refuses: {why}");
    let opts = SendOpts { warn_m: Some(b.warn_m), hysteresis_m: Some(b.hysteresis_m), sender: Sender::of(SenderKind::System), ..Default::default() };
    match send_boundary(ctx, &b.herd_id, b.geometry.clone(), opts, &b.decision_id).await {
        Ok(n) => tell(ctx, "boundary.withdrawn", &b.herd_id, None, b, &format!("{why} Replaced by v{} with the map's new zones kept out.", n.version)).await,
        Err(e) => {
            let holding = holding(ctx, &[b.version]).await.unwrap_or_default();
            let said = format!("{why} Nothing the safety gate passes can replace it ({e:#}); {} collar(s) enforce it now.", holding.len());
            tracing::error!(herd = %b.herd_id, version = b.version, "{said}");
            tell(ctx, "boundary.unsafe", &b.herd_id, None, b, &said).await;
        }
    }
}

async fn supersede(ctx: &Ctx, herd_id: &str, split: &db::HerdBoundaries, refused: &[(Boundary, String)]) -> anyhow::Result<Vec<Withdrawn>> {
    let versions: Vec<u32> = refused.iter().map(|(b, _)| b.version).collect();
    let replaced = match &split.active {
        Some(a) => {
            let opts = SendOpts { warn_m: Some(a.warn_m), hysteresis_m: Some(a.hysteresis_m), sender: Sender::of(SenderKind::System), ..Default::default() };
            match send_boundary(ctx, herd_id, a.geometry.clone(), opts, &a.decision_id).await {
                Ok(n) => Ok(n),
                Err(first) => resend(ctx, a, None).await.map_err(|e| format!("{first:#}; as it is: {}", e.message)),
            }
        }
        None => Err("the herd has no boundary in effect to send again".to_owned()),
    };
    let mut out = Vec::new();
    match replaced {
        Ok(n) => {
            // Staged boundaries that still pass, not a schedule's (it stages its own again), go again above it.
            for b in split.staged.iter().filter(|b| !versions.contains(&b.version) && !b.decision_id.starts_with(op_core::schedule::SCHEDULE)) {
                if let Err(e) = resend(ctx, b, b.effective_at).await {
                    tracing::warn!(herd = %herd_id, version = b.version, "staged boundary not staged again: {}", e.message);
                }
            }
            for (b, why) in refused {
                let holding = holding(ctx, &[b.version]).await?;
                let said = format!("{why} Withdrawn: v{} replaces the boundary in effect, so collars drop it.", n.version);
                tell(ctx, "boundary.withdrawn", herd_id, None, b, &said).await;
                out.push(Withdrawn { herd_id: herd_id.to_owned(), version: b.version, superseded_by: Some(n.version), holding });
            }
        }
        Err(e) => {
            for (b, why) in refused {
                let holding = holding(ctx, &[b.version]).await?;
                let said = format!(
                    "{why} It can't be withdrawn: nothing the safety gate passes can replace the boundary in effect ({e}). {} collar(s) hold it and will open it at its time.",
                    holding.len()
                );
                tracing::error!(herd = %herd_id, version = b.version, "{said}");
                tell(ctx, "boundary.unsafe", herd_id, None, b, &said).await;
                out.push(Withdrawn { herd_id: herd_id.to_owned(), version: b.version, superseded_by: None, holding });
            }
        }
    }
    Ok(out)
}

/// Collars that hold any of `versions` (staged or applied), as their reports and acks say.
pub async fn holding(ctx: &Ctx, versions: &[u32]) -> anyhow::Result<Vec<String>> {
    let mut out = Vec::new();
    for v in versions {
        let ids: Vec<String> = sqlx::query_scalar("SELECT collar_id FROM collar_slots WHERE version = ? AND status != 'rejected' ORDER BY collar_id")
            .bind(i64::from(*v))
            .fetch_all(ctx.db())
            .await?;
        for c in ids {
            if !out.contains(&c) {
                out.push(c);
            }
        }
    }
    Ok(out)
}

/// Say it in the activity for the herd, and the collar when it is one's.
pub(crate) async fn tell(ctx: &Ctx, kind: &str, herd_id: &str, collar_id: Option<&str>, b: &Boundary, why: &str) {
    let at = now();
    let title = match kind {
        "boundary.withdrawn" => format!("Staged boundary v{} withdrawn by the safety gate", b.version),
        "boundary.unsafe" => format!("Boundary v{} is refused by the safety gate and can't be withdrawn", b.version),
        "boundary.refused" => format!("Boundary v{} not sent to a collar: refused by the safety gate", b.version),
        _ => format!("Boundary v{}", b.version),
    };
    let mut targets = vec![("herd".to_owned(), herd_id.to_owned()), ("decision".to_owned(), b.decision_id.clone())];
    if let Some(c) = collar_id {
        targets.push(("collar".into(), c.to_owned()));
    }
    let event = ActivityEvent {
        id: id::new_id(id::EVENT),
        kind: kind.into(),
        source: "system".into(),
        occurred_at: at,
        recorded_at: at,
        title,
        body: Some(why.to_owned()),
        payload: serde_json::json!({ "boundary_id": b.id, "version": b.version, "effective_at": b.effective_at, "collar_id": collar_id }),
        targets,
    };
    if let Err(e) = ctx.store().record_event(&event).await {
        tracing::warn!("activity log: {e:#}");
    }
}
