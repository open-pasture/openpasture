//! Hosted telemetry loop for the stable demo factory collars.
//!
//! Reports are deliberately sent over the public collar HTTP contract, not
//! inserted into analytics tables, so ingest, alerts and live events see the
//! same path as hardware.

use std::collections::HashMap;
use std::time::Duration;

use op_core::Ctx;
use serde_json::json;

const TASK: &str = "mobile.hosted_sim";

pub fn spawn(ctx: &Ctx) {
    ctx.supervise(TASK, Some(Duration::from_secs(30)), |ctx| async move {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("HTTP client");
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tick.tick() => {
                    ctx.beat(TASK);
                    if let Err(error) = report_all(&ctx, &client).await {
                        tracing::warn!("hosted collar simulation tick: {error:#}");
                    }
                    if let Err(error) = refresh_decisions(&ctx).await {
                        tracing::warn!("hosted decision refresh: {error:#}");
                    }
                }
            }
        }
    });
}

async fn refresh_decisions(ctx: &Ctx) -> anyhow::Result<()> {
    let cutoff = op_core::time::to_db(&(chrono::Utc::now() - chrono::Duration::hours(2)));
    for herd in ctx.store().list_herds().await? {
        let claimed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT 1 FROM collars c JOIN fleet_units u ON u.collar_id = c.id
             WHERE c.herd_id = ? AND (u.serial LIKE 'OPR-ROOK-%' OR u.serial LIKE 'OPS-STARLING-%'))",
        )
        .bind(&herd.id)
        .fetch_one(ctx.db())
        .await?;
        if !claimed {
            continue;
        }
        let recent: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM decisions WHERE herd_id = ? AND created_at >= ?)")
            .bind(&herd.id)
            .bind(&cutoff)
            .fetch_one(ctx.db())
            .await?;
        if !recent {
            match op_engine::cycle::start_decision(ctx, &herd.id).await {
                Ok(decision) => tracing::info!(herd = %herd.id, decision = %decision.id, "hosted simulation decision started"),
                Err(error) => tracing::warn!(herd = %herd.id, "hosted simulation decision: {}", error.message),
            }
        }
    }
    Ok(())
}

async fn report_all(ctx: &Ctx, client: &reqwest::Client) -> anyhow::Result<()> {
    let bearers = op_fleet::hosted::bearers(ctx)?;
    if bearers.is_empty() {
        return Ok(());
    }
    let herds: HashMap<_, _> = ctx.store().list_herds().await?.into_iter().map(|h| (h.id.clone(), h)).collect();
    let paddocks: HashMap<_, _> = ctx.store().list_paddocks().await?.into_iter().map(|p| (p.id.clone(), p)).collect();
    let farm = ctx.store().get_farm().await?;
    let now = chrono::Utc::now();
    for (serial, bearer) in bearers {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT c.id, c.herd_id FROM fleet_units u JOIN collars c ON c.id = u.collar_id
             WHERE u.serial = ? AND u.state IN ('claimed', 'in_service')",
        )
        .bind(&serial)
        .fetch_optional(ctx.db())
        .await?;
        let Some((collar_id, herd_id)) = row else { continue };
        let center = herds
            .get(&herd_id)
            .and_then(|h| h.paddock_id.as_ref())
            .and_then(|id| paddocks.get(id))
            .and_then(|p| p.geometry.centroid())
            .or_else(|| farm.as_ref().map(|f| f.center))
            .unwrap_or([-96.58, 39.19]);
        let seed = serial.bytes().fold(0u64, |a, b| a.wrapping_mul(131).wrapping_add(u64::from(b)));
        let phase = now.timestamp() as f64 / 210.0 + (seed % 360) as f64;
        // A slow 15–35 m grazing orbit, with a stable per-collar offset.
        let radius_m = 15.0 + (seed % 20) as f64;
        let lat = center[1] + phase.sin() * radius_m / 111_320.0;
        let lon = center[0] + phase.cos() * radius_m / (111_320.0 * center[1].to_radians().cos().abs().max(0.2));
        let days = (now.timestamp().rem_euclid(90 * 86_400)) as f64 / 86_400.0;
        let battery = (0.98 - days * 0.004 - (seed % 7) as f64 * 0.003).clamp(0.55, 0.99);
        let class = if serial.contains("ROOK") { "direct" } else { "node" };
        let body = json!({
            "collar_id": collar_id,
            "device": {
                "fw": "0.2-hosted-sim",
                "caps": ["holes", "slots", "collar_id", "cue_mode", "episodes", "config", "sim"],
                "device_class": class
            },
            "fixes": [{"at": now, "point": [lon, lat], "accuracy_m": 3.5, "sats": 11, "hdop": 0.9}],
            "battery": battery,
            "health": {"temp_c": 24.0 + phase.sin() * 3.0, "battery_v": 3.65 + battery * 0.5}
        });
        let response = client.post(format!("{}/collar/v1/report", ctx.local_url())).bearer_auth(&bearer).json(&body).send().await?;
        if !response.status().is_success() {
            tracing::warn!(%serial, status = %response.status(), "hosted simulated report rejected");
        }
    }
    Ok(())
}
