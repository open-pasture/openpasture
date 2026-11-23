//! Hosted telemetry loop for the stable demo factory collars.
//!
//! Reports are deliberately sent over the public collar HTTP contract, not
//! inserted into analytics tables, so ingest, alerts and live events see the
//! same path as hardware.
//!
//! Boundaries go the same way: each simulated collar polls
//! `GET /collar/v1/boundary` like firmware does and acks what it gets on
//! `POST /collar/v1/ack` — `received` for a staged boundary, `applied` a few
//! seconds later for one in effect (and for a staged one once its time
//! comes). What it holds is read back from its own acks, so a restart (a
//! deploy) picks up where it left off, including versions pushed while no
//! ack was being sent. Only collars whose bearer the hosted inventory holds
//! do this; every other collar is untouched.

use std::collections::HashMap;
use std::time::Duration;

use op_core::Ctx;
use serde_json::json;

const TASK: &str = "mobile.hosted_sim";

/// How long a boundary push takes to reach the simulated collars: the
/// cellular/LoRa hop before they poll.
const PUSH_LAG: Duration = Duration::from_secs(3);
/// Most boundary polls per collar in one pass (an in-effect one plus staged ones).
const MAX_POLLS: usize = 4;

pub fn spawn(ctx: &Ctx) {
    ctx.supervise(TASK, Some(Duration::from_secs(30)), |ctx| async move {
        let client = reqwest::Client::builder().timeout(Duration::from_secs(10)).build().expect("HTTP client");
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut events = ctx.subscribe();
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                ev = events.recv() => match ev {
                    // A boundary was pushed (or a staged one took effect):
                    // the collars pick it up shortly, not on the next report.
                    Ok(op_core::Event::Boundary { .. }) | Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        tokio::time::sleep(PUSH_LAG).await;
                        while events.try_recv().is_ok() {}
                        if let Err(error) = sync_all(&ctx, &client, ack_delay).await {
                            tracing::warn!("hosted collar boundary sync: {error:#}");
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                _ = tick.tick() => {
                    ctx.beat(TASK);
                    if let Err(error) = report_all(&ctx, &client).await {
                        tracing::warn!("hosted collar simulation tick: {error:#}");
                    }
                    if let Err(error) = sync_all(&ctx, &client, ack_delay).await {
                        tracing::warn!("hosted collar boundary sync: {error:#}");
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
    let collars = claimed(ctx).await?;
    if collars.is_empty() {
        return Ok(());
    }
    let herds: HashMap<_, _> = ctx.store().list_herds().await?.into_iter().map(|h| (h.id.clone(), h)).collect();
    let paddocks: HashMap<_, _> = ctx.store().list_paddocks().await?.into_iter().map(|p| (p.id.clone(), p)).collect();
    let farm = ctx.store().get_farm().await?;
    let now = chrono::Utc::now();
    for (serial, bearer, collar_id, herd_id) in collars {
        let active = op_ingest::herd_boundaries(ctx.db(), &herd_id, now).await?.active;
        let boundary_center = active.as_ref().and_then(|b| b.geometry.centroid());
        let center = boundary_center
            .or_else(|| herds.get(&herd_id).and_then(|h| h.paddock_id.as_ref()).and_then(|id| paddocks.get(id)).and_then(|p| p.geometry.centroid()))
            .or_else(|| farm.as_ref().map(|f| f.center))
            .unwrap_or([-96.58, 39.19]);
        let seed = seed_of(&serial);
        let phase = now.timestamp() as f64 / 210.0 + (seed % 360) as f64;
        // A slow 15–35 m grazing orbit, with a stable per-collar offset.
        let radius_m = 15.0 + (seed % 20) as f64;
        let lat = center[1] + phase.sin() * radius_m / 111_320.0;
        let lon = center[0] + phase.cos() * radius_m / (111_320.0 * center[1].to_radians().cos().abs().max(0.2));
        let days = (now.timestamp().rem_euclid(90 * 86_400)) as f64 / 86_400.0;
        let battery = (0.98 - days * 0.004 - (seed % 7) as f64 * 0.003).clamp(0.55, 0.99);
        let class = if serial.contains("ROOK") { "direct" } else { "node" };
        let applied = applied_version(ctx, &collar_id, &herd_id).await?;
        let mut welfare = op_fleet::hosted::welfare(ctx, &serial)?;
        if welfare.boundary_version != applied {
            welfare.rearm(applied);
        }
        let cues = active
            .as_ref()
            .filter(|b| b.version == applied)
            .and_then(|b| {
                let cfg = op_geo::GeofenceConfig { warn_m: b.warn_m, hysteresis_m: b.hysteresis_m, ..Default::default() };
                let mut fence = op_geo::Geofence::from_polygon(cfg, &b.geometry, b.version, &op_geo::CollarLimits::V0).ok()?;
                let fix = fence.update([lon, lat], 3.5);
                cue_for_fix(&mut welfare, now.timestamp_millis(), &fix, b.warn_m).map(|cue| cue_record(now, cue, &fix, applied))
            })
            .into_iter()
            .collect::<Vec<_>>();
        let body = json!({
            "collar_id": collar_id,
            "boundary_version": (applied > 0).then_some(applied),
            "device": {
                "fw": "0.2-hosted-sim",
                "caps": ["holes", "slots", "collar_id", "cue_mode", "config", "sim"],
                "device_class": class
            },
            "fixes": [{"at": now, "point": [lon, lat], "accuracy_m": 3.5, "sats": 11, "hdop": 0.9}],
            "cues": cues,
            "battery": battery,
            "health": {"temp_c": 24.0 + phase.sin() * 3.0, "battery_v": 3.65 + battery * 0.5}
        });
        // The physical collar advances welfare state when it evaluates the
        // fix, regardless of whether its subsequent report reaches us.
        op_fleet::hosted::save_welfare(ctx, &serial, welfare)?;
        let response = client.post(format!("{}/collar/v1/report", ctx.local_url())).bearer_auth(&bearer).json(&body).send().await?;
        if !response.status().is_success() {
            tracing::warn!(%serial, status = %response.status(), "hosted simulated report rejected");
        }
    }
    Ok(())
}

struct SimCue {
    kind: &'static str,
    level: u8,
}

fn cue_record(at: chrono::DateTime<chrono::Utc>, cue: SimCue, fix: &op_geo::GeofenceResult, boundary_version: u32) -> serde_json::Value {
    json!({"at": at, "kind": cue.kind, "level": cue.level, "dur_ms": 300,
        "margin_m": fix.margin_m, "ring": fix.nearest_ring, "boundary_version": boundary_version})
}

/// Firmware cue.c's pacing and welfare transitions for one GPS fix.
fn cue_for_fix(state: &mut op_fleet::hosted::WelfareState, now_ms: i64, fix: &op_geo::GeofenceResult, warn_m: f64) -> Option<SimCue> {
    use op_geo::GeofenceState::{Inside, Outside, Warning};
    match fix.state {
        Inside => {
            if state.crossed_out {
                if state.escape_window_start_ms == 0 || now_ms - state.escape_window_start_ms >= 24 * 60 * 60 * 1000 {
                    state.escape_window_start_ms = now_ms;
                    state.escape_cycles = 1;
                } else {
                    state.escape_cycles = state.escape_cycles.saturating_add(1);
                }
                if state.escape_cycles >= 3 {
                    state.enforcement_off = true;
                    state.armed = false;
                }
            }
            state.escaping = false;
            state.outside_cues = 0;
            state.pulse_cutoff = false;
            state.crossed_out = false;
            if !state.enforcement_off {
                state.armed = true;
            }
        }
        Warning => {
            if !state.seen_outside && !state.enforcement_off {
                state.armed = true;
            }
            state.escaping = false;
        }
        Outside => {
            if state.armed && !state.enforcement_off {
                state.armed = false;
                state.escaping = true;
                state.outside_since_ms = now_ms;
                state.outside_cues = 0;
                state.pulse_cutoff = false;
                state.crossed_out = true;
            }
            state.seen_outside = true;
        }
        _ => {}
    }
    if state.enforcement_off || now_ms < state.rest_until_ms {
        return None;
    }
    let cue = if fix.state == Warning && state.armed {
        let level = (1 + (3.0 * (warn_m - fix.margin_m) / warn_m.max(f64::EPSILON)) as i32).clamp(1, 4) as u8;
        Some(SimCue { kind: "warn", level })
    } else if fix.state == Outside && state.escaping && !state.pulse_cutoff && state.outside_cues < 3 && now_ms - state.outside_since_ms < 10_000 {
        state.outside_cues += 1;
        if state.outside_cues == 3 {
            state.escaping = false;
            state.pulse_cutoff = true;
        }
        Some(SimCue { kind: "outside", level: 4 })
    } else {
        if fix.state == Outside && state.escaping {
            state.escaping = false;
            state.pulse_cutoff = true;
        }
        state.cueing = false;
        None
    }?;
    if !state.cueing {
        state.cueing = true;
        state.active_since_ms = now_ms;
    } else if now_ms - state.active_since_ms >= 20_000 {
        state.cueing = false;
        state.rest_until_ms = now_ms + 30_000;
        return None;
    }
    Some(cue)
}

/// A simulated collar's apply time: 2–6 s, stable per collar (verify the
/// signature, write the slot, rearm).
fn ack_delay(serial: &str) -> Duration {
    Duration::from_millis(2_000 + seed_of(serial) % 4_000)
}

fn seed_of(serial: &str) -> u64 {
    serial.bytes().fold(0u64, |a, b| a.wrapping_mul(131).wrapping_add(u64::from(b)))
}

/// Simulated collars claimed into a herd: (serial, bearer, collar id, herd id).
async fn claimed(ctx: &Ctx) -> anyhow::Result<Vec<(String, String, String, String)>> {
    let mut out = Vec::new();
    for (serial, bearer) in op_fleet::hosted::bearers(ctx)? {
        let row: Option<(String, String)> = sqlx::query_as(
            "SELECT c.id, c.herd_id FROM fleet_units u JOIN collars c ON c.id = u.collar_id
             WHERE u.serial = ? AND u.state IN ('claimed', 'in_service')",
        )
        .bind(&serial)
        .fetch_optional(ctx.db())
        .await?;
        if let Some((collar_id, herd_id)) = row {
            out.push((serial, bearer, collar_id, herd_id));
        }
    }
    Ok(out)
}

/// Every simulated collar fetches and acks the boundaries it is due, at once.
pub(crate) async fn sync_all(ctx: &Ctx, client: &reqwest::Client, delay: fn(&str) -> Duration) -> anyhow::Result<()> {
    let collars = claimed(ctx).await?;
    let runs = collars.iter().map(|(serial, bearer, collar_id, herd_id)| async move {
        if let Err(error) = sync_collar(ctx, client, bearer, collar_id, herd_id, delay(serial)).await {
            tracing::warn!(%serial, "hosted simulated boundary sync: {error:#}");
        }
    });
    futures::future::join_all(runs).await;
    Ok(())
}

/// One collar, as firmware does it: a staged boundary it holds whose time
/// has come is applied; then it asks for anything newer than what it holds
/// and acks each one (`received` when staged, `applied` when in effect).
async fn sync_collar(ctx: &Ctx, client: &reqwest::Client, bearer: &str, collar_id: &str, herd_id: &str, delay: Duration) -> anyhow::Result<()> {
    let base = ctx.local_url();
    let now = chrono::Utc::now();
    // Staged boundaries are applied only on the first simulated GPS fix at or
    // after their effective time, exactly as the firmware slot tick does.
    let staged: Vec<(String, i64, Option<String>)> = sqlx::query_as(
        "SELECT b.id, b.version, b.effective_at FROM acks a JOIN boundaries b ON b.id = a.command_id
         WHERE a.collar_id = ?1 AND a.herd_id = ?2 AND a.status = 'received'
           AND NOT EXISTS (SELECT 1 FROM acks x WHERE x.collar_id = ?1 AND x.version = a.version AND x.status IN ('applied', 'rejected'))
         ORDER BY b.version DESC",
    )
    .bind(collar_id)
    .bind(herd_id)
    .fetch_all(ctx.db())
    .await?;
    let applied = applied_version(ctx, collar_id, herd_id).await?;
    let due = staged.iter().find(|(_, v, at)| *v as u32 > applied && at.as_deref().and_then(|t| op_core::time::from_db(t).ok()).is_none_or(|t| t <= now));
    if let Some((command_id, version, effective_at)) = due {
        let effective_at = effective_at.as_deref().and_then(|t| op_core::time::from_db(t).ok()).unwrap_or(now);
        let fix_at: Option<String> = sqlx::query_scalar("SELECT at FROM fixes WHERE collar_id = ? AND at >= ? ORDER BY at LIMIT 1")
            .bind(collar_id)
            .bind(op_core::time::to_db(&effective_at))
            .fetch_optional(ctx.db())
            .await?;
        if let Some(fix_at) = fix_at {
            let at = op_core::time::from_db(&fix_at)?;
            post_ack(client, &base, bearer, collar_id, command_id, *version as u32, "applied", None, at).await?;
        }
    }

    for _ in 0..MAX_POLLS {
        let have = held_version(ctx, collar_id, herd_id).await?;
        let mut url = format!("{base}/collar/v1/boundary");
        if have > 0 {
            url.push_str(&format!("?have={have}"));
        }
        let response = client.get(url).bearer_auth(bearer).send().await?;
        match response.status() {
            reqwest::StatusCode::NO_CONTENT => break,
            s if s.is_success() => {}
            s => {
                // Refused by the safety gate for this collar, say: nothing to ack.
                tracing::warn!(collar = %collar_id, status = %s, "hosted simulated boundary poll refused");
                break;
            }
        }
        let wire = response.bytes().await?;
        let untrusted: serde_json::Value = serde_json::from_slice(&wire).unwrap_or_default();
        let (Some(command_id), Some(version)) = (untrusted["command_id"].as_str().map(str::to_owned), untrusted["version"].as_u64()) else {
            anyhow::bail!("boundary command without command_id or version");
        };
        if op_protocol::verify_wire(&wire, &ctx.public_key()).is_err() {
            post_ack(client, &base, bearer, collar_id, &command_id, version as u32, "rejected", Some("bad_sig"), chrono::Utc::now()).await?;
            break;
        }
        let command = untrusted;
        let version = version as u32;
        if version <= have {
            break;
        }
        let effective_at = command["effective_at"].as_str().and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.to_utc());
        let now = chrono::Utc::now();
        if effective_at.is_some_and(|t| t > now) {
            post_ack(client, &base, bearer, collar_id, &command_id, version, "received", None, now).await?;
        } else {
            tokio::time::sleep(delay).await;
            post_ack(client, &base, bearer, collar_id, &command_id, version, "applied", None, chrono::Utc::now()).await?;
        }
    }
    Ok(())
}

/// The highest version this collar applied in this herd (0: none).
async fn applied_version(ctx: &Ctx, collar_id: &str, herd_id: &str) -> anyhow::Result<u32> {
    let v: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM acks WHERE collar_id = ? AND herd_id = ? AND status = 'applied'")
        .bind(collar_id)
        .bind(herd_id)
        .fetch_one(ctx.db())
        .await?;
    Ok(v.unwrap_or(0) as u32)
}

/// The highest version this collar holds in this herd, applied or staged (0: none).
async fn held_version(ctx: &Ctx, collar_id: &str, herd_id: &str) -> anyhow::Result<u32> {
    let v: Option<i64> = sqlx::query_scalar("SELECT MAX(version) FROM acks WHERE collar_id = ? AND herd_id = ? AND status IN ('received', 'applied')")
        .bind(collar_id)
        .bind(herd_id)
        .fetch_one(ctx.db())
        .await?;
    Ok(v.unwrap_or(0) as u32)
}

#[allow(clippy::too_many_arguments)]
async fn post_ack(
    client: &reqwest::Client,
    base: &str,
    bearer: &str,
    collar_id: &str,
    command_id: &str,
    version: u32,
    status: &str,
    code: Option<&str>,
    at: chrono::DateTime<chrono::Utc>,
) -> anyhow::Result<()> {
    let mut body = json!({ "collar_id": collar_id, "command_id": command_id, "version": version, "status": status, "at": at });
    if let Some(code) = code {
        body["code"] = json!(code);
    }
    let response = client.post(format!("{base}/collar/v1/ack")).bearer_auth(bearer).json(&body).send().await?;
    if !response.status().is_success() {
        anyhow::bail!("ack {status} v{version} rejected: {}", response.status());
    }
    tracing::info!(collar = %collar_id, version, status, "hosted simulated collar acked boundary");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    const CENTER: [f64; 2] = [-92.405, 38.125];

    /// A rectangle `half_m` either side of CENTER.
    fn square(half_m: f64) -> Value {
        let dlat = half_m / 111_320.0;
        let dlon = half_m / (111_320.0 * CENTER[1].to_radians().cos());
        let (x, y) = (CENTER[0], CENTER[1]);
        json!({"type": "Polygon", "coordinates": [[[x - dlon, y - dlat], [x + dlon, y - dlat], [x + dlon, y + dlat], [x - dlon, y + dlat], [x - dlon, y - dlat]]]})
    }

    fn no_delay(_: &str) -> Duration {
        Duration::ZERO
    }

    struct Api {
        base: String,
        http: reqwest::Client,
    }

    impl Api {
        async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (reqwest::StatusCode, Value) {
            let mut req = self.http.request(method, format!("{}{path}", self.base));
            if let Some(b) = body {
                req = req.json(&b);
            }
            let res = req.send().await.unwrap();
            let status = res.status();
            let text = res.text().await.unwrap();
            (status, serde_json::from_str(&text).unwrap_or(Value::String(text)))
        }

        async fn ok(&self, method: reqwest::Method, path: &str, body: Value) -> Value {
            let (s, v) = self.call(method, path, Some(body)).await;
            assert!(s.is_success(), "{path}: {s} {v}");
            v
        }

        async fn herd(&self, name: &str) -> String {
            let pad = self.ok(reqwest::Method::POST, "/api/paddocks", json!({"name": format!("{name} paddock"), "geometry": square(150.0)})).await;
            let h = self.ok(reqwest::Method::POST, "/api/herds", json!({"name": name, "species": "cattle", "count": 0, "paddock_id": pad["id"]})).await;
            h["id"].as_str().unwrap().to_owned()
        }

        async fn boundary_not_applied(&self) -> Vec<Value> {
            let (_, all) = self.call(reqwest::Method::GET, "/api/alerts?status=all", None).await;
            let list = all.as_array().cloned().or_else(|| all["alerts"].as_array().cloned()).unwrap_or_default();
            list.into_iter().filter(|a| a["kind"] == "boundary_not_applied").collect()
        }
    }

    fn open_for<'a>(alerts: &'a [Value], herd: &str) -> Vec<&'a Value> {
        alerts.iter().filter(|a| a["herd_id"] == herd && a["status"] != "resolved").collect()
    }

    /// Simulated collars ack a pushed boundary through the collar API (also
    /// one pushed before they ever acked, as on a server that ran the old
    /// simulator), their open `boundary_not_applied` resolves, and a real
    /// collar that never acks still raises it.
    #[tokio::test]
    async fn simulated_collars_ack_boundaries_and_real_ones_still_alert() {
        let dir = tempfile::tempdir().unwrap();
        let handle = crate::serve(crate::ServeOptions { data_dir: Some(dir.path().into()), free_port: true, ..Default::default() }).await.unwrap();
        let ctx = handle.ctx().clone();
        op_fleet::hosted::seed_inventory(&ctx).await.unwrap();
        let token = ctx.settings().await.unwrap().server.app_token;
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("authorization", format!("Bearer {token}").parse().unwrap());
        let api = Api { base: handle.url().to_owned(), http: reqwest::Client::builder().default_headers(headers).build().unwrap() };
        let client = reqwest::Client::new();

        // Alert timeout shortened to its minimum (1 min); evaluations run 70 s ahead.
        api.ok(
            reqwest::Method::PUT,
            "/api/alerts/rules",
            json!({"rules": {"boundary_not_applied": {"after_min": 1}}, "policy": {"start_grace_min": 0, "clear_after_min": 0}}),
        )
        .await;

        api.ok(reqwest::Method::POST, "/api/farm", json!({"name": "Home", "timezone": "UTC", "center": CENTER})).await;
        let sim_herd = api.herd("Demo").await;
        let real_herd = api.herd("Real").await;
        let cards = op_fleet::hosted::cards();
        let mut sim_collars = Vec::new();
        for card in [&cards[0], &cards[8]] {
            let c = api.ok(reqwest::Method::POST, "/api/fleet/claim", json!({"card": card.card, "herd_id": sim_herd})).await;
            sim_collars.push(c["collar"]["id"].as_str().unwrap().to_owned());
        }
        let real = api.ok(reqwest::Method::POST, "/api/collars", json!({"herd_id": real_herd})).await;
        let real_key = real["key"].as_str().unwrap().to_owned();
        let real_id = real["collar"]["id"].as_str().unwrap().to_owned();
        // The real collar reports (so it is not silent) and never acks.
        let report = json!({"fixes": [{"at": chrono::Utc::now(), "point": CENTER, "accuracy_m": 3.0, "sats": 9}], "battery": 0.9,
            "device": {"fw": "0.2.0", "caps": ["holes", "slots", "collar_id", "cue_mode", "episodes"]}});
        let res = client.post(format!("{}/collar/v1/report", api.base)).bearer_auth(&real_key).json(&report).send().await.unwrap();
        assert!(res.status().is_success(), "{}", res.text().await.unwrap());

        // The simulated collars report; then boundaries go out and the old
        // simulator (reports only) leaves them unacked.
        report_all(&ctx, &client).await.unwrap();
        api.ok(reqwest::Method::POST, &format!("/api/herds/{sim_herd}/boundary"), json!({"geometry": square(100.0)})).await;
        api.ok(reqwest::Method::POST, &format!("/api/herds/{real_herd}/boundary"), json!({"geometry": square(100.0)})).await;
        op_alerts::engine::evaluate_all(&ctx, chrono::Utc::now() + chrono::Duration::seconds(70)).await.unwrap();
        let alerts = api.boundary_not_applied().await;
        assert!(!open_for(&alerts, &sim_herd).is_empty(), "pending before any ack: {alerts:#?}");
        assert!(!open_for(&alerts, &real_herd).is_empty(), "{alerts:#?}");

        // The new simulator: they fetch and ack it through /collar/v1.
        sync_all(&ctx, &client, no_delay).await.unwrap();
        let status = api.ok(reqwest::Method::GET, &format!("/api/herds/{sim_herd}/boundary"), Value::Null).await;
        let v1 = status["active"]["version"].as_u64().expect("boundary version");
        for id in &sim_collars {
            let (version, st): (i64, String) =
                sqlx::query_as("SELECT version, status FROM collar_boundary_state WHERE collar_id = ?").bind(id).fetch_one(ctx.db()).await.unwrap();
            assert_eq!((version as u64, st.as_str()), (v1, "applied"), "{id}");
        }
        // The herd's boundary status: every collar applied the active version.
        let acks = status["acks"].as_array().unwrap();
        assert_eq!(acks.len(), 2, "{status:#}");
        assert!(acks.iter().all(|a| a["version"] == v1 && a["status"] == "applied"), "{status:#}");
        op_alerts::engine::evaluate_all(&ctx, chrono::Utc::now() + chrono::Duration::seconds(70)).await.unwrap();
        let alerts = api.boundary_not_applied().await;
        assert!(open_for(&alerts, &sim_herd).is_empty(), "simulated herd's alert resolved once acked: {alerts:#?}");
        assert!(alerts.iter().any(|a| a["herd_id"] == sim_herd && a["status"] == "resolved"), "kept, as resolved: {alerts:#?}");
        assert!(!open_for(&alerts, &real_herd).is_empty(), "the real collar still alerts: {alerts:#?}");

        // Nothing new to fetch: the next pass is a quiet poll (204).
        sync_all(&ctx, &client, no_delay).await.unwrap();
        let acks: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM acks WHERE collar_id = ?").bind(&sim_collars[0]).fetch_one(ctx.db()).await.unwrap();
        assert_eq!(acks, 1);

        // A staged boundary: acked received now, applied once its time comes.
        let at = chrono::Utc::now() + chrono::Duration::seconds(2);
        api.ok(
            reqwest::Method::POST,
            &format!("/api/herds/{sim_herd}/boundary"),
            json!({"geometry": square(90.0), "effective_at": at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)}),
        )
        .await;
        let status = api.ok(reqwest::Method::GET, &format!("/api/herds/{sim_herd}/boundary"), Value::Null).await;
        let v2 = status["pending"]["version"].as_u64().expect("staged version");
        sync_all(&ctx, &client, no_delay).await.unwrap();
        let st: String =
            sqlx::query_scalar("SELECT status FROM collar_boundary_state WHERE collar_id = ?").bind(&sim_collars[0]).fetch_one(ctx.db()).await.unwrap();
        assert_eq!(st, "received");
        tokio::time::sleep(Duration::from_millis(3_200)).await;
        report_all(&ctx, &client).await.unwrap();
        sync_all(&ctx, &client, no_delay).await.unwrap();
        for id in &sim_collars {
            let (version, st): (i64, String) =
                sqlx::query_as("SELECT version, status FROM collar_boundary_state WHERE collar_id = ?").bind(id).fetch_one(ctx.db()).await.unwrap();
            assert_eq!((version as u64, st.as_str()), (v2, "applied"), "{id}");
            let (ack_at, fix_at): (String, String) = sqlx::query_as(
                "SELECT a.at, f.at FROM acks a JOIN fixes f ON f.collar_id = a.collar_id AND f.at = a.at
                 WHERE a.collar_id = ? AND a.version = ? AND a.status = 'applied'",
            )
            .bind(id)
            .bind(v2 as i64)
            .fetch_one(ctx.db())
            .await
            .unwrap();
            assert_eq!(ack_at, fix_at);
        }
        op_alerts::engine::evaluate_all(&ctx, chrono::Utc::now() + chrono::Duration::seconds(70)).await.unwrap();
        assert!(open_for(&api.boundary_not_applied().await, &sim_herd).is_empty());

        // The real collar was never touched by the simulator.
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM acks WHERE collar_id = ?").bind(&real_id).fetch_one(ctx.db()).await.unwrap();
        assert_eq!(n, 0);
        handle.shutdown().await.unwrap();
    }

    fn fix(state: op_geo::GeofenceState, margin_m: f64) -> op_geo::GeofenceResult {
        op_geo::GeofenceResult { state, margin_m, nearest_ring: 2, ..Default::default() }
    }

    #[test]
    fn three_outside_cues_then_silent_until_clear_inside() {
        let mut state = op_fleet::hosted::WelfareState::default();
        assert!(cue_for_fix(&mut state, 1, &fix(op_geo::GeofenceState::Inside, 10.0), 5.0).is_none());
        for at in 2..=4 {
            assert_eq!(cue_for_fix(&mut state, at, &fix(op_geo::GeofenceState::Outside, -1.0), 5.0).unwrap().kind, "outside");
        }
        assert!(state.pulse_cutoff);
        assert!(cue_for_fix(&mut state, 5, &fix(op_geo::GeofenceState::Outside, -2.0), 5.0).is_none());
        assert!(cue_for_fix(&mut state, 6, &fix(op_geo::GeofenceState::Warning, 2.0), 5.0).is_none());
        assert!(cue_for_fix(&mut state, 7, &fix(op_geo::GeofenceState::Inside, 10.0), 5.0).is_none());
        assert!(cue_for_fix(&mut state, 8, &fix(op_geo::GeofenceState::Warning, 2.0), 5.0).is_some());
    }

    #[test]
    fn third_escape_return_cycle_shuts_enforcement_off() {
        let mut state = op_fleet::hosted::WelfareState::default();
        for cycle in 0..3 {
            let at = 100 + cycle * 10;
            cue_for_fix(&mut state, at, &fix(op_geo::GeofenceState::Inside, 10.0), 5.0);
            cue_for_fix(&mut state, at + 1, &fix(op_geo::GeofenceState::Outside, -1.0), 5.0);
            cue_for_fix(&mut state, at + 2, &fix(op_geo::GeofenceState::Inside, 10.0), 5.0);
        }
        assert_eq!(state.escape_cycles, 3);
        assert!(state.enforcement_off);
        assert!(cue_for_fix(&mut state, 200, &fix(op_geo::GeofenceState::Warning, 2.0), 5.0).is_none());
    }

    #[test]
    fn cue_wire_shape_has_version_and_ring_but_no_point() {
        let record = cue_record(chrono::Utc::now(), SimCue { kind: "warn", level: 2 }, &fix(op_geo::GeofenceState::Warning, 1.5), 57);
        assert_eq!(record["boundary_version"], 57);
        assert_eq!(record["ring"], 2);
        assert!(record.get("point").is_none());
    }

    #[test]
    fn simulator_accepts_server_signature_and_rejects_tampering() {
        let key = op_protocol::SigningKey::from_bytes(&[9; 32]);
        let mut command = op_protocol::BoundaryCommand {
            command_id: "bnd_test".into(),
            version: 1,
            boundary: vec![[0.0, 0.0], [0.001, 0.0], [0.0, 0.001]],
            ..Default::default()
        };
        op_protocol::sign_command(&mut command, &key);
        let good = serde_json::to_vec(&command).unwrap();
        assert!(op_protocol::verify_wire(&good, &key.verifying_key()).is_ok());
        let bad = String::from_utf8(good).unwrap().replace("\"version\":1", "\"version\":2");
        assert_eq!(op_protocol::verify_wire(bad.as_bytes(), &key.verifying_key()), Err(op_protocol::RejectCode::BadSig));
    }

    async fn boundary_peer(wire: Vec<u8>) -> (String, std::sync::Arc<std::sync::Mutex<Vec<Value>>>) {
        use axum::Router;
        use axum::body::Bytes;
        use axum::extract::State;
        use axum::http::StatusCode;
        use axum::routing::{get, post};
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        #[derive(Clone)]
        struct Peer {
            wire: Arc<Vec<u8>>,
            served: Arc<AtomicBool>,
            acks: Arc<std::sync::Mutex<Vec<Value>>>,
        }
        async fn boundary(State(peer): State<Peer>) -> (StatusCode, Bytes) {
            if peer.served.swap(true, Ordering::SeqCst) { (StatusCode::NO_CONTENT, Bytes::new()) } else { (StatusCode::OK, Bytes::copy_from_slice(&peer.wire)) }
        }
        async fn ack(State(peer): State<Peer>, axum::Json(body): axum::Json<Value>) -> StatusCode {
            peer.acks.lock().unwrap().push(body);
            StatusCode::NO_CONTENT
        }

        let acks = Arc::new(std::sync::Mutex::new(Vec::new()));
        let peer = Peer { wire: Arc::new(wire), served: Arc::new(AtomicBool::new(false)), acks: acks.clone() };
        let app = Router::new().route("/collar/v1/boundary", get(boundary)).route("/collar/v1/ack", post(ack)).with_state(peer);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        (format!("http://{address}"), acks)
    }

    #[tokio::test]
    async fn bad_signature_is_rejected_and_good_signature_is_applied() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = op_core::Ctx::open(dir.path()).await.unwrap();
        let mut command = op_protocol::BoundaryCommand {
            command_id: "bnd_test".into(),
            version: 1,
            boundary: vec![[0.0, 0.0], [0.001, 0.0], [0.0, 0.001]],
            ..Default::default()
        };
        op_protocol::sign_command(&mut command, ctx.signing_key());
        let good = serde_json::to_vec(&command).unwrap();
        let bad = String::from_utf8(good.clone()).unwrap().replace("\"version\":1", "\"version\":2").into_bytes();

        let (url, acks) = boundary_peer(bad).await;
        ctx.set_local_url(url);
        sync_collar(&ctx, &reqwest::Client::new(), "bearer", "col_test", "herd_test", Duration::ZERO).await.unwrap();
        let got = acks.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["status"], "rejected");
        assert_eq!(got[0]["code"], "bad_sig");
        assert!(!got.iter().any(|a| a["status"] == "applied"));

        let (url, acks) = boundary_peer(good).await;
        ctx.set_local_url(url);
        sync_collar(&ctx, &reqwest::Client::new(), "bearer", "col_test", "herd_test", Duration::ZERO).await.unwrap();
        let got = acks.lock().unwrap().clone();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0]["status"], "applied");
        assert!(got[0].get("code").is_none());
    }
}
