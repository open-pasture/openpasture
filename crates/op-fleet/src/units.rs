//! The units registry: one row per serial from the moment a station
//! registers it, its device keys, its history, and its lifecycle
//! `built → tested → provisioned → claimed → in_service → retired | rma`.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::time::{now, opt_from_db, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, DbEnum, Event, Identity, Role};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqliteConnection};

use crate::runs::TestRun;
use crate::{keys, stations, who};

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/fleet/units", get(list).post(register))
        .route("/api/fleet/units/{serial}", get(detail))
        .route("/api/fleet/units/{serial}/state", post(set_state))
        .route("/api/fleet/summary", get(summary))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UnitState {
    Built,
    Tested,
    Provisioned,
    Claimed,
    InService,
    Retired,
    Rma,
}

impl DbEnum for UnitState {}

impl UnitState {
    /// On a farm: tests and key changes wait until it is marked RMA.
    pub fn on_farm(self) -> bool {
        matches!(self, UnitState::Claimed | UnitState::InService)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    /// Registered by a station on this server.
    Station,
    /// Claimed here from another builder's card.
    Card,
}

impl DbEnum for Origin {}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Radios {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lora_eui: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub imei: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iccid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ble_addr: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LastTest {
    pub id: String,
    pub result: String,
    pub plan_id: String,
    pub plan_version: i64,
    pub finished_at: DateTime<Utc>,
    /// The server's order of runs: the latest is the highest.
    pub seq: i64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Unit {
    pub serial: String,
    pub device_class: String,
    pub hw_rev: String,
    pub fw_version: String,
    pub device_pubkey: String,
    pub device_kid: String,
    /// The protocol v2 device id: the same value as `device_kid`.
    pub device_id: String,
    pub radios: Radios,
    /// Kids of the firmware keys its bootloader trusts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub image_keys: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<String>,
    pub state: UnitState,
    /// Goes up when the unit is sent back (RMA), retired or gets a new key.
    /// Only test runs of this generation count.
    pub generation: i64,
    /// When this generation began (server time); absent for the first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub generation_at: Option<DateTime<Utc>>,
    pub origin: Origin,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub station: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claim_code: Option<String>,
    /// The claim code was checked: this server drew it, a station wrote it,
    /// or it matched the factory record's claim check. A card claimed with
    /// no record is taken at its word until the collar reports.
    pub claim_checked: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_test: Option<LastTest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maker: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub maker_kid: Option<String>,
    /// The factory record's signature checked out against a maker key this server knows.
    pub maker_verified: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub made_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provisioned_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Units with their herd (through the collar) and the run that decides
/// whether they can be provisioned: the latest by the server's order among
/// the counted runs of the unit's current generation.
const SELECT: &str = "SELECT u.*, c.herd_id AS c_herd_id,
    (SELECT json_object('id', r.id, 'result', r.result, 'plan_id', r.plan_id, 'plan_version', r.plan_version, 'finished_at', r.finished_at, 'seq', r.seq)
       FROM fleet_test_runs r WHERE r.id = (SELECT r.id FROM fleet_test_runs r WHERE r.serial = u.serial AND r.generation = u.generation AND r.counted = 1 ORDER BY r.seq DESC LIMIT 1)) AS last_test_json
    FROM fleet_units u LEFT JOIN collars c ON c.id = u.collar_id";

fn from_row(r: &SqliteRow) -> anyhow::Result<Unit> {
    let ts = |col: &str| -> anyhow::Result<DateTime<Utc>> { op_core::time::from_db(&r.try_get::<String, _>(col)?) };
    let last_test = match r.try_get::<Option<String>, _>("last_test_json")? {
        Some(s) => Some(serde_json::from_str::<LastTest>(&s)?),
        None => None,
    };
    Ok(Unit {
        serial: r.try_get("serial")?,
        device_class: r.try_get("device_class")?,
        hw_rev: r.try_get("hw_rev")?,
        fw_version: r.try_get("fw_version")?,
        device_pubkey: r.try_get("device_pubkey")?,
        device_id: r.try_get("device_kid")?,
        device_kid: r.try_get("device_kid")?,
        radios: serde_json::from_str(&r.try_get::<String, _>("radios")?).unwrap_or_default(),
        image_keys: serde_json::from_str(&r.try_get::<String, _>("image_keys")?).unwrap_or_default(),
        caps: serde_json::from_str(&r.try_get::<String, _>("caps")?).unwrap_or_default(),
        state: UnitState::from_db(&r.try_get::<String, _>("state")?)?,
        generation: r.try_get("generation")?,
        generation_at: opt_from_db(r.try_get("generation_at")?)?,
        origin: Origin::from_db(&r.try_get::<String, _>("origin")?)?,
        station: r.try_get("station")?,
        claim_code: r.try_get("claim_code")?,
        claim_checked: r.try_get::<i64, _>("claim_checked")? != 0,
        collar_id: r.try_get("collar_id")?,
        herd_id: r.try_get("c_herd_id")?,
        last_test,
        maker: r.try_get("maker")?,
        maker_kid: r.try_get("maker_kid")?,
        maker_verified: r.try_get::<i64, _>("maker_verified")? != 0,
        made_at: opt_from_db(r.try_get("made_at")?)?,
        provisioned_at: opt_from_db(r.try_get("provisioned_at")?)?,
        claimed_at: opt_from_db(r.try_get("claimed_at")?)?,
        created_at: ts("created_at")?,
        updated_at: ts("updated_at")?,
    })
}

pub async fn load(conn: &mut SqliteConnection, serial: &str) -> anyhow::Result<Option<Unit>> {
    let row = sqlx::query(&format!("{SELECT} WHERE u.serial = ?")).bind(serial).fetch_optional(&mut *conn).await?;
    row.map(|r| from_row(&r)).transpose()
}

pub async fn get_unit(ctx: &Ctx, serial: &str) -> anyhow::Result<Option<Unit>> {
    let mut conn = ctx.db().acquire().await?;
    load(&mut conn, serial).await
}

pub async fn find(ctx: &Ctx, serial: &str) -> ApiResult<Unit> {
    get_unit(ctx, serial).await?.ok_or_else(|| ApiError::not_found(format!("No unit with serial {serial}.")))
}

/// The unit linked to a collar, if it came from one.
pub async fn for_collar(ctx: &Ctx, collar_id: &str) -> anyhow::Result<Option<Unit>> {
    let row = sqlx::query(&format!("{SELECT} WHERE u.collar_id = ?")).bind(collar_id).fetch_optional(ctx.db()).await?;
    row.map(|r| from_row(&r)).transpose()
}

/// Add a line to a unit's history.
pub async fn log(conn: &mut SqliteConnection, serial: &str, kind: &str, detail: Value, by: Option<&str>) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO fleet_unit_events (serial, at, kind, detail, by) VALUES (?, ?, ?, ?, ?)")
        .bind(serial)
        .bind(to_db(&now()))
        .bind(kind)
        .bind(detail.to_string())
        .bind(by)
        .execute(&mut *conn)
        .await?;
    Ok(())
}

/// Tell `/api/live` (managers) the unit changed.
pub async fn publish(ctx: &Ctx, serial: &str) {
    match get_unit(ctx, serial).await {
        Ok(Some(unit)) => ctx.publish(Event::FleetUnit { unit: serde_json::to_value(&unit).unwrap_or(Value::Null) }),
        Ok(None) => {}
        Err(e) => tracing::warn!(serial, "fleet unit event: {e:#}"),
    }
}

// ---- checks ------------------------------------------------------------------------------

/// Protocol v2: 1–32 characters of `A–Z 0–9 -`.
pub(crate) fn check_serial(s: &str) -> ApiResult<()> {
    if !op_protocol::v2::identity::serial_ok(s) {
        return Err(ApiError::bad_request("A serial is 1–32 characters of A–Z, 0–9 and -."));
    }
    Ok(())
}

pub(crate) fn check_class(s: &str) -> ApiResult<()> {
    if s.is_empty() || s.len() > 32 || !s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_') {
        return Err(ApiError::bad_request("A device class is 1–32 lowercase letters, digits, - or _ (node, anchor, …)."));
    }
    Ok(())
}

fn check_text(what: &str, s: &str, max: usize) -> ApiResult<()> {
    if s.trim().is_empty() || s.len() > max || s.chars().any(char::is_control) {
        return Err(ApiError::bad_request(format!("{what} must be 1–{max} characters.")));
    }
    Ok(())
}

fn check_kids(kids: &[String]) -> ApiResult<()> {
    if kids.len() > 16 || kids.iter().any(|k| keys::parse_kid(k).is_none()) {
        return Err(ApiError::bad_request("image_keys lists up to 16 key ids of 16 hex digits."));
    }
    Ok(())
}

fn check_caps(caps: &[String]) -> ApiResult<()> {
    if caps.len() > 64 || caps.iter().any(|c| c.is_empty() || c.len() > 32) {
        return Err(ApiError::bad_request("caps lists up to 64 capabilities of 1–32 characters."));
    }
    Ok(())
}

/// A claim code from a fixture, normalized (protocol v2: 16 Crockford base32 characters).
pub(crate) fn claim_code(code: &str) -> ApiResult<String> {
    op_protocol::v2::identity::normalize_claim_code(code).ok_or_else(|| ApiError::bad_request("A claim code is 16 characters of Crockford base32."))
}

fn check_radios(r: &Radios) -> ApiResult<()> {
    for (name, v) in [("lora_eui", &r.lora_eui), ("imei", &r.imei), ("iccid", &r.iccid), ("ble_addr", &r.ble_addr)] {
        if let Some(v) = v {
            check_text(name, v, 64)?;
        }
    }
    Ok(())
}

// ---- register ------------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewUnit {
    serial: String,
    device_class: String,
    hw_rev: String,
    fw_version: String,
    device_pubkey: String,
    #[serde(default)]
    radios: Radios,
    station: Option<String>,
    #[serde(default)]
    image_keys: Vec<String>,
    #[serde(default)]
    caps: Vec<String>,
    /// The claim code the fixture drew and wrote to the unit.
    claim_code: Option<String>,
    /// The fixture's maker-signed record (hex COSE_Sign1).
    factory_record: Option<String>,
    #[serde(default)]
    force: bool,
}

/// What a station's factory record says, checked against the rest of the body.
struct FromRecord {
    cose: String,
    maker: String,
    maker_kid: String,
    verified: bool,
    made_at: String,
}

async fn check_record(ctx: &Ctx, b: &NewUnit, serial: &str, key: &op_protocol::VerifyingKey, code: Option<&str>) -> ApiResult<Option<FromRecord>> {
    let Some(hex) = &b.factory_record else { return Ok(None) };
    let read = crate::record::read(ctx, hex).await.map_err(ApiError::bad_request)?;
    let r = &read.record;
    let mismatch = |what: &str| ApiError::bad_request(format!("The factory record's {what} doesn't match the unit."));
    if r.serial != serial {
        return Err(mismatch("serial"));
    }
    if r.device_key != key.to_bytes() {
        return Err(mismatch("device key"));
    }
    if r.class.is_known() && r.class.name() != b.device_class {
        return Err(mismatch("class"));
    }
    if r.hw_rev != b.hw_rev.trim() {
        return Err(mismatch("hardware revision"));
    }
    if let (Some(check), Some(code)) = (r.claim_check, code) {
        let id = op_protocol::v2::crypto::kid(key.as_bytes());
        if op_protocol::v2::identity::claim_check(&op_protocol::v2::identity::claim_key(code, &id)) != check {
            return Err(ApiError::bad_request("The claim code doesn't match the factory record's claim check."));
        }
    }
    let made_at = chrono::DateTime::from_timestamp(r.made_at as i64, 0).unwrap_or_else(now);
    Ok(Some(FromRecord {
        cose: read.view.cose,
        maker: r.maker.clone(),
        maker_kid: read.view.maker_kid,
        verified: read.view.verified,
        made_at: to_db(&made_at),
    }))
}

/// `POST /api/fleet/units`: idempotent on serial. Another key for a known
/// serial needs the owner and `force`.
async fn register(State(ctx): State<Ctx>, identity: Identity, ApiJson(b): ApiJson<NewUnit>) -> ApiResult<(StatusCode, Json<Unit>)> {
    let serial = b.serial.trim().to_owned();
    check_serial(&serial)?;
    check_class(&b.device_class)?;
    check_text("hw_rev", &b.hw_rev, 16)?;
    check_text("fw_version", &b.fw_version, 32)?;
    check_radios(&b.radios)?;
    check_kids(&b.image_keys)?;
    check_caps(&b.caps)?;
    let key = keys::parse_public_key(&b.device_pubkey, "device_pubkey").map_err(ApiError::bad_request)?;
    let pubkey = op_protocol::encode_public_key(&key);
    let kid = keys::kid(&key);
    let code = b.claim_code.as_deref().map(claim_code).transpose()?;
    let record = check_record(&ctx, &b, &serial, &key, code.as_deref()).await?;
    let station = match b.station.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            check_text("station", s, 64)?;
            Some(s.to_owned())
        }
        None => stations::station_name(&identity),
    };
    let by = who(&identity);
    let at = to_db(&now());
    let radios = serde_json::to_string(&b.radios).map_err(anyhow::Error::from)?;
    let image_keys = serde_json::to_string(&b.image_keys.iter().map(|k| k.trim().to_ascii_lowercase()).collect::<Vec<_>>()).map_err(anyhow::Error::from)?;
    let caps = serde_json::to_string(&b.caps).map_err(anyhow::Error::from)?;

    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let other: Option<String> =
        sqlx::query_scalar("SELECT serial FROM fleet_units WHERE device_kid = ? AND serial != ?").bind(&kid).bind(&serial).fetch_optional(&mut *tx).await?;
    if let Some(other) = other {
        return Err(ApiError::conflict(format!("This device key is already registered to {other}. Two units can't share a key.")));
    }
    if let Some(c) = &code {
        let taken: Option<String> =
            sqlx::query_scalar("SELECT serial FROM fleet_units WHERE claim_code = ? AND serial != ?").bind(c).bind(&serial).fetch_optional(&mut *tx).await?;
        if taken.is_some() {
            return Err(ApiError::conflict("Another unit already has this claim code. Draw a new one."));
        }
    }
    let existing = load(&mut tx, &serial).await?;
    let status = match existing {
        None => {
            sqlx::query(
                "INSERT INTO fleet_units (serial, device_class, hw_rev, fw_version, device_pubkey, device_kid, radios, image_keys, caps, state, origin, station, claim_code, claim_checked, created_at, updated_at)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'built', 'station', ?, ?, 1, ?, ?)",
            )
            .bind(&serial)
            .bind(&b.device_class)
            .bind(b.hw_rev.trim())
            .bind(b.fw_version.trim())
            .bind(&pubkey)
            .bind(&kid)
            .bind(&radios)
            .bind(&image_keys)
            .bind(&caps)
            .bind(&station)
            .bind(&code)
            .bind(&at)
            .bind(&at)
            .execute(&mut *tx)
            .await?;
            sqlx::query("INSERT INTO fleet_unit_keys (serial, device_pubkey, device_kid, from_at) VALUES (?, ?, ?, ?)")
                .bind(&serial)
                .bind(&pubkey)
                .bind(&kid)
                .bind(&at)
                .execute(&mut *tx)
                .await?;
            log(
                &mut tx,
                &serial,
                "registered",
                json!({ "device_class": b.device_class, "hw_rev": b.hw_rev, "fw_version": b.fw_version, "device_id": kid, "station": station }),
                by.as_deref(),
            )
            .await?;
            StatusCode::CREATED
        }
        Some(u) => {
            if u.device_class != b.device_class {
                return Err(ApiError::conflict(format!("{serial} is registered as a {}, not a {}.", u.device_class, b.device_class)));
            }
            if u.device_kid != kid {
                if !b.force {
                    return Err(ApiError::conflict(format!(
                        "{serial} is registered with another device key (device id {}). The owner can replace it with force.",
                        u.device_kid
                    )));
                }
                if identity.role != Role::Owner || stations::is_station(&identity) {
                    return Err(ApiError::forbidden("Only the owner can replace a unit's device key."));
                }
                if u.state.on_farm() {
                    return Err(ApiError::conflict(format!("{serial} is on a farm. Mark it RMA before replacing its key.")));
                }
                sqlx::query("UPDATE fleet_unit_keys SET to_at = ? WHERE serial = ? AND to_at IS NULL").bind(&at).bind(&serial).execute(&mut *tx).await?;
                sqlx::query("INSERT OR REPLACE INTO fleet_unit_keys (serial, device_pubkey, device_kid, from_at) VALUES (?, ?, ?, ?)")
                    .bind(&serial)
                    .bind(&pubkey)
                    .bind(&kid)
                    .bind(&at)
                    .execute(&mut *tx)
                    .await?;
                // A new key needs a new factory record and a fresh test: back to
                // the fixture, in a new generation so no earlier pass counts.
                let n = sqlx::query(
                    "UPDATE fleet_units SET device_pubkey = ?, device_kid = ?, factory_record = NULL, maker = NULL, maker_kid = NULL, maker_verified = 0, state = 'built',
                       generation = generation + 1, generation_at = ?, updated_at = ? WHERE serial = ? AND generation = ?",
                )
                .bind(&pubkey)
                .bind(&kid)
                .bind(&at)
                .bind(&at)
                .bind(&serial)
                .bind(u.generation)
                .execute(&mut *tx)
                .await?
                .rows_affected();
                if n != 1 {
                    return Err(ApiError::conflict(format!("{serial} changed meanwhile. Try again.")));
                }
                log(&mut tx, &serial, "key_replaced", json!({ "from": u.device_kid, "to": kid }), by.as_deref()).await?;
            }
            let changed = u.hw_rev != b.hw_rev.trim()
                || u.fw_version != b.fw_version.trim()
                || u.radios != b.radios
                || (station.is_some() && u.station != station)
                || (!b.image_keys.is_empty() && u.image_keys != b.image_keys)
                || (!b.caps.is_empty() && u.caps != b.caps)
                || (code.is_some() && u.claim_code != code);
            if changed {
                sqlx::query(
                    "UPDATE fleet_units SET hw_rev = ?, fw_version = ?, radios = ?, station = COALESCE(?, station),
                       image_keys = CASE WHEN ? = '[]' THEN image_keys ELSE ? END, caps = CASE WHEN ? = '[]' THEN caps ELSE ? END,
                       claim_code = COALESCE(?, claim_code), updated_at = ? WHERE serial = ?",
                )
                .bind(b.hw_rev.trim())
                .bind(b.fw_version.trim())
                .bind(&radios)
                .bind(&station)
                .bind(&image_keys)
                .bind(&image_keys)
                .bind(&caps)
                .bind(&caps)
                .bind(&code)
                .bind(&at)
                .bind(&serial)
                .execute(&mut *tx)
                .await?;
                log(
                    &mut tx,
                    &serial,
                    "registered",
                    json!({ "again": true, "hw_rev": b.hw_rev, "fw_version": b.fw_version, "station": station }),
                    by.as_deref(),
                )
                .await?;
            }
            StatusCode::OK
        }
    };
    if let Some(r) = &record {
        sqlx::query("UPDATE fleet_units SET factory_record = ?, maker = ?, maker_kid = ?, maker_verified = ?, made_at = ? WHERE serial = ?")
            .bind(&r.cose)
            .bind(&r.maker)
            .bind(&r.maker_kid)
            .bind(r.verified as i64)
            .bind(&r.made_at)
            .bind(&serial)
            .execute(&mut *tx)
            .await?;
    }
    let unit = load(&mut tx, &serial).await?.ok_or_else(|| ApiError::internal("unit vanished"))?;
    tx.commit().await?;
    if status == StatusCode::CREATED {
        tracing::info!(serial = %serial, class = %unit.device_class, "unit registered");
    }
    ctx.publish(Event::FleetUnit { unit: serde_json::to_value(&unit).unwrap_or(Value::Null) });
    Ok((status, Json(unit)))
}

// ---- read -----------------------------------------------------------------------------------

#[derive(Deserialize)]
struct ListQuery {
    state: Option<String>,
    device_class: Option<String>,
    station: Option<String>,
    q: Option<String>,
    limit: Option<i64>,
    /// `next` from the previous page.
    cursor: Option<String>,
}

/// Units per page by default, and at most.
pub const PAGE: i64 = 200;
pub const MAX_PAGE: i64 = 1000;

/// One page of units, newest first, with how many match in all.
#[derive(Debug, Serialize)]
pub struct UnitPage {
    pub units: Vec<Unit>,
    /// Every unit matching the filters, not just this page.
    pub total: i64,
    /// Pass as `cursor` for the next page; absent on the last.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// A page boundary: the last unit's `created_at` and serial.
fn cursor_of(u: &Unit) -> String {
    format!("{}|{}", to_db(&u.created_at), u.serial)
}

/// `GET /api/fleet/units`: newest first, a page at a time (keyset, so units
/// registered meanwhile don't shift or repeat what the next page shows).
async fn list(State(ctx): State<Ctx>, Query(q): Query<ListQuery>) -> ApiResult<Json<UnitPage>> {
    let mut filter = String::from(" WHERE 1 = 1");
    let mut binds: Vec<String> = Vec::new();
    if let Some(s) = q.state.as_deref().filter(|s| !s.is_empty()) {
        UnitState::from_db(s).map_err(|_| ApiError::bad_request(format!("No unit state {s:?}.")))?;
        filter.push_str(" AND u.state = ?");
        binds.push(s.to_owned());
    }
    if let Some(c) = q.device_class.as_deref().filter(|s| !s.is_empty()) {
        filter.push_str(" AND u.device_class = ?");
        binds.push(c.to_owned());
    }
    if let Some(s) = q.station.as_deref().filter(|s| !s.is_empty()) {
        filter.push_str(" AND u.station = ?");
        binds.push(s.to_owned());
    }
    if let Some(text) = q.q.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        filter.push_str(" AND (u.serial LIKE ? ESCAPE '\\' OR u.device_kid LIKE ? ESCAPE '\\' OR u.radios LIKE ? ESCAPE '\\' OR u.collar_id = ?)");
        let like = format!("%{}%", text.replace('\\', "\\\\").replace('%', "\\%").replace('_', "\\_"));
        binds.extend([like.clone(), like.clone(), like, text.to_owned()]);
    }
    let count_sql = format!("SELECT COUNT(*) FROM fleet_units u{filter}");
    let mut count = sqlx::query_scalar::<_, i64>(&count_sql);
    for b in &binds {
        count = count.bind(b);
    }
    let total = count.fetch_one(ctx.db()).await?;
    let mut page = filter.clone();
    if let Some(c) = q.cursor.as_deref().filter(|s| !s.is_empty()) {
        let (at, serial) = c.split_once('|').ok_or_else(|| ApiError::bad_request("That cursor isn't one this server gave."))?;
        page.push_str(" AND (u.created_at < ? OR (u.created_at = ? AND u.serial < ?))");
        binds.extend([at.to_owned(), at.to_owned(), serial.to_owned()]);
    }
    let limit = q.limit.unwrap_or(PAGE).clamp(1, MAX_PAGE);
    let sql = format!("{SELECT}{page} ORDER BY u.created_at DESC, u.serial DESC LIMIT {}", limit + 1);
    let mut query = sqlx::query(&sql);
    for b in &binds {
        query = query.bind(b);
    }
    let rows = query.fetch_all(ctx.db()).await?;
    let mut units: Vec<Unit> = rows.iter().map(from_row).collect::<anyhow::Result<_>>()?;
    let next = (units.len() as i64 > limit).then(|| {
        units.truncate(limit as usize);
        units.last().map(cursor_of)
    });
    Ok(Json(UnitPage { units, total, next: next.flatten() }))
}

#[derive(Debug, Serialize)]
pub struct UnitEvent {
    pub at: DateTime<Utc>,
    pub kind: String,
    pub detail: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct KeyHistory {
    pub device_pubkey: String,
    pub device_kid: String,
    pub from: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
}

#[derive(Debug, Serialize)]
pub struct UnitDetail {
    pub unit: Unit,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub factory_record: Option<crate::record::RecordView>,
    pub runs: Vec<TestRun>,
    pub events: Vec<UnitEvent>,
    pub keys: Vec<KeyHistory>,
}

/// `GET /api/fleet/units/:serial`: the unit, every test run (newest first),
/// its history and its keys. What a returned collar is looked up by.
async fn detail(State(ctx): State<Ctx>, Path(serial): Path<String>) -> ApiResult<Json<UnitDetail>> {
    let unit = find(&ctx, &serial).await?;
    let runs = crate::runs::for_unit(&ctx, &serial).await?;
    let events = sqlx::query("SELECT at, kind, detail, by FROM fleet_unit_events WHERE serial = ? ORDER BY id")
        .bind(&serial)
        .fetch_all(ctx.db())
        .await?
        .iter()
        .map(|r| -> anyhow::Result<UnitEvent> {
            Ok(UnitEvent {
                at: op_core::time::from_db(&r.try_get::<String, _>("at")?)?,
                kind: r.try_get("kind")?,
                detail: serde_json::from_str(&r.try_get::<String, _>("detail")?).unwrap_or(Value::Null),
                by: r.try_get("by")?,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    let keys = sqlx::query("SELECT device_pubkey, device_kid, from_at, to_at FROM fleet_unit_keys WHERE serial = ? ORDER BY from_at")
        .bind(&serial)
        .fetch_all(ctx.db())
        .await?
        .iter()
        .map(|r| -> anyhow::Result<KeyHistory> {
            Ok(KeyHistory {
                device_pubkey: r.try_get("device_pubkey")?,
                device_kid: r.try_get("device_kid")?,
                from: op_core::time::from_db(&r.try_get::<String, _>("from_at")?)?,
                to: opt_from_db(r.try_get("to_at")?)?,
            })
        })
        .collect::<anyhow::Result<_>>()?;
    let stored: Option<String> = sqlx::query_scalar("SELECT factory_record FROM fleet_units WHERE serial = ?").bind(&serial).fetch_one(ctx.db()).await?;
    let factory_record = match stored {
        Some(h) => crate::record::view(&ctx, &h).await,
        None => None,
    };
    Ok(Json(UnitDetail { unit, factory_record, runs, events, keys }))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StateBody {
    state: UnitState,
    note: Option<String>,
}

/// `POST /api/fleet/units/:serial/state`: retire a unit or send it back
/// (RMA). It keeps its collar, so a repaired unit returns to the same one.
/// Either starts a new generation: no earlier test run counts, so a unit
/// back from RMA needs a fresh pass before it is provisioned again. A
/// retired unit can only come back through RMA.
async fn set_state(State(ctx): State<Ctx>, identity: Identity, Path(serial): Path<String>, ApiJson(b): ApiJson<StateBody>) -> ApiResult<Json<Unit>> {
    if !matches!(b.state, UnitState::Retired | UnitState::Rma) {
        return Err(ApiError::bad_request("A unit can be set to retired or rma; the rest follow from tests, provisioning and claims."));
    }
    if let Some(n) = &b.note {
        check_text("note", n, 500)?;
    }
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let unit = load(&mut tx, &serial).await?.ok_or_else(|| ApiError::not_found(format!("No unit with serial {serial}.")))?;
    if unit.state == b.state {
        return Ok(Json(unit));
    }
    let at = to_db(&now());
    let n = sqlx::query(
        "UPDATE fleet_units SET state = ?, generation = generation + 1, generation_at = ?, updated_at = ? WHERE serial = ? AND state = ? AND generation = ?",
    )
    .bind(b.state.as_db())
    .bind(&at)
    .bind(&at)
    .bind(&serial)
    .bind(unit.state.as_db())
    .bind(unit.generation)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(ApiError::conflict(format!("{serial} changed meanwhile. Look at it again.")));
    }
    log(&mut tx, &serial, "state", json!({ "from": unit.state, "to": b.state, "note": b.note, "generation": unit.generation + 1 }), who(&identity).as_deref())
        .await?;
    let unit = load(&mut tx, &serial).await?.ok_or_else(|| ApiError::internal("unit vanished"))?;
    tx.commit().await?;
    publish(&ctx, &serial).await;
    Ok(Json(unit))
}

#[derive(Serialize)]
struct Summary {
    total: i64,
    by_state: serde_json::Map<String, Value>,
    by_class: serde_json::Map<String, Value>,
}

async fn summary(State(ctx): State<Ctx>) -> ApiResult<Json<Summary>> {
    let mut by_state = serde_json::Map::new();
    let mut by_class = serde_json::Map::new();
    let mut total = 0;
    for (state, n) in sqlx::query_as::<_, (String, i64)>("SELECT state, COUNT(*) FROM fleet_units GROUP BY state").fetch_all(ctx.db()).await? {
        total += n;
        by_state.insert(state, n.into());
    }
    for (class, n) in sqlx::query_as::<_, (String, i64)>("SELECT device_class, COUNT(*) FROM fleet_units GROUP BY device_class").fetch_all(ctx.db()).await? {
        by_class.insert(class, n.into());
    }
    Ok(Json(Summary { total, by_state, by_class }))
}

// ---- in service --------------------------------------------------------------------------------

/// A claimed unit is in service from its collar's first report. Its claim
/// code is forgotten then: claim codes are kept only as long as a claim
/// takes (protocol v2 `server.md` §10); the factory record's claim check stays.
pub async fn mark_in_service(ctx: &Ctx, collar_id: &str) -> anyhow::Result<bool> {
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let serial: Option<String> = sqlx::query_scalar(&format!(
        "SELECT u.serial FROM fleet_units u JOIN collars c ON c.id = u.collar_id WHERE u.collar_id = ? AND u.state = 'claimed' AND {REPORTED_SINCE_CLAIM}"
    ))
    .bind(collar_id)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(serial) = serial else { return Ok(false) };
    let n = sqlx::query("UPDATE fleet_units SET state = 'in_service', claim_code = NULL, updated_at = ? WHERE serial = ? AND state = 'claimed'")
        .bind(to_db(&now()))
        .bind(&serial)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n == 1 {
        log(&mut tx, &serial, "in_service", json!({ "collar_id": collar_id }), None).await?;
    }
    tx.commit().await?;
    if n == 1 {
        tracing::info!(serial = %serial, collar = %collar_id, "unit in service");
        publish(ctx, &serial).await;
    }
    Ok(n == 1)
}

/// The collar reported after the unit was (re)claimed: a reclaimed unit
/// waits for a report under its new key.
const REPORTED_SINCE_CLAIM: &str = "c.last_seen IS NOT NULL AND (u.claimed_at IS NULL OR c.last_seen >= u.claimed_at)";

/// Claimed units whose collar has reported, in one sweep (the live feed can
/// drop events; the rollout tick calls this every minute).
pub async fn sweep_in_service(ctx: &Ctx) -> anyhow::Result<()> {
    let ids: Vec<String> = sqlx::query_scalar(&format!(
        "SELECT u.collar_id FROM fleet_units u JOIN collars c ON c.id = u.collar_id WHERE u.state = 'claimed' AND {REPORTED_SINCE_CLAIM}"
    ))
    .fetch_all(ctx.db())
    .await?;
    for id in ids {
        mark_in_service(ctx, &id).await?;
    }
    Ok(())
}

pub fn spawn_in_service_watch(ctx: &Ctx) {
    let ctx = ctx.clone();
    let mut rx = ctx.subscribe();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                ev = rx.recv() => match ev {
                    Ok(Event::Collar { collar }) if collar.last_seen.is_some() => {
                        if let Err(e) = mark_in_service(&ctx, &collar.id).await {
                            tracing::warn!(collar = %collar.id, "fleet in-service: {e:#}");
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
            }
        }
    });
}
