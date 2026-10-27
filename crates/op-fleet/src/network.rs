//! The herd network. Most collars are nodes (LoRa only); a few anchors also
//! carry LTE, take turns as the herd's link, broadcast boundaries and relay
//! the herd's reports. Fixed anchors on posts are optional. Collars that
//! talk to the server themselves over LTE (protocol v1) are `direct`.
//!
//! Roles come from what the network reports (v2 membership), else the
//! unit's device class, else `direct` for a collar that reports by itself. Link quality fills from protocol v2 reports
//! through [`record_uplink`] and [`record_beacon`], which the collar ingest
//! calls.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::get;
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use op_core::time::{now, opt_from_db, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Collar, Ctx, id};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::Row;

use crate::units::Unit;

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/fleet/network/{herd_id}", get(herd_network))
        .route("/api/fleet/anchors", get(list_anchors).post(create_anchor))
        .route("/api/fleet/anchors/{id}", axum::routing::patch(update_anchor).delete(delete_anchor))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Anchor,
    Node,
    Direct,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Quality {
    Good,
    Fair,
    Poor,
    Lost,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Member {
    pub collar_id: String,
    pub name: String,
    pub role: Role,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_heard_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub via: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rssi_dbm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snr_db: Option<f64>,
    pub quality: Quality,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AnchorView {
    pub id: String,
    pub name: String,
    pub source: &'static str,
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_beacon_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub battery: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lte_rsrp_dbm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Gap {
    pub collar_id: String,
    pub name: String,
    pub since: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub point: Option<[f64; 2]>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct HerdNetwork {
    pub herd_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub active_anchor: Option<String>,
    pub anchors: Vec<AnchorView>,
    pub members: Vec<Member>,
    pub gaps: Vec<Gap>,
}

/// A collar's part when the network hasn't said: from its unit's class
/// (protocol v2 device classes), else `direct` once it has reported (a v1
/// collar talks to the server itself).
pub fn role_from(class: Option<&str>, heard: bool) -> Role {
    match class {
        Some("anchor" | "station") => Role::Anchor,
        Some("node") => Role::Node,
        Some("direct") => Role::Direct,
        _ if heard => Role::Direct,
        _ => Role::Unknown,
    }
}

/// LoRa link quality from the last uplink: good with margin to spare, fair
/// near the demodulation floor (SNR about −7.5 dB at SF7), poor below it.
pub fn quality(rssi_dbm: Option<f64>, snr_db: Option<f64>) -> Quality {
    match (rssi_dbm, snr_db) {
        (_, Some(snr)) if snr < -7.5 => Quality::Poor,
        (Some(rssi), _) if rssi < -120.0 => Quality::Poor,
        (Some(rssi), Some(snr)) if rssi >= -110.0 && snr >= 0.0 => Quality::Good,
        (None, None) => Quality::Unknown,
        _ => Quality::Fair,
    }
}

/// How long a collar can go unheard before it counts as lost: three report
/// intervals, at least 15 minutes.
async fn lost_after(ctx: &Ctx) -> Duration {
    let v = ctx.store().get_setting_json("collars.config").await.ok().flatten().unwrap_or(Value::Null);
    let report_s = v.get("report_s").and_then(Value::as_i64).unwrap_or(60);
    Duration::seconds(report_s * 3).max(Duration::minutes(15))
}

#[derive(Debug, Default)]
struct LinkRow {
    role: Option<String>,
    heard_at: Option<DateTime<Utc>>,
    via: Option<String>,
    rssi_dbm: Option<f64>,
    snr_db: Option<f64>,
    beacon_at: Option<DateTime<Utc>>,
    active: bool,
    lte_rsrp_dbm: Option<f64>,
}

async fn link_row(ctx: &Ctx, collar_id: &str) -> anyhow::Result<Option<LinkRow>> {
    let row = sqlx::query("SELECT * FROM fleet_links WHERE collar_id = ?").bind(collar_id).fetch_optional(ctx.db()).await?;
    row.map(|r| -> anyhow::Result<LinkRow> {
        Ok(LinkRow {
            role: r.try_get("role")?,
            heard_at: opt_from_db(r.try_get("heard_at")?)?,
            via: r.try_get("via")?,
            rssi_dbm: r.try_get("rssi_dbm")?,
            snr_db: r.try_get("snr_db")?,
            beacon_at: opt_from_db(r.try_get("beacon_at")?)?,
            active: r.try_get::<i64, _>("active")? != 0,
            lte_rsrp_dbm: r.try_get("lte_rsrp_dbm")?,
        })
    })
    .transpose()
}

fn member_of(collar: &Collar, unit: Option<&Unit>, link: Option<&LinkRow>, lost_after: Duration, at: DateTime<Utc>) -> Member {
    let reported = link.and_then(|l| l.role.as_deref()).and_then(|r| serde_json::from_value::<Role>(Value::String(r.into())).ok());
    let role = reported.unwrap_or_else(|| role_from(unit.map(|u| u.device_class.as_str()), collar.last_seen.is_some()));
    let heard = link.and_then(|l| l.heard_at).or(collar.last_seen);
    let mut q = match role {
        Role::Anchor | Role::Node => quality(link.and_then(|l| l.rssi_dbm), link.and_then(|l| l.snr_db)),
        _ => Quality::Unknown,
    };
    if heard.is_some_and(|h| at - h > lost_after) {
        q = Quality::Lost;
    }
    Member {
        collar_id: collar.id.clone(),
        name: collar.name.clone(),
        role,
        serial: unit.map(|u| u.serial.clone()),
        last_heard_at: heard,
        via: link.and_then(|l| l.via.clone()),
        rssi_dbm: link.and_then(|l| l.rssi_dbm),
        snr_db: link.and_then(|l| l.snr_db),
        quality: q,
    }
}

/// One collar's place in its herd's network.
pub async fn member(ctx: &Ctx, collar: &Collar, unit: Option<&Unit>) -> anyhow::Result<Member> {
    let link = link_row(ctx, &collar.id).await?;
    Ok(member_of(collar, unit, link.as_ref(), lost_after(ctx).await, now()))
}

/// `GET /api/fleet/network/:herd_id`.
async fn herd_network(State(ctx): State<Ctx>, Path(herd_id): Path<String>) -> ApiResult<Json<HerdNetwork>> {
    if ctx.store().get_herd(&herd_id).await?.is_none() {
        return Err(ApiError::not_found("No such herd."));
    }
    Ok(Json(network(&ctx, &herd_id).await?))
}

pub async fn network(ctx: &Ctx, herd_id: &str) -> anyhow::Result<HerdNetwork> {
    let at = now();
    let lost = lost_after(ctx).await;
    let collars: Vec<Collar> = sqlx::query("SELECT * FROM collars WHERE herd_id = ? ORDER BY name, id")
        .bind(herd_id)
        .fetch_all(ctx.db())
        .await?
        .iter()
        .map(op_core::store::collar_from_row)
        .collect::<anyhow::Result<_>>()?;
    let mut members = Vec::with_capacity(collars.len());
    let mut anchors = Vec::new();
    let mut gaps = Vec::new();
    let mut active_anchor = None;
    for c in &collars {
        let unit = crate::units::for_collar(ctx, &c.id).await?;
        let link = link_row(ctx, &c.id).await?;
        let m = member_of(c, unit.as_ref(), link.as_ref(), lost, at);
        if m.role == Role::Anchor {
            let active = link.as_ref().is_some_and(|l| l.active);
            if active {
                active_anchor = Some(c.id.clone());
            }
            anchors.push(AnchorView {
                id: c.id.clone(),
                name: c.name.clone(),
                source: "collar",
                active,
                last_beacon_at: link.as_ref().and_then(|l| l.beacon_at),
                battery: c.battery,
                lte_rsrp_dbm: link.as_ref().and_then(|l| l.lte_rsrp_dbm),
                serial: unit.as_ref().map(|u| u.serial.clone()),
            });
        }
        if m.role == Role::Node
            && m.quality == Quality::Lost
            && let Some(since) = m.last_heard_at
        {
            gaps.push(Gap { collar_id: c.id.clone(), name: c.name.clone(), since, point: c.last_fix.as_ref().map(|f| f.point) });
        }
        members.push(m);
    }
    for a in fixed_anchors(ctx).await? {
        if !a.herd_ids.is_empty() && !a.herd_ids.iter().any(|h| h == herd_id) {
            continue;
        }
        let link = link_row(ctx, &a.id).await?;
        let active = link.as_ref().is_some_and(|l| l.active);
        if active && active_anchor.is_none() {
            active_anchor = Some(a.id.clone());
        }
        anchors.push(AnchorView {
            id: a.id.clone(),
            name: a.name.clone(),
            source: "fixed",
            active,
            last_beacon_at: link.as_ref().and_then(|l| l.beacon_at),
            battery: None,
            lte_rsrp_dbm: link.as_ref().and_then(|l| l.lte_rsrp_dbm),
            serial: a.serial.clone(),
        });
    }
    Ok(HerdNetwork { herd_id: herd_id.to_owned(), active_anchor, anchors, members, gaps })
}

// ---- hooks for the collar ingest -----------------------------------------------------------

/// A node's report as an anchor relayed it (or an anchor's own).
#[derive(Debug, Clone, Default)]
pub struct Uplink {
    pub collar_id: String,
    /// The role the collar reports for itself, when it does.
    pub role: Option<Role>,
    /// The anchor (collar id or fixed anchor id) it came through; `None` for its own LTE.
    pub via: Option<String>,
    pub rssi_dbm: Option<f64>,
    pub snr_db: Option<f64>,
    pub at: DateTime<Utc>,
}

fn role_str(r: Role) -> String {
    serde_json::to_value(r).ok().and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_default()
}

/// Keep a collar's link current. Cheap: one upsert.
pub async fn record_uplink(ctx: &Ctx, u: &Uplink) -> anyhow::Result<()> {
    let herd: Option<String> = sqlx::query_scalar("SELECT herd_id FROM collars WHERE id = ?").bind(&u.collar_id).fetch_optional(ctx.db()).await?;
    sqlx::query(
        "INSERT INTO fleet_links (collar_id, herd_id, role, heard_at, via, rssi_dbm, snr_db, updated_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
         ON CONFLICT(collar_id) DO UPDATE SET herd_id = excluded.herd_id, role = COALESCE(excluded.role, fleet_links.role),
           heard_at = excluded.heard_at, via = excluded.via, rssi_dbm = excluded.rssi_dbm, snr_db = excluded.snr_db, updated_at = excluded.updated_at",
    )
    .bind(&u.collar_id)
    .bind(herd)
    .bind(u.role.map(role_str))
    .bind(to_db(&u.at))
    .bind(&u.via)
    .bind(u.rssi_dbm)
    .bind(u.snr_db)
    .bind(to_db(&now()))
    .execute(ctx.db())
    .await?;
    Ok(())
}

/// An anchor's beacon: it is the herd's link now, and the others in the herd aren't.
#[derive(Debug, Clone)]
pub struct Beacon {
    /// Collar id or fixed anchor id.
    pub anchor_id: String,
    pub herd_id: String,
    pub lte_rsrp_dbm: Option<f64>,
    pub at: DateTime<Utc>,
}

pub async fn record_beacon(ctx: &Ctx, b: &Beacon) -> anyhow::Result<()> {
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let at = to_db(&now());
    sqlx::query("UPDATE fleet_links SET active = 0, updated_at = ? WHERE herd_id = ? AND collar_id != ? AND active = 1").bind(&at).bind(&b.herd_id).bind(&b.anchor_id).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO fleet_links (collar_id, herd_id, role, beacon_at, active, lte_rsrp_dbm, heard_at, updated_at) VALUES (?1, ?2, 'anchor', ?3, 1, ?4, ?3, ?5)
         ON CONFLICT(collar_id) DO UPDATE SET herd_id = excluded.herd_id, role = 'anchor', beacon_at = excluded.beacon_at, active = 1,
           lte_rsrp_dbm = COALESCE(excluded.lte_rsrp_dbm, fleet_links.lte_rsrp_dbm), heard_at = excluded.heard_at, updated_at = excluded.updated_at",
    )
    .bind(&b.anchor_id)
    .bind(&b.herd_id)
    .bind(to_db(&b.at))
    .bind(b.lte_rsrp_dbm)
    .bind(&at)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

// ---- fixed anchors -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FixedAnchor {
    pub id: String,
    pub name: String,
    pub point: [f64; 2],
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    pub herd_ids: Vec<String>,
    pub created_at: DateTime<Utc>,
}

async fn fixed_anchors(ctx: &Ctx) -> anyhow::Result<Vec<FixedAnchor>> {
    sqlx::query("SELECT * FROM fleet_fixed_anchors ORDER BY name, id")
        .fetch_all(ctx.db())
        .await?
        .iter()
        .map(|r| -> anyhow::Result<FixedAnchor> {
            Ok(FixedAnchor {
                id: r.try_get("id")?,
                name: r.try_get("name")?,
                point: [r.try_get("lon")?, r.try_get("lat")?],
                serial: r.try_get("serial")?,
                herd_ids: serde_json::from_str(&r.try_get::<String, _>("herd_ids")?).unwrap_or_default(),
                created_at: op_core::time::from_db(&r.try_get::<String, _>("created_at")?)?,
            })
        })
        .collect()
}

async fn list_anchors(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<FixedAnchor>>> {
    Ok(Json(fixed_anchors(&ctx).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct AnchorBody {
    name: Option<String>,
    point: Option<[f64; 2]>,
    serial: Option<String>,
    herd_ids: Option<Vec<String>>,
}

async fn check_anchor(ctx: &Ctx, b: &AnchorBody) -> ApiResult<()> {
    if let Some(n) = &b.name
        && (n.trim().is_empty() || n.len() > 100)
    {
        return Err(ApiError::bad_request("An anchor needs a name of 1–100 characters."));
    }
    if let Some([lon, lat]) = b.point
        && !((-180.0..=180.0).contains(&lon) && (-90.0..=90.0).contains(&lat))
    {
        return Err(ApiError::bad_request("point is [longitude, latitude]."));
    }
    if let Some(s) = &b.serial
        && crate::units::get_unit(ctx, s).await?.is_none()
    {
        return Err(ApiError::bad_request(format!("No unit with serial {s}.")));
    }
    for h in b.herd_ids.iter().flatten() {
        if ctx.store().get_herd(h).await?.is_none() {
            return Err(ApiError::bad_request("No such herd."));
        }
    }
    Ok(())
}

async fn one_anchor(ctx: &Ctx, id: &str) -> ApiResult<FixedAnchor> {
    fixed_anchors(ctx).await?.into_iter().find(|a| a.id == id).ok_or_else(|| ApiError::not_found("No such anchor."))
}

async fn create_anchor(State(ctx): State<Ctx>, ApiJson(b): ApiJson<AnchorBody>) -> ApiResult<(StatusCode, Json<FixedAnchor>)> {
    let (Some(name), Some(point)) = (&b.name, b.point) else {
        return Err(ApiError::bad_request("An anchor needs a name and a point."));
    };
    check_anchor(&ctx, &b).await?;
    let id = id::new_id("anc");
    sqlx::query("INSERT INTO fleet_fixed_anchors (id, name, lon, lat, serial, herd_ids, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(&id)
        .bind(name.trim())
        .bind(point[0])
        .bind(point[1])
        .bind(&b.serial)
        .bind(serde_json::to_string(&b.herd_ids.clone().unwrap_or_default()).map_err(anyhow::Error::from)?)
        .bind(to_db(&now()))
        .execute(ctx.db())
        .await?;
    Ok((StatusCode::CREATED, Json(one_anchor(&ctx, &id).await?)))
}

async fn update_anchor(State(ctx): State<Ctx>, Path(id): Path<String>, ApiJson(b): ApiJson<AnchorBody>) -> ApiResult<Json<FixedAnchor>> {
    let cur = one_anchor(&ctx, &id).await?;
    check_anchor(&ctx, &b).await?;
    let name = b.name.as_deref().map(str::trim).unwrap_or(&cur.name).to_owned();
    let point = b.point.unwrap_or(cur.point);
    let serial = if b.serial.is_some() { b.serial.clone() } else { cur.serial.clone() };
    let herds = b.herd_ids.clone().unwrap_or(cur.herd_ids);
    sqlx::query("UPDATE fleet_fixed_anchors SET name = ?, lon = ?, lat = ?, serial = ?, herd_ids = ? WHERE id = ?")
        .bind(name)
        .bind(point[0])
        .bind(point[1])
        .bind(serial)
        .bind(serde_json::to_string(&herds).map_err(anyhow::Error::from)?)
        .bind(&id)
        .execute(ctx.db())
        .await?;
    Ok(Json(one_anchor(&ctx, &id).await?))
}

async fn delete_anchor(State(ctx): State<Ctx>, Path(id): Path<String>) -> ApiResult<StatusCode> {
    let n = sqlx::query("DELETE FROM fleet_fixed_anchors WHERE id = ?").bind(&id).execute(ctx.db()).await?.rows_affected();
    if n == 0 {
        return Err(ApiError::not_found("No such anchor."));
    }
    sqlx::query("DELETE FROM fleet_links WHERE collar_id = ?").bind(&id).execute(ctx.db()).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_come_from_the_class() {
        assert_eq!(role_from(Some("anchor"), false), Role::Anchor);
        assert_eq!(role_from(Some("station"), true), Role::Anchor);
        assert_eq!(role_from(Some("node"), true), Role::Node);
        assert_eq!(role_from(Some("direct"), false), Role::Direct);
        assert_eq!(role_from(None, true), Role::Direct, "protocol v1 over LTE");
        assert_eq!(role_from(Some("unknown"), false), Role::Unknown);
    }

    #[test]
    fn link_quality_by_rssi_and_snr() {
        assert_eq!(quality(Some(-90.0), Some(8.0)), Quality::Good);
        assert_eq!(quality(Some(-115.0), Some(2.0)), Quality::Fair);
        assert_eq!(quality(Some(-100.0), Some(-5.0)), Quality::Fair);
        assert_eq!(quality(Some(-100.0), Some(-9.0)), Quality::Poor);
        assert_eq!(quality(Some(-125.0), None), Quality::Poor);
        assert_eq!(quality(None, None), Quality::Unknown);
    }
}
