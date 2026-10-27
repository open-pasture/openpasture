//! Firmware releases and how they reach collars.
//!
//! What most farmers want: updates are automatic and invisible. A signed
//! release goes out in waves to every collar it fits; nothing is applied
//! while the herd is being moved or the collar is out on an escape, or below
//! the battery floor; the collar rolls itself back when a new image doesn't
//! confirm, and the server records it. The farmer sees versions and can
//! pause. Builders who sign their own firmware enrol their image key.
//!
//! A release is the protocol v2 release manifest (message type 65, a
//! COSE_Sign1 by the OpenCollar release key built in here
//! ([`OPENCOLLAR_RELEASE_KEYS`]) or an image key the owner enrolled). It
//! reaches a collar inside the protocol's firmware command (type 9); collars
//! that don't report the `fota` capability are `unsupported`.

pub mod rollout;

use axum::body::Bytes;
use axum::extract::{DefaultBodyLimit, Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post, put};
use axum::{Json, Router};
use chrono::{DateTime, Utc};
use op_core::time::{now, opt_from_db, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Identity, id};
use op_protocol::v2::firmware::Manifest;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;

use crate::keys::{self, hex, unhex};
use crate::who;

pub use rollout::{FirmwareOffer, FwResult, UnitFirmware, offer_for, record_result, status_for_collar};

/// OpenCollar's firmware release keys: (name, base64 Ed25519 public key).
/// Empty until the first signed release is published with its key; until
/// then only image keys an owner enrols here can sign releases.
pub const OPENCOLLAR_RELEASE_KEYS: &[(&str, &str)] = &[];

/// Largest image accepted, bytes.
pub const MAX_IMAGE: usize = 4 << 20;

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/fleet/firmware", get(list).post(register))
        .route("/api/fleet/firmware/{id}/image", put(upload).layer(DefaultBodyLimit::max(MAX_IMAGE + 1024)))
        .route("/api/fleet/firmware/{id}/withdraw", post(withdraw))
        .route("/api/fleet/firmware/policy", get(get_policy).put(put_policy))
        .route("/api/fleet/firmware/rollouts", get(rollout::list).post(rollout::create))
        .route("/api/fleet/firmware/rollouts/{id}/{action}", post(rollout::act))
        .route("/api/fleet/firmware/units", get(rollout::units))
        .route("/api/fleet/signing-keys", get(list_keys).post(enrol_key))
        .route("/api/fleet/signing-keys/{kid}/revoke", post(revoke_key))
}

// ---- signing keys --------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum KeyKind {
    /// Built into the server: OpenCollar's release keys.
    Vendor,
    /// Enrolled by the owner.
    Owner,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Purpose {
    /// Signs firmware releases.
    Image,
    /// Signs factory records (another builder's units).
    Maker,
}

impl op_core::DbEnum for Purpose {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SigningKey {
    pub kid: String,
    pub name: String,
    pub public_key: String,
    pub kind: KeyKind,
    pub purpose: Purpose,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
}

fn vendor_keys() -> Vec<SigningKey> {
    OPENCOLLAR_RELEASE_KEYS
        .iter()
        .filter_map(|(name, b64)| {
            let key = op_protocol::decode_public_key(b64).ok()?;
            Some(SigningKey {
                kid: keys::kid(&key),
                name: (*name).into(),
                public_key: (*b64).into(),
                kind: KeyKind::Vendor,
                purpose: Purpose::Image,
                created_at: None,
                revoked_at: None,
            })
        })
        .collect()
}

/// Every key: built in, enrolled, and revoked ones (with `revoked_at`).
pub async fn signing_keys(ctx: &Ctx) -> anyhow::Result<Vec<SigningKey>> {
    let revoked: Vec<(String, String)> = sqlx::query_as("SELECT kid, at FROM fleet_revoked_kids").fetch_all(ctx.db()).await?;
    let revoked_at = |kid: &str| -> anyhow::Result<Option<DateTime<Utc>>> { revoked.iter().find(|(k, _)| k == kid).map(|(_, at)| op_core::time::from_db(at)).transpose() };
    let mut out = Vec::new();
    for mut k in vendor_keys() {
        k.revoked_at = revoked_at(&k.kid)?;
        out.push(k);
    }
    for r in sqlx::query("SELECT * FROM fleet_signing_keys ORDER BY created_at").fetch_all(ctx.db()).await? {
        let kid: String = r.try_get("kid")?;
        let own: Option<DateTime<Utc>> = opt_from_db(r.try_get("revoked_at")?)?;
        out.push(SigningKey {
            name: r.try_get("name")?,
            public_key: r.try_get("public_key")?,
            kind: KeyKind::Owner,
            purpose: op_core::DbEnum::from_db(&r.try_get::<String, _>("purpose")?)?,
            created_at: Some(op_core::time::from_db(&r.try_get::<String, _>("created_at")?)?),
            revoked_at: own.or(revoked_at(&kid)?),
            kid,
        });
    }
    Ok(out)
}

/// The unrevoked keys trusted for `purpose`.
pub async fn trusted_keys(ctx: &Ctx, purpose: Purpose) -> anyhow::Result<Vec<SigningKey>> {
    Ok(signing_keys(ctx).await?.into_iter().filter(|k| k.purpose == purpose && k.revoked_at.is_none()).collect())
}

/// The unrevoked image key with this kid.
pub async fn trusted_key(ctx: &Ctx, kid: &str) -> anyhow::Result<Option<SigningKey>> {
    Ok(trusted_keys(ctx, Purpose::Image).await?.into_iter().find(|k| k.kid == kid))
}

async fn list_keys(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<SigningKey>>> {
    Ok(Json(signing_keys(&ctx).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewKey {
    name: String,
    public_key: String,
    purpose: Option<Purpose>,
}

/// `POST /api/fleet/signing-keys` (owner): trust releases (`image`, the
/// default) or factory records (`maker`) this key signs.
async fn enrol_key(State(ctx): State<Ctx>, identity: Identity, ApiJson(b): ApiJson<NewKey>) -> ApiResult<(StatusCode, Json<SigningKey>)> {
    let name = b.name.trim();
    if name.is_empty() || name.len() > 100 {
        return Err(ApiError::bad_request("A signing key needs a name of 1–100 characters."));
    }
    let key = keys::parse_public_key(&b.public_key, "public_key").map_err(ApiError::bad_request)?;
    let kid = keys::kid(&key);
    if signing_keys(&ctx).await?.iter().any(|k| k.kid == kid) {
        return Err(ApiError::conflict(format!("Key {kid} is already known here.")));
    }
    let purpose = b.purpose.unwrap_or(Purpose::Image);
    sqlx::query("INSERT INTO fleet_signing_keys (kid, purpose, name, public_key, created_at, created_by) VALUES (?, ?, ?, ?, ?, ?)")
        .bind(&kid)
        .bind(op_core::DbEnum::as_db(&purpose))
        .bind(name)
        .bind(op_protocol::encode_public_key(&key))
        .bind(to_db(&now()))
        .bind(who(&identity))
        .execute(ctx.db())
        .await?;
    tracing::info!(kid = %kid, purpose = ?purpose, "signing key enrolled");
    let k = signing_keys(&ctx).await?.into_iter().find(|k| k.kid == kid).ok_or_else(|| ApiError::internal("key vanished"))?;
    Ok((StatusCode::CREATED, Json(k)))
}

/// `POST /api/fleet/signing-keys/:kid/revoke` (owner). Its releases stop
/// rolling out. Built-in release keys are revoked only by a signed release.
async fn revoke_key(State(ctx): State<Ctx>, Path(kid): Path<String>) -> ApiResult<Json<SigningKey>> {
    if vendor_keys().iter().any(|k| k.kid == kid) {
        return Err(ApiError::bad_request("OpenCollar's release keys are revoked by a signed release, not here."));
    }
    let n = sqlx::query("UPDATE fleet_signing_keys SET revoked_at = COALESCE(revoked_at, ?) WHERE kid = ?").bind(to_db(&now())).bind(&kid).execute(ctx.db()).await?.rows_affected();
    if n == 0 {
        return Err(ApiError::not_found("No such signing key."));
    }
    rollout::stop_for_signer(&ctx, &kid).await?;
    tracing::info!(kid = %kid, "signing key revoked");
    Ok(Json(signing_keys(&ctx).await?.into_iter().find(|k| k.kid == kid).ok_or_else(|| ApiError::internal("key vanished"))?))
}

// ---- releases ------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Signer {
    pub kid: String,
    pub name: String,
    pub kind: KeyKind,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Firmware {
    pub id: String,
    pub version: String,
    pub classes: Vec<String>,
    pub hw_revs: Vec<String>,
    pub sha256: String,
    pub size: i64,
    pub signer_kid: String,
    pub security_counter: i64,
    /// Percent, from the manifest.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_battery: Option<i64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub revoke: Vec<String>,
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// The signed manifest, hex.
    pub manifest: String,
    pub has_image: bool,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub withdrawn_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signer: Option<Signer>,
}

pub(crate) fn firmware_from_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Firmware> {
    Ok(Firmware {
        id: r.try_get("id")?,
        version: r.try_get("version")?,
        classes: serde_json::from_str(&r.try_get::<String, _>("classes")?)?,
        hw_revs: serde_json::from_str(&r.try_get::<String, _>("hw_revs")?)?,
        sha256: r.try_get("sha256")?,
        size: r.try_get("size")?,
        signer_kid: r.try_get("signer_kid")?,
        security_counter: r.try_get("security_counter")?,
        min_battery: r.try_get("min_battery")?,
        revoke: serde_json::from_str(&r.try_get::<String, _>("revoke")?)?,
        channel: r.try_get("channel")?,
        notes: r.try_get("notes")?,
        manifest: r.try_get("manifest")?,
        has_image: r.try_get::<i64, _>("has_image")? != 0,
        created_at: op_core::time::from_db(&r.try_get::<String, _>("created_at")?)?,
        withdrawn_at: opt_from_db(r.try_get("withdrawn_at")?)?,
        signer: None,
    })
}

async fn with_signer(ctx: &Ctx, mut f: Firmware) -> anyhow::Result<Firmware> {
    f.signer = signing_keys(ctx).await?.into_iter().find(|k| k.kid == f.signer_kid).map(|k| Signer { kid: k.kid, name: k.name, kind: k.kind });
    Ok(f)
}

pub async fn get_firmware(ctx: &Ctx, id: &str) -> anyhow::Result<Option<Firmware>> {
    let row = sqlx::query("SELECT * FROM fleet_firmware WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?;
    match row {
        Some(r) => Ok(Some(with_signer(ctx, firmware_from_row(&r)?).await?)),
        None => Ok(None),
    }
}

async fn find(ctx: &Ctx, id: &str) -> ApiResult<Firmware> {
    get_firmware(ctx, id).await?.ok_or_else(|| ApiError::not_found("No such firmware release."))
}

async fn list(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<Firmware>>> {
    let rows = sqlx::query("SELECT * FROM fleet_firmware ORDER BY created_at DESC, id DESC").fetch_all(ctx.db()).await?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push(with_signer(&ctx, firmware_from_row(r)?).await?);
    }
    Ok(Json(out))
}

/// `1.2.3`, `v1.2.3` or `1.2.3-beta.1` as comparable numbers; a pre-release
/// sorts before its release.
pub fn version_key(v: &str) -> Option<(u64, u64, u64, bool)> {
    let v = v.trim().trim_start_matches('v');
    let (core, pre) = match v.split_once('-') {
        Some((c, _)) => (c, true),
        None => (v, false),
    };
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let major = parts.next()??;
    let minor = parts.next().flatten().unwrap_or(0);
    let patch = parts.next().flatten().unwrap_or(0);
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch, !pre))
}

/// Whether `a` is older than `b`. Unreadable versions are never older.
pub fn older(a: &str, b: &str) -> bool {
    match (version_key(a), version_key(b)) {
        (Some(x), Some(y)) => x < y,
        _ => false,
    }
}

/// What the server needs a manifest to say before it rolls one out.
pub(crate) fn check_manifest(m: &Manifest) -> Result<(), String> {
    if version_key(&m.version).is_none() {
        return Err("The release's version isn't like 1.2.3.".into());
    }
    if m.size == 0 || m.size > MAX_IMAGE as u64 {
        return Err(format!("The image is 1–{MAX_IMAGE} bytes."));
    }
    if m.classes.is_empty() || m.classes.iter().any(|c| !c.is_known()) {
        return Err("The release names no device class, or one this server doesn't know.".into());
    }
    if m.hw_revs.is_empty() {
        return Err("The release names no board revision, so it fits no collar.".into());
    }
    if m.min_battery.is_some_and(|b| b > 100) {
        return Err("min_battery is a percentage.".into());
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewRelease {
    /// The signed manifest (COSE_Sign1), hex.
    manifest: String,
    channel: Option<String>,
    notes: Option<String>,
}

/// `POST /api/fleet/firmware` (owner): register a signed release manifest.
/// Kids it revokes stop being trusted here.
async fn register(State(ctx): State<Ctx>, identity: Identity, ApiJson(b): ApiJson<NewRelease>) -> ApiResult<(StatusCode, Json<Firmware>)> {
    let bytes = unhex(b.manifest.trim()).ok_or_else(|| ApiError::bad_request("manifest is the signed manifest's bytes as hex."))?;
    let (env, m) = Manifest::read(&bytes).map_err(|e| ApiError::bad_request(format!("That isn't a release manifest ({}).", e.name())))?;
    check_manifest(&m).map_err(ApiError::bad_request)?;
    let channel = b.channel.unwrap_or_else(|| "stable".into());
    if channel != "stable" && channel != "beta" {
        return Err(ApiError::bad_request("channel is stable or beta."));
    }
    if b.notes.as_deref().is_some_and(|n| n.len() > 2000) {
        return Err(ApiError::bad_request("Notes are up to 2000 characters."));
    }
    let kid = hex(&env.kid);
    let signer = trusted_key(&ctx, &kid)
        .await?
        .ok_or_else(|| ApiError::bad_request(format!("Key {kid} isn't a firmware key this server trusts. The owner can enrol it under Fleet, Firmware.")))?;
    let key = keys::parse_public_key(&signer.public_key, "signer key").map_err(ApiError::internal)?;
    env.verify(&key).map_err(|_| ApiError::bad_request(format!("The manifest's signature doesn't verify against key {kid}.")))?;
    let sha = hex(&m.digest);
    let taken: Option<String> = sqlx::query_scalar("SELECT id FROM fleet_firmware WHERE sha256 = ? AND version = ?").bind(&sha).bind(&m.version).fetch_optional(ctx.db()).await?;
    if taken.is_some() {
        return Err(ApiError::conflict(format!("Release {} is already here.", m.version)));
    }
    let fw_id = id::new_id("fwi");
    let has_image = std::fs::read(image_path(&ctx, &sha)).is_ok_and(|b| hex(&Sha256::digest(&b)) == sha);
    let classes: Vec<&str> = m.classes.iter().map(|c| c.name()).collect();
    let revoke: Vec<String> = m.revoke.iter().map(|k| hex(k)).collect();
    let at = to_db(&now());
    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    sqlx::query(
        "INSERT INTO fleet_firmware (id, version, classes, hw_revs, sha256, size, signer_kid, security_counter, min_battery, revoke, channel, notes, manifest, has_image, created_at, created_by)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&fw_id)
    .bind(&m.version)
    .bind(serde_json::to_string(&classes).map_err(anyhow::Error::from)?)
    .bind(serde_json::to_string(&m.hw_revs).map_err(anyhow::Error::from)?)
    .bind(&sha)
    .bind(m.size as i64)
    .bind(&kid)
    .bind(i64::from(m.security_counter))
    .bind(m.min_battery.map(i64::from))
    .bind(serde_json::to_string(&revoke).map_err(anyhow::Error::from)?)
    .bind(&channel)
    .bind(&b.notes)
    .bind(hex(&bytes))
    .bind(has_image as i64)
    .bind(&at)
    .bind(who(&identity))
    .execute(&mut *tx)
    .await?;
    for r in &revoke {
        sqlx::query("INSERT OR IGNORE INTO fleet_revoked_kids (kid, at, firmware_id) VALUES (?, ?, ?)").bind(r).bind(&at).bind(&fw_id).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    for r in &revoke {
        rollout::stop_for_signer(&ctx, r).await?;
    }
    tracing::info!(firmware = %fw_id, version = %m.version, classes = ?classes, "firmware release registered");
    rollout::wake();
    Ok((StatusCode::CREATED, Json(find(&ctx, &fw_id).await?)))
}

/// Where an image's bytes live: `<data>/firmware/<sha256>.bin`.
pub fn image_path(ctx: &Ctx, sha256: &str) -> std::path::PathBuf {
    ctx.data_dir().join("firmware").join(format!("{sha256}.bin"))
}

/// The image with this digest, for the collar endpoint that serves it to
/// anchors (`<endpoint>/fw/<digest hex>` in protocol v2).
pub async fn image_bytes(ctx: &Ctx, sha256: &str) -> anyhow::Result<Option<Vec<u8>>> {
    let known: Option<i64> = sqlx::query_scalar("SELECT has_image FROM fleet_firmware WHERE sha256 = ? AND withdrawn_at IS NULL LIMIT 1").bind(sha256).fetch_optional(ctx.db()).await?;
    if known != Some(1) {
        return Ok(None);
    }
    let path = image_path(ctx, sha256);
    Ok(tokio::fs::read(path).await.ok().filter(|b| hex(&Sha256::digest(b)) == sha256))
}

/// `PUT /api/fleet/firmware/:id/image` (owner): the image bytes, checked
/// against the manifest's digest and size.
async fn upload(State(ctx): State<Ctx>, Path(fw_id): Path<String>, body: Bytes) -> ApiResult<Json<Firmware>> {
    let fw = find(&ctx, &fw_id).await?;
    if body.len() as i64 != fw.size {
        return Err(ApiError::bad_request(format!("The image is {} bytes; the manifest says {}.", body.len(), fw.size)));
    }
    if hex(&Sha256::digest(&body)) != fw.sha256 {
        return Err(ApiError::bad_request("The image's SHA-256 doesn't match the manifest."));
    }
    let path = image_path(&ctx, &fw.sha256);
    let dir = path.parent().map(std::path::Path::to_path_buf).unwrap_or_default();
    let bytes = body.to_vec();
    tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join(format!(".{}.part", ulid::Ulid::new()));
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    })
    .await
    .map_err(anyhow::Error::from)??;
    sqlx::query("UPDATE fleet_firmware SET has_image = 1 WHERE sha256 = ?").bind(&fw.sha256).execute(ctx.db()).await?;
    rollout::wake();
    Ok(Json(find(&ctx, &fw_id).await?))
}

/// `POST /api/fleet/firmware/:id/withdraw` (owner): no collar gets it any
/// more; its rollouts are cancelled.
async fn withdraw(State(ctx): State<Ctx>, Path(fw_id): Path<String>) -> ApiResult<Json<Firmware>> {
    find(&ctx, &fw_id).await?;
    sqlx::query("UPDATE fleet_firmware SET withdrawn_at = COALESCE(withdrawn_at, ?) WHERE id = ?").bind(to_db(&now())).bind(&fw_id).execute(ctx.db()).await?;
    rollout::cancel_for_firmware(&ctx, &fw_id, "Release withdrawn").await?;
    tracing::info!(firmware = %fw_id, "firmware withdrawn");
    Ok(Json(find(&ctx, &fw_id).await?))
}

// ---- policy ----------------------------------------------------------------------------------

const POLICY_KEY: &str = "fleet.firmware";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Auto,
    Paused,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FirmwarePolicy {
    pub mode: Mode,
    /// `stable`, or `beta` for beta releases too.
    pub channel: String,
    /// No update below this battery level (0–1); the collar's own floor is 40 %.
    pub battery_floor: f64,
    /// The rest of the first herd gets a release this long after its first
    /// collar (if that one confirmed); every herd three times this long after.
    pub wave_hours: f64,
    /// Herds held at the version they run.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub held_herds: Vec<String>,
}

impl Default for FirmwarePolicy {
    fn default() -> Self {
        Self { mode: Mode::Auto, channel: "stable".into(), battery_floor: f64::from(op_protocol::v2::firmware::MIN_BATTERY_FLOOR) / 100.0, wave_hours: 24.0, held_herds: Vec::new() }
    }
}

pub async fn policy(ctx: &Ctx) -> anyhow::Result<FirmwarePolicy> {
    Ok(ctx.store().get_setting::<FirmwarePolicy>(POLICY_KEY).await?.unwrap_or_default())
}

async fn get_policy(State(ctx): State<Ctx>) -> ApiResult<Json<FirmwarePolicy>> {
    Ok(Json(policy(&ctx).await?))
}

async fn put_policy(State(ctx): State<Ctx>, ApiJson(p): ApiJson<FirmwarePolicy>) -> ApiResult<Json<FirmwarePolicy>> {
    if p.channel != "stable" && p.channel != "beta" {
        return Err(ApiError::bad_request("channel is stable or beta."));
    }
    let floor = f64::from(op_protocol::v2::firmware::MIN_BATTERY_FLOOR) / 100.0;
    if !(floor..=0.9).contains(&p.battery_floor) {
        return Err(ApiError::bad_request(format!("The battery floor is between {:.0} % (the collar's own) and 90 %.", floor * 100.0)));
    }
    if !(1.0..=24.0 * 14.0).contains(&p.wave_hours) {
        return Err(ApiError::bad_request("A stage runs from 1 hour to 14 days."));
    }
    for h in &p.held_herds {
        if ctx.store().get_herd(h).await?.is_none() {
            return Err(ApiError::bad_request("No such herd."));
        }
    }
    ctx.store().set_setting(POLICY_KEY, &p).await?;
    rollout::wake();
    Ok(Json(p))
}

#[derive(Deserialize)]
pub(crate) struct UnitsQuery {
    pub herd_id: Option<String>,
    pub rollout_id: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use op_protocol::v2::registry::DeviceClass;

    #[test]
    fn versions_compare_as_numbers_with_prereleases_first() {
        assert!(older("0.2.0", "0.10.0"));
        assert!(older("1.0.0-beta.1", "1.0.0"));
        assert!(older("v1.2", "1.2.1"));
        assert!(!older("1.2.0", "1.2.0"));
        assert!(!older("junk", "1.0.0"), "unreadable is never older");
        assert_eq!(version_key("1.2.3.4"), None);
    }

    fn manifest() -> Manifest {
        Manifest {
            version: "2.1.0".into(),
            digest: [1; 32],
            size: 1000,
            classes: vec![DeviceClass::Node],
            hw_revs: vec!["N1.0".into()],
            security_counter: 1,
            min_battery: Some(50),
            revoke: vec![],
        }
    }

    #[test]
    fn manifests_must_fit_some_collar() {
        check_manifest(&manifest()).unwrap();
        for (m, want) in [
            (Manifest { version: "two".into(), ..manifest() }, "version"),
            (Manifest { size: 0, ..manifest() }, "bytes"),
            (Manifest { classes: vec![], ..manifest() }, "class"),
            (Manifest { classes: vec![DeviceClass::Other(99)], ..manifest() }, "class"),
            (Manifest { hw_revs: vec![], ..manifest() }, "board"),
            (Manifest { min_battery: Some(140), ..manifest() }, "percentage"),
        ] {
            let e = check_manifest(&m).unwrap_err();
            assert!(e.contains(want), "{e} / {want}");
        }
    }
}
