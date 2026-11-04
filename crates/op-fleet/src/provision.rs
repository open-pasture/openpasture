//! Provisioning and claim (protocol v2 `identity.md` §3–4).
//!
//! A unit that passed its test gets a claim code (the fixture's, or one
//! drawn here), a factory record signed by this server's maker key (unless
//! the fixture sent its own), its card, and this server's key, kid, endpoint
//! and link parameters for bench and DIY use.
//!
//! A farm claims a unit with its claim code: the unit becomes a collar in a
//! herd (`/api/collars`). A unit this server never saw can be claimed from
//! its card (`OC2:<serial>:<device key>:<claim code>`). Getting the server
//! key onto a v2 collar is the bind command, which needs the collar's claim
//! nonce from its hello; the collar ingest sends it.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use op_core::time::{now, to_db};
use op_core::{ActivityEvent, ApiError, ApiJson, ApiResult, Collar, Ctx, Event, Identity, Role, id, undo_window};
use op_protocol::v2::crypto::ct_eq;
use op_protocol::v2::identity::{Card, normalize_claim_code};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::claim_limit;
use crate::record::RecordView;
use crate::units::{self, Origin, Unit, UnitState};
use crate::{keys, who};

/// Wrong claim codes allowed per caller per minute (also per serial / target).
pub const CLAIM_TRIES_PER_MINUTE: usize = claim_limit::TRIES_PER_MINUTE;

/// Viewers may not claim; hands and up may. Matches op-server policy.
const CLAIM_VIEWER_MSG: &str = "Viewers can't add collars. Ask a farm hand, manager or owner.";
const CLAIM_VIEWER_CODE: &str = "claim_requires_hand";

fn require_claim_role(identity: &Identity) -> ApiResult<()> {
    if identity.is_anonymous() {
        return Err(ApiError::unauthorized("Sign in first."));
    }
    if !identity.can(Role::Hand) {
        return Err(ApiError::forbidden(CLAIM_VIEWER_MSG).with_details(json!({ "code": CLAIM_VIEWER_CODE })));
    }
    Ok(())
}

/// Constant-time compare of a stored claim code to a normalized attempt.
fn codes_equal(stored: Option<&str>, given: &str) -> bool {
    let dummy = "????????????????";
    let a = stored.unwrap_or(dummy);
    ct_eq(a.as_bytes(), given.as_bytes()) && stored.is_some()
}

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/fleet/units/{serial}/provision", post(provision))
        .route("/api/fleet/units/{serial}/claim", post(claim_serial))
        .route("/api/fleet/claim", post(claim_any))
        .route("/api/fleet/provisioning", get(get_settings).put(put_settings))
}

// ---- settings ------------------------------------------------------------------------------

const SETTINGS_KEY: &str = "fleet.provisioning";

/// LoRa regional parameter sets a unit can be told to use.
pub const LORA_REGIONS: [&str; 7] = ["US915", "AU915", "EU868", "AS923", "IN865", "KR920", "CN470"];

/// What provisioning writes besides keys: the LoRa region for the farm's
/// country (US915 by default: FCC 902–928 MHz), and the maker named in the
/// factory records this server signs (the farm's name when unset).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProvisioningSettings {
    pub lora_region: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub maker: Option<String>,
}

impl Default for ProvisioningSettings {
    fn default() -> Self {
        Self { lora_region: "US915".into(), maker: None }
    }
}

pub async fn settings(ctx: &Ctx) -> anyhow::Result<ProvisioningSettings> {
    Ok(ctx.store().get_setting::<ProvisioningSettings>(SETTINGS_KEY).await?.unwrap_or_default())
}

#[derive(Serialize)]
struct SettingsView {
    #[serde(flatten)]
    settings: ProvisioningSettings,
    /// This server's maker key, which signs its factory records: its kid,
    /// and its public half (base64) for the farms that enrol it.
    maker_kid: String,
    maker_key: String,
}

fn view(ctx: &Ctx, settings: ProvisioningSettings) -> anyhow::Result<SettingsView> {
    let key = keys::maker_key(ctx.data_dir())?.verifying_key();
    Ok(SettingsView { settings, maker_kid: keys::kid(&key), maker_key: op_protocol::encode_public_key(&key) })
}

async fn get_settings(State(ctx): State<Ctx>) -> ApiResult<Json<SettingsView>> {
    Ok(Json(view(&ctx, settings(&ctx).await?)?))
}

async fn put_settings(State(ctx): State<Ctx>, ApiJson(b): ApiJson<ProvisioningSettings>) -> ApiResult<Json<SettingsView>> {
    if !LORA_REGIONS.contains(&b.lora_region.as_str()) {
        return Err(ApiError::bad_request(format!("LoRa region is one of {}.", LORA_REGIONS.join(", "))));
    }
    if b.maker.as_deref().is_some_and(|m| m.trim().is_empty() || m.len() > 64) {
        return Err(ApiError::bad_request("The maker's name is 1–64 characters."));
    }
    ctx.store().set_setting(SETTINGS_KEY, &b).await?;
    Ok(Json(view(&ctx, b)?))
}

/// Who signs this server's factory records.
async fn maker_name(ctx: &Ctx, s: &ProvisioningSettings) -> anyhow::Result<String> {
    if let Some(m) = &s.maker {
        return Ok(m.clone());
    }
    Ok(ctx.store().get_farm().await?.map(|f| f.name).unwrap_or_else(|| "openpasture".into()))
}

/// A claim code in its printed form: four groups of four.
pub fn grouped(code: &str) -> String {
    code.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("-")
}

// ---- provision -----------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Link {
    pub transport: String,
    pub lora_region: String,
    pub report_s: u32,
    pub poll_s: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Provisioning {
    pub serial: String,
    pub device_id: String,
    pub server_key: String,
    pub server_kid: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    pub link: Link,
    /// 16 Crockford base32 characters; printed in groups of four.
    pub claim_code: String,
    /// The card's QR text.
    pub card: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub factory_record: Option<RecordView>,
}

/// How a unit reaches the server: nodes over LoRa through an anchor, anchors
/// and stations over both, direct collars over LTE; another class by the
/// radios it was registered with.
fn transport(unit: &Unit) -> &'static str {
    match unit.device_class.as_str() {
        "node" => "lora",
        "anchor" | "station" => "lora+lte",
        "direct" => "lte",
        _ => match (unit.radios.lora_eui.is_some(), unit.radios.imei.is_some() || unit.radios.iccid.is_some()) {
            (true, true) => "lora+lte",
            (false, true) => "lte",
            _ => "lora",
        },
    }
}

/// Where collars reach this server (`server.collar_url` + `/collar/v1`),
/// when a usable collar address is set.
pub async fn collar_endpoint(ctx: &Ctx) -> anyhow::Result<Option<String>> {
    let url = ctx.settings().await?.server.collar_url;
    Ok(url
        .filter(|u| op_core::settings::collar_url_problem(u).is_none())
        .map(|u| format!("{}/collar/v1", u.trim_end_matches('/')))
        .filter(|e| e.len() <= op_protocol::config::MAX_ENDPOINT_BYTES))
}

/// Why there is no endpoint.
pub const NO_COLLAR_ADDRESS: &str =
    "Collars need an https address that reaches this server, and this one has none yet. Set the collar address in Settings, Server.";

/// The collars' base cadence (op-ingest's `collars.config`), 60 s each by default.
async fn cadence(ctx: &Ctx) -> (u32, u32) {
    let v = ctx.store().get_setting_json("collars.config").await.ok().flatten().unwrap_or(Value::Null);
    let n = |k: &str| v.get(k).and_then(Value::as_u64).map(|x| x as u32).unwrap_or(60);
    (n("report_s"), n("poll_s"))
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct ProvisionBody {
    /// The code the fixture drew and wrote to the unit.
    claim_code: Option<String>,
    /// Draw a new code (the old label stops working).
    #[serde(default)]
    rotate_claim: bool,
}

/// `POST /api/fleet/units/:serial/provision`. A unit that already belongs
/// to a collar (back from RMA) returns to `claimed`; any other becomes
/// `provisioned`. Eligibility (its latest counted run of this generation
/// passed, and its state) is read in the same write that changes it, so a
/// retire or a failed run that lands meanwhile wins.
async fn provision(State(ctx): State<Ctx>, identity: Identity, Path(serial): Path<String>, body: axum::body::Bytes) -> ApiResult<Json<Provisioning>> {
    // The body is optional: a station may post nothing.
    let b: ProvisionBody = if body.iter().all(u8::is_ascii_whitespace) {
        ProvisionBody::default()
    } else {
        serde_json::from_slice(&body).map_err(|e| ApiError::new(StatusCode::UNPROCESSABLE_ENTITY, format!("Bad provisioning body: {e}")))?
    };
    let given = b.claim_code.as_deref().map(units::claim_code).transpose()?;
    let endpoint = collar_endpoint(&ctx).await?;
    let s = settings(&ctx).await?;
    let (report_s, poll_s) = cadence(&ctx).await;
    let maker = maker_name(&ctx, &s).await?;
    // Made and on disk before anything is signed with it.
    let own_maker = keys::kid(&keys::maker_key(ctx.data_dir())?.verifying_key());

    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let unit = units::load(&mut tx, &serial).await?.ok_or_else(|| ApiError::not_found(format!("No unit with serial {serial}.")))?;
    let tested = unit.last_test.as_ref().is_some_and(|t| t.result == "pass");
    let from_card = unit.origin == Origin::Card && unit.state.on_farm();
    match unit.state {
        UnitState::Retired => return Err(ApiError::conflict(format!("{serial} is retired. Send it back (RMA) and test it to use it again."))),
        UnitState::Rma => return Err(ApiError::conflict(format!("{serial} is back for repair. It needs a fresh passing test run first."))),
        UnitState::Built => {
            return Err(ApiError::conflict(match &unit.last_test {
                None => format!("{serial} hasn't been tested yet. Provisioning needs a passing test run."),
                Some(_) => format!("{serial} failed its latest test run. Provisioning needs a passing one."),
            }));
        }
        _ => {}
    }
    if !(tested || from_card) {
        return Err(ApiError::conflict(match &unit.last_test {
            None => format!("{serial} hasn't been tested since it came back. Provisioning needs a passing test run."),
            Some(_) => format!("{serial} failed its latest test run. Provisioning needs a passing one."),
        }));
    }
    let t = transport(&unit);
    if t != "lora" && endpoint.is_none() {
        return Err(ApiError::conflict(format!("{serial} talks to the server over LTE. {NO_COLLAR_ADDRESS}")));
    }
    let link = Link { transport: t.into(), lora_region: s.lora_region.clone(), report_s, poll_s };
    let run = match &unit.last_test {
        Some(t) => crate::runs::load(&mut tx, &t.id).await?,
        None => None,
    };

    let code = match (given, &unit.claim_code, b.rotate_claim) {
        (Some(c), _, _) => c,
        (None, Some(c), false) => c.clone(),
        _ => loop {
            let c = op_protocol::v2::identity::new_claim_code();
            let taken: Option<String> = sqlx::query_scalar("SELECT serial FROM fleet_units WHERE claim_code = ?").bind(&c).fetch_optional(&mut *tx).await?;
            if taken.is_none() {
                break c;
            }
        },
    };
    let taken: Option<String> =
        sqlx::query_scalar("SELECT serial FROM fleet_units WHERE claim_code = ? AND serial != ?").bind(&code).bind(&serial).fetch_optional(&mut *tx).await?;
    if taken.is_some() {
        return Err(ApiError::conflict("Another unit already has this claim code. Draw a new one."));
    }
    let with_code = Unit { claim_code: Some(code.clone()), ..unit.clone() };
    // The fixture's own record stands while the claim code is the one it checked.
    let stored: Option<String> = sqlx::query_scalar("SELECT factory_record FROM fleet_units WHERE serial = ?").bind(&serial).fetch_one(&mut *tx).await?;
    let keep = matches!((&stored, &unit.claim_code), (Some(_), Some(old)) if *old == code && unit.maker_kid.as_deref() != Some(own_maker.as_str()));
    let record = if keep {
        match &stored {
            Some(h) => crate::record::view(&ctx, h).await,
            None => None,
        }
    } else {
        crate::record::build(&ctx, &with_code, run.as_ref(), &maker, &s.lora_region)?.map(|(_, v)| v)
    };
    let next = if unit.state.on_farm() {
        unit.state
    } else if unit.collar_id.is_some() {
        UnitState::Claimed
    } else {
        UnitState::Provisioned
    };
    let at = to_db(&now());
    let n = sqlx::query("UPDATE fleet_units SET claim_code = ?, claim_checked = 1, state = ?, provisioned_at = ?, updated_at = ? WHERE serial = ? AND state = ? AND generation = ?")
        .bind(&code)
        .bind(op_core::DbEnum::as_db(&next))
        .bind(&at)
        .bind(&at)
        .bind(&serial)
        .bind(op_core::DbEnum::as_db(&unit.state))
        .bind(unit.generation)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n != 1 {
        return Err(ApiError::conflict(format!("{serial} changed meanwhile. Look at it again.")));
    }
    if let (Some(r), false) = (&record, keep) {
        sqlx::query(
            "UPDATE fleet_units SET factory_record = ?, maker = ?, maker_kid = ?, maker_verified = 1, made_at = COALESCE(made_at, created_at) WHERE serial = ?",
        )
        .bind(&r.cose)
        .bind(&maker)
        .bind(&r.maker_kid)
        .bind(&serial)
        .execute(&mut *tx)
        .await?;
    }
    let by = who(&identity);
    let rotated = unit.claim_code.as_deref().is_some_and(|old| old != code);
    units::log(&mut tx, &serial, "provisioned", json!({ "server_kid": keys::kid(&ctx.public_key()), "transport": t, "rotated_claim": rotated }), by.as_deref())
        .await?;
    tx.commit().await?;
    tracing::info!(serial = %serial, "unit provisioned");
    units::publish(&ctx, &serial).await;
    Ok(Json(Provisioning {
        serial: serial.clone(),
        device_id: unit.device_kid.clone(),
        server_key: ctx.public_key_b64(),
        server_kid: keys::kid(&ctx.public_key()),
        endpoint,
        link,
        card: crate::record::card(&with_code, &code).unwrap_or_default(),
        claim_code: code,
        factory_record: record,
    }))
}

// ---- claim ---------------------------------------------------------------------------------

fn caller_key(identity: &Identity) -> String {
    identity.user_id.clone().unwrap_or_else(|| format!("{:?}", identity.via))
}

async fn audit_attempt(ctx: &Ctx, identity: &Identity, serial: Option<&str>, serial_unknown: Option<&str>, outcome: &str, detail: Value) {
    let at = now();
    let mut payload = detail;
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("outcome".into(), json!(outcome));
        obj.insert("caller_id".into(), json!(caller_key(identity)));
        if let Some(s) = serial {
            obj.insert("serial".into(), json!(s));
        }
        if let Some(h) = serial_unknown {
            obj.insert("serial_hash".into(), json!(claim_limit::serial_hash(h)));
        }
    }
    let mut targets = vec![("farm".into(), "farm".into())];
    if let Some(s) = serial {
        targets.push(("unit".into(), s.to_owned()));
    }
    let e = ActivityEvent {
        id: id::new_id(id::EVENT),
        kind: format!("fleet.claim.{outcome}"),
        source: "farmer".into(),
        occurred_at: at,
        recorded_at: at,
        title: match outcome {
            "ok" | "reclaimed" => "Collar claimed".into(),
            _ => "Claim refused".into(),
        },
        body: None,
        payload,
        targets,
    };
    if let Err(err) = ctx.store().record_event(&e).await {
        tracing::warn!("claim audit: {err:#}");
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClaimBody {
    claim_code: Option<String>,
    /// The card's QR text, `OC2:<serial>:<device key>:<claim code>`.
    card: Option<String>,
    /// The unit's factory record (hex), when the maker sent one with it.
    factory_record: Option<String>,
    herd_id: String,
    name: Option<String>,
    animal_id: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct V1Provisioning {
    /// The collar's bearer key for protocol v1. Shown once; only its hash is kept.
    pub key: String,
    pub public_key: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    /// `provision {"v":1,…}` for the collar's console (protocol v1 README).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Claimed {
    pub unit: Unit,
    pub collar: Collar,
    pub v1: V1Provisioning,
    /// The claim code was checked here (see [`Unit::claim_checked`]). When
    /// false, the collar itself checks it when the bind reaches it, and the
    /// unit stays `claimed` until the collar reports.
    pub code_checked: bool,
    /// An in-service unit that was reset, claimed again from its card: the
    /// same collar, a new key (the old one stops working).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub reclaimed: bool,
    /// `POST /api/collars/{id}/undo` within [`op_core::UNDO_WINDOW_HOURS`] reverses this claim.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undo_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub undo_until: Option<chrono::DateTime<chrono::Utc>>,
}

async fn claim_serial(
    State(ctx): State<Ctx>,
    identity: Identity,
    Path(serial): Path<String>,
    ApiJson(b): ApiJson<ClaimBody>,
) -> ApiResult<(StatusCode, Json<Claimed>)> {
    require_claim_role(&identity)?;
    if b.card.is_some() || b.factory_record.is_some() {
        return Err(ApiError::bad_request("A card goes to POST /api/fleet/claim."));
    }
    let caller = caller_key(&identity);
    let target = claim_limit::target_for_serial(&serial);
    claim_limit::may_try(&ctx, &caller, Some(&serial), &target).await?;
    let code = match b.claim_code.as_deref().and_then(normalize_claim_code) {
        Some(c) => c,
        None => {
            audit_attempt(&ctx, &identity, Some(&serial), None, "wrong_code", json!({})).await;
            return Err(claim_limit::wrong(&ctx, &caller, Some(&serial), &target).await);
        }
    };
    let unit = match units::get_unit(&ctx, &serial).await? {
        Some(u) => u,
        None => {
            audit_attempt(&ctx, &identity, None, Some(&serial), "wrong_code", json!({})).await;
            return Err(claim_limit::wrong(&ctx, &caller, Some(&serial), &target).await);
        }
    };
    // Validate the code first. Wrong / unknown codes always look the same (404);
    // never reveal that a serial is claimed until the code checks out.
    let code_ok = codes_equal(unit.claim_code.as_deref(), &code) || stored_factory_code_matches(&ctx, &unit, &code).await?;
    if !code_ok {
        audit_attempt(&ctx, &identity, Some(&serial), None, "wrong_code", json!({})).await;
        return Err(claim_limit::wrong(&ctx, &caller, Some(&serial), &target).await);
    }
    claim(&ctx, &identity, unit, &b).await
}

/// `POST /api/fleet/claim`: by claim code, or by card.
async fn claim_any(State(ctx): State<Ctx>, identity: Identity, ApiJson(b): ApiJson<ClaimBody>) -> ApiResult<(StatusCode, Json<Claimed>)> {
    require_claim_role(&identity)?;
    let caller = caller_key(&identity);
    let unit = match (&b.card, &b.claim_code) {
        (Some(text), _) => {
            let card = Card::parse(text).ok_or_else(|| ApiError::bad_request("That isn't an OpenCollar card (OC2:serial:key:code)."))?;
            let target = claim_limit::target_for_serial(&card.serial);
            claim_limit::may_try(&ctx, &caller, Some(&card.serial), &target).await?;
            if b.claim_code.as_deref().is_some_and(|c| normalize_claim_code(c).as_deref() != Some(card.claim_code.as_str())) {
                audit_attempt(&ctx, &identity, Some(&card.serial), None, "wrong_code", json!({ "via": "card" })).await;
                return Err(claim_limit::wrong(&ctx, &caller, Some(&card.serial), &target).await);
            }
            match from_card(&ctx, &identity, &card, b.factory_record.as_deref(), &caller, &target).await? {
                FromCard::New(u) => u,
                FromCard::Reset(u) => return reclaim(&ctx, &identity, u, &b).await,
            }
        }
        (None, Some(code)) => {
            let code = match normalize_claim_code(code) {
                Some(c) => c,
                None => {
                    let target = claim_limit::target_for_code(code);
                    claim_limit::may_try(&ctx, &caller, None, &target).await?;
                    audit_attempt(&ctx, &identity, None, None, "wrong_code", json!({ "via": "code" })).await;
                    return Err(claim_limit::wrong(&ctx, &caller, None, &target).await);
                }
            };
            let target = claim_limit::target_for_code(&code);
            claim_limit::may_try(&ctx, &caller, None, &target).await?;
            let serial: Option<String> = sqlx::query_scalar("SELECT serial FROM fleet_units WHERE claim_code = ?").bind(&code).fetch_optional(ctx.db()).await?;
            match serial {
                Some(s) => units::find(&ctx, &s).await?,
                None => {
                    audit_attempt(&ctx, &identity, None, None, "wrong_code", json!({ "via": "code" })).await;
                    return Err(claim_limit::wrong(&ctx, &caller, None, &target).await);
                }
            }
        }
        (None, None) => return Err(ApiError::bad_request("Send the claim code or the card.")),
    };
    claim(&ctx, &identity, unit, &b).await
}

/// What a card names here.
enum FromCard {
    /// A unit to claim: known here and still holding its code, or new here.
    New(Unit),
    /// A unit in service here whose code was forgotten, and the card's code
    /// matches its factory record's claim check: the collar was reset.
    Reset(Unit),
}

/// Whether `code` is the one a record's claim check was made from
/// (protocol v2 `identity.md` §3, key 8): the typo check, before any airtime.
fn code_matches(check: &[u8; 16], code: &str, device_key: &[u8; 32]) -> bool {
    let id = op_protocol::v2::crypto::kid(device_key);
    op_protocol::v2::identity::claim_check(&op_protocol::v2::identity::claim_key(code, &id)) == *check
}

/// Conflict when a unit is already on this farm — never name the herd (enumeration).
fn already_claimed(serial: &str) -> ApiError {
    ApiError::conflict(format!("{serial} is already claimed."))
}

/// True when the unit's stored factory record's claim check matches `code`.
async fn stored_factory_code_matches(ctx: &Ctx, unit: &Unit, code: &str) -> ApiResult<bool> {
    let stored: Option<String> =
        sqlx::query_scalar("SELECT factory_record FROM fleet_units WHERE serial = ?").bind(&unit.serial).fetch_optional(ctx.db()).await?;
    let Some(hex) = stored else {
        return Ok(false);
    };
    let Ok(view) = crate::record::read(ctx, &hex).await else {
        return Ok(false);
    };
    let Some(check) = view.record.claim_check else {
        return Ok(false);
    };
    let Ok(pk) = op_protocol::decode_public_key(&unit.device_pubkey) else {
        return Ok(false);
    };
    Ok(code_matches(&check, code, pk.as_bytes()))
}

/// The unit a card names: known here (its key and code must match the card),
/// or registered from the card. The card is the proof of holding the unit;
/// the key on it is the one the collar proves itself with. A code is checked
/// against the factory record's claim check whenever there is a record.
async fn from_card(ctx: &Ctx, identity: &Identity, card: &Card, record: Option<&str>, caller: &str, target: &str) -> ApiResult<FromCard> {
    let pk = op_protocol::VerifyingKey::from_bytes(&card.device_key).map_err(|_| ApiError::bad_request("The card's device key isn't an Ed25519 key."))?;
    let pubkey = op_protocol::encode_public_key(&pk);
    if let Some(u) = units::get_unit(ctx, &card.serial).await? {
        if u.device_pubkey != pubkey {
            return Err(ApiError::conflict(format!("{} is known here with another device key than the card's.", card.serial)));
        }
        if codes_equal(u.claim_code.as_deref(), &card.claim_code) {
            return Ok(FromCard::New(u));
        }
        // In service, code erased at claim: the factory record still tells a
        // right code from a wrong one (reclaim after a factory reset). Card alone.
        if u.claim_code.is_none() && u.state == UnitState::InService {
            if stored_factory_code_matches(ctx, &u, &card.claim_code).await? {
                return Ok(FromCard::Reset(u));
            }
            audit_attempt(ctx, identity, Some(&card.serial), None, "wrong_code", json!({ "via": "card" })).await;
            return Err(claim_limit::wrong(ctx, caller, Some(&card.serial), target).await);
        }
        if u.state.on_farm() {
            // Code was erased at claim. Only confirm "already claimed" when the
            // card's code still checks out against the factory record.
            if stored_factory_code_matches(ctx, &u, &card.claim_code).await? {
                return Err(already_claimed(&u.serial));
            }
            audit_attempt(ctx, identity, Some(&card.serial), None, "wrong_code", json!({ "via": "card" })).await;
            return Err(claim_limit::wrong(ctx, caller, Some(&card.serial), target).await);
        }
        audit_attempt(ctx, identity, Some(&card.serial), None, "wrong_code", json!({ "via": "card" })).await;
        return Err(claim_limit::wrong(ctx, caller, Some(&card.serial), target).await);
    }
    let kid = keys::kid(&pk);
    // What the maker's record says about it, when it came with one.
    let rec = match record {
        Some(h) => {
            let r = crate::record::read(ctx, h).await.map_err(ApiError::bad_request)?;
            if r.record.serial != card.serial || r.record.device_key != card.device_key {
                return Err(ApiError::bad_request("The factory record is for another unit than the card."));
            }
            Some(r)
        }
        None => None,
    };
    let check = rec.as_ref().and_then(|r| r.record.claim_check);
    if check.is_some_and(|c| !code_matches(&c, &card.claim_code, &card.device_key)) {
        audit_attempt(ctx, identity, Some(&card.serial), None, "wrong_code", json!({ "via": "card" })).await;
        return Err(claim_limit::wrong(ctx, caller, Some(&card.serial), target).await);
    }
    let checked = check.is_some();
    let (class, hw_rev, fw) = match &rec {
        Some(r) => (r.record.class.name().to_owned(), r.record.hw_rev.clone(), r.record.fw.clone()),
        None => ("unknown".to_owned(), String::new(), String::new()),
    };
    let at = to_db(&now());
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    let other: Option<String> = sqlx::query_scalar("SELECT serial FROM fleet_units WHERE device_kid = ? OR claim_code = ?")
        .bind(&kid)
        .bind(&card.claim_code)
        .fetch_optional(&mut *tx)
        .await?;
    if let Some(other) = other {
        return Err(ApiError::conflict(format!("The card's key or code is already registered to {other}.")));
    }
    sqlx::query(
        "INSERT INTO fleet_units (serial, device_class, hw_rev, fw_version, device_pubkey, device_kid, state, origin, claim_code, claim_checked,
           factory_record, maker, maker_kid, maker_verified, made_at, image_keys, caps, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, ?, 'provisioned', 'card', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&card.serial)
    .bind(&class)
    .bind(&hw_rev)
    .bind(&fw)
    .bind(&pubkey)
    .bind(&kid)
    .bind(&card.claim_code)
    .bind(checked)
    .bind(rec.as_ref().map(|r| r.view.cose.clone()))
    .bind(rec.as_ref().map(|r| r.record.maker.clone()))
    .bind(rec.as_ref().map(|r| r.view.maker_kid.clone()))
    .bind(rec.as_ref().is_some_and(|r| r.view.verified) as i64)
    .bind(rec.as_ref().and_then(|r| chrono::DateTime::from_timestamp(r.record.made_at as i64, 0)).map(|t| to_db(&t)))
    .bind(
        serde_json::to_string(&rec.as_ref().map(|r| r.record.image_keys.iter().map(|k| keys::hex(k)).collect::<Vec<_>>()).unwrap_or_default())
            .map_err(anyhow::Error::from)?,
    )
    .bind(serde_json::to_string(&rec.as_ref().map(|r| r.record.caps.clone()).unwrap_or_default()).map_err(anyhow::Error::from)?)
    .bind(&at)
    .bind(&at)
    .execute(&mut *tx)
    .await?;
    sqlx::query("INSERT INTO fleet_unit_keys (serial, device_pubkey, device_kid, from_at) VALUES (?, ?, ?, ?)")
        .bind(&card.serial)
        .bind(&pubkey)
        .bind(&kid)
        .bind(&at)
        .execute(&mut *tx)
        .await?;
    units::log(
        &mut tx,
        &card.serial,
        "registered",
        json!({ "from_card": true, "device_class": class, "device_id": kid, "maker": rec.as_ref().map(|r| r.record.maker.clone()), "code_checked": checked }),
        who(identity).as_deref(),
    )
    .await?;
    let unit = units::load(&mut tx, &card.serial).await?.ok_or_else(|| ApiError::internal("unit vanished"))?;
    tx.commit().await?;
    Ok(FromCard::New(unit))
}

/// A reset unit claimed again from its card: the same collar (its history,
/// its herd, its animal), a new collar key, so the old one stops working.
/// The collar holds nothing after a reset, so its fence state starts over.
/// It is `claimed` until it reports under the new key. Moving it to another
/// herd or animal is the collar's own setting, afterwards.
async fn reclaim(ctx: &Ctx, identity: &Identity, unit: Unit, b: &ClaimBody) -> ApiResult<(StatusCode, Json<Claimed>)> {
    let collar_id = unit.collar_id.clone().ok_or_else(|| ApiError::conflict(format!("{} has no collar here to return to.", unit.serial)))?;
    let collar = collar_record(ctx, &collar_id).await?;
    if b.herd_id != collar.herd_id {
        let herd = ctx.store().get_herd(&collar.herd_id).await?.map(|h| h.name).unwrap_or_else(|| collar.herd_id.clone());
        return Err(ApiError::conflict(format!(
            "{} comes back as {} in {herd}. Claim it there, then move it from the collar's page.",
            unit.serial, collar.name
        )));
    }
    if b.animal_id.as_ref().is_some_and(|a| collar.animal_id.as_ref() != Some(a)) {
        return Err(ApiError::conflict(format!("{} comes back on the animal it was on. Change the animal from the collar's page.", unit.serial)));
    }
    let bearer = op_core::keys::new_collar_key();
    let at = to_db(&now());
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    // Card alone is enough after a factory reset (no proof-from-collar).
    let n = sqlx::query("UPDATE fleet_units SET state = 'claimed', claimed_at = ?, updated_at = ? WHERE serial = ? AND state = 'in_service' AND claim_code IS NULL AND generation = ? AND collar_id = ?")
        .bind(&at)
        .bind(&at)
        .bind(&unit.serial)
        .bind(unit.generation)
        .bind(&collar_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n != 1 {
        return Err(ApiError::conflict(format!("{} changed meanwhile. Look at it again.", unit.serial)));
    }
    sqlx::query("UPDATE collars SET key_hash = ?, boundary_version = NULL, state = 'unknown', outside_since = NULL WHERE id = ?")
        .bind(op_core::keys::hash_key(&bearer))
        .bind(&collar_id)
        .execute(&mut *tx)
        .await?;
    units::log(&mut tx, &unit.serial, "reclaimed", json!({ "collar_id": collar_id, "from_card": true }), who(identity).as_deref()).await?;
    tx.commit().await?;
    tracing::info!(serial = %unit.serial, collar = %collar_id, "unit reclaimed from its card after a reset");
    finish_claim(ctx, identity, &unit.serial, &collar_id, &collar.herd_id, collar.animal_id.clone(), bearer, true, StatusCode::OK).await
}

/// The unit becomes a collar in the herd, on the animal when one is named.
async fn claim(ctx: &Ctx, identity: &Identity, unit: Unit, b: &ClaimBody) -> ApiResult<(StatusCode, Json<Claimed>)> {
    match unit.state {
        UnitState::Provisioned => {}
        UnitState::Claimed | UnitState::InService => {
            return Err(already_claimed(&unit.serial));
        }
        _ => return Err(ApiError::conflict(format!("{} isn't provisioned yet, so it can't be claimed.", unit.serial))),
    }
    if ctx.store().get_herd(&b.herd_id).await?.is_none() {
        return Err(ApiError::bad_request("No such herd."));
    }
    // A real unit never joins a simulated herd (op_core::sim).
    if op_core::sim::herd_is_simulated(ctx.db(), &b.herd_id).await? {
        return Err(ApiError::conflict(op_core::sim::WRONG_SIDE));
    }
    let name = match b.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) if n.len() > 200 => return Err(ApiError::bad_request("The collar name is too long.")),
        Some(n) => n.to_owned(),
        None => unit.serial.clone(),
    };
    if let Some(aid) = &b.animal_id {
        let a = ctx.store().get_animal(aid).await?.ok_or_else(|| ApiError::bad_request("No such animal."))?;
        if a.herd_id != b.herd_id {
            return Err(ApiError::bad_request(format!("{} is in another herd.", a.tag)));
        }
        if a.removed_at.is_some() {
            return Err(ApiError::bad_request(format!("{} is no longer on the farm.", a.tag)));
        }
        if a.collar_id.is_some() {
            return Err(ApiError::conflict(format!("{} already wears a collar.", a.tag)));
        }
    }
    let collar_id = id::new_id(id::COLLAR);
    let bearer = op_core::keys::new_collar_key();
    let at = to_db(&now());
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    sqlx::query("INSERT INTO collars (id, name, herd_id, key_hash, state, created_at, animal_id) VALUES (?, ?, ?, ?, 'unknown', ?, ?)")
        .bind(&collar_id)
        .bind(&name)
        .bind(&b.herd_id)
        .bind(op_core::keys::hash_key(&bearer))
        .bind(&at)
        .bind(&b.animal_id)
        .execute(&mut *tx)
        .await?;
    if let Some(aid) = &b.animal_id {
        let n = sqlx::query("UPDATE animals SET collar_id = ? WHERE id = ? AND herd_id = ? AND collar_id IS NULL AND removed_at IS NULL")
            .bind(&collar_id)
            .bind(aid)
            .bind(&b.herd_id)
            .execute(&mut *tx)
            .await?
            .rows_affected();
        if n != 1 {
            return Err(ApiError::conflict("The animal took another collar meanwhile. Nothing was claimed."));
        }
    }
    // Claim code erased at claim time (identity.md §4 / server.md): single-use
    // until first report is no longer needed for secrecy; reclaim uses the
    // factory record's claim check while in_service.
    let n = sqlx::query(
        "UPDATE fleet_units SET collar_id = ?, state = 'claimed', claim_code = NULL, claimed_at = ?, updated_at = ? WHERE serial = ? AND state = 'provisioned'",
    )
    .bind(&collar_id)
    .bind(&at)
    .bind(&at)
    .bind(&unit.serial)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if n != 1 {
        return Err(ApiError::conflict(format!("{} was claimed meanwhile.", unit.serial)));
    }
    units::log(&mut tx, &unit.serial, "claimed", json!({ "collar_id": collar_id, "herd_id": b.herd_id, "animal_id": b.animal_id }), who(identity).as_deref())
        .await?;
    tx.commit().await?;
    tracing::info!(serial = %unit.serial, collar = %collar_id, herd = %b.herd_id, "unit claimed");

    finish_claim(ctx, identity, &unit.serial, &collar_id, &b.herd_id, b.animal_id.clone(), bearer, false, StatusCode::CREATED).await
}

/// The claim's answer: the collar, the unit, and the v1 key for today's firmware.
async fn finish_claim(
    ctx: &Ctx,
    identity: &Identity,
    serial: &str,
    collar_id: &str,
    herd_id: &str,
    animal_id: Option<String>,
    bearer: String,
    reclaimed: bool,
    status: StatusCode,
) -> ApiResult<(StatusCode, Json<Claimed>)> {
    claim_limit::clear_serial(ctx, serial).await;
    let collar = collar_record(ctx, collar_id).await?;
    let undo_id = record_claim_undo(ctx, identity, &collar, serial, reclaimed).await;
    let outcome = if reclaimed { "reclaimed" } else { "ok" };
    audit_attempt(
        ctx,
        identity,
        Some(serial),
        None,
        outcome,
        json!({ "collar_id": collar_id, "herd_id": herd_id, "animal_id": animal_id, "undo_id": undo_id }),
    )
    .await;
    ctx.publish(Event::Collar { collar: collar.clone() });
    ctx.publish(Event::Claimed {
        serial: serial.to_owned(),
        collar_id: collar_id.to_owned(),
        herd_id: herd_id.to_owned(),
        animal_id: animal_id.clone(),
        reclaimed,
        undo_id: undo_id.clone(),
        undo_until: undo_id.as_ref().map(|_| now() + undo_window()),
    });
    units::publish(ctx, serial).await;
    let endpoint = collar_endpoint(ctx).await?;
    let public_key = ctx.public_key_b64();
    // The v1 card payload, keys in the order v c h k e s (op-import cards).
    let line = endpoint.as_ref().map(|e| {
        format!(r#"provision {{"v":1,"c":{},"h":{},"k":{},"e":{},"s":{}}}"#, json!(collar_id), json!(herd_id), json!(bearer), json!(e), json!(public_key))
    });
    let why = endpoint.is_none().then(|| NO_COLLAR_ADDRESS.to_owned());
    let unit = units::find(ctx, serial).await?;
    let code_checked = unit.claim_checked || reclaimed;
    let undo_until = undo_id.as_ref().map(|_| now() + undo_window());
    Ok((
        status,
        Json(Claimed { unit, collar, v1: V1Provisioning { key: bearer, public_key, endpoint, line, why }, code_checked, reclaimed, undo_id, undo_until }),
    ))
}

/// Undo a claim within [`op_core::UNDO_WINDOW_HOURS`]: reverse it back to a provisioned unit.
async fn record_claim_undo(ctx: &Ctx, identity: &Identity, collar: &Collar, serial: &str, reclaimed: bool) -> Option<String> {
    if reclaimed {
        // Reclaim keeps the same collar/herd; no herd to restore. Skip undo row.
        return None;
    }
    let id = id::new_id("undo");
    let at = now();
    let expires = at + undo_window();
    let ok = sqlx::query(
        "INSERT INTO collar_undos (id, collar_id, kind, serial, to_herd_id, to_animal_id, by_user, created_at, expires_at)
         VALUES (?, ?, 'claim', ?, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(&collar.id)
    .bind(serial)
    .bind(&collar.herd_id)
    .bind(&collar.animal_id)
    .bind(who(identity))
    .bind(to_db(&at))
    .bind(to_db(&expires))
    .execute(ctx.db())
    .await;
    match ok {
        Ok(_) => Some(id),
        Err(e) => {
            tracing::warn!("claim undo row: {e:#}");
            None
        }
    }
}

/// The collar as `/api/collars/:id` returns it.
async fn collar_record(ctx: &Ctx, id: &str) -> anyhow::Result<Collar> {
    let row = sqlx::query("SELECT * FROM collars WHERE id = ?").bind(id).fetch_one(ctx.db()).await?;
    op_core::store::collar_from_row(&row)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_codes_print_in_groups_of_four() {
        assert_eq!(grouped("7Q4K9M2XH8D3TV5W"), "7Q4K-9M2X-H8D3-TV5W");
        let c = op_protocol::v2::identity::new_claim_code();
        assert_eq!(normalize_claim_code(&grouped(&c).to_lowercase()).as_deref(), Some(c.as_str()));
    }
}
