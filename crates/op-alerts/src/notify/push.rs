//! Web Push: alerts and the brief on a phone or browser, free, with no
//! Twilio. It works only while the server is reached over https (its public
//! URL): browsers subscribe to push, and install the app, only from a secure
//! origin.
//!
//! - **Keys.** One VAPID key pair per farm (RFC 8292), made the first time
//!   anyone asks for it: the private key in the `vapid_private_key` secret
//!   (PKCS#8, base64url), the public key in the `push` setting, which the app
//!   hands to `PushManager.subscribe`.
//! - **Subscriptions.** `push_subscriptions` holds each browser's
//!   `PushSubscription` (endpoint, `p256dh`, `auth`), owned by a person;
//!   turning alerts on in a browser also adds `push` to that person's alert
//!   channels. The endpoint must be https; plain http only to a push service
//!   on this machine, asked for from this machine (development and tests).
//! - **Sending.** A message on channel `push` is addressed to a subscription
//!   id. The payload (JSON: title, body, tag, url) is encrypted for that
//!   browser as one aes128gcm record (RFC 8291 over RFC 8188) and POSTed to
//!   the endpoint with a VAPID JWT (`Authorization: vapid t=…, k=…`), a TTL,
//!   an urgency and, for alerts, a topic (a newer message for the same alert
//!   replaces one the phone hasn't fetched yet). 404 and 410 mean the
//!   browser dropped the subscription: the row goes and the message fails.
//! - The text loses its texting instructions ("Reply OK to ack"): a
//!   notification opens the app, where the alert or the decision is answered.

use std::sync::OnceLock;

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD as B64};
use chrono::{DateTime, Utc};
use hmac::{Hmac, Mac};
use op_core::alert::MessageLog;
use op_core::time::{from_db, now, to_db};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Identity, Role, Via, id};
use ring::rand::{SecureRandom, SystemRandom};
use ring::signature::{ECDSA_P256_SHA256_FIXED_SIGNING, EcdsaKeyPair, KeyPair};
use ring::{aead, agreement};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use sqlx::Row as _;
use sqlx::sqlite::SqliteRow;

use super::{Channel, ChannelError, Delivery, Sent, http, net_error, record_sent};

/// Id prefix of a subscription.
pub const SUBSCRIPTION: &str = "psh";
/// The setting holding the public key and whether push is on.
pub const SETTING: &str = "push";
/// The secret holding the VAPID private key.
pub const SECRET: &str = "vapid_private_key";
/// Record size of the one aes128gcm record (RFC 8188 §2).
pub const RS: u32 = 4096;
/// The encryption header: salt (16), rs (4), idlen (1), the server's key (65).
const HEADER_LEN: usize = 16 + 4 + 1 + 65;
/// Largest payload: a push service takes 4096 bytes of body, less the
/// header, the 16-byte tag and the record delimiter.
pub const MAX_PAYLOAD: usize = RS as usize - HEADER_LEN - 16 - 1;
/// A VAPID JWT is good for this long (RFC 8292 allows at most 24 h).
pub const JWT_S: i64 = 12 * 3600;
/// Subscriptions one person may hold; the oldest go past this.
pub const PER_PERSON: i64 = 10;

// ---- settings and keys ------------------------------------------------------------------

/// `push`: the public half of the farm's VAPID key and whether push is on.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PushSettings {
    /// Uncompressed P-256 point, base64url (the app's `applicationServerKey`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vapid_public_key: Option<String>,
    /// Off: nothing goes out by push (subscriptions stay).
    pub enabled: bool,
}

impl Default for PushSettings {
    fn default() -> Self {
        Self { vapid_public_key: None, enabled: true }
    }
}

pub async fn settings(ctx: &Ctx) -> anyhow::Result<PushSettings> {
    Ok(ctx.store().get_setting::<PushSettings>(SETTING).await?.unwrap_or_default())
}

/// Reached over https: the base URL (settings or a tunnel) is https.
pub fn over_https(ctx: &Ctx) -> bool {
    ctx.base_url().starts_with("https://")
}

/// The farm's VAPID key pair.
pub struct Vapid {
    key: EcdsaKeyPair,
    /// The public key, base64url.
    pub public: String,
}

fn rng() -> &'static SystemRandom {
    static RNG: OnceLock<SystemRandom> = OnceLock::new();
    RNG.get_or_init(SystemRandom::new)
}

fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim();
    [&B64, &URL_SAFE, &STANDARD, &STANDARD_NO_PAD].iter().find_map(|e| e.decode(s).ok())
}

impl Vapid {
    fn from_pkcs8(pkcs8: &[u8]) -> anyhow::Result<Self> {
        let key = EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, pkcs8, rng()).map_err(|e| anyhow::anyhow!("VAPID key unreadable: {e}"))?;
        let public = B64.encode(key.public_key().as_ref());
        Ok(Self { key, public })
    }

    /// The stored key, or a new one when `make` and there is none. The
    /// setting's public key follows the secret.
    pub async fn load(ctx: &Ctx, make: bool) -> anyhow::Result<Option<Self>> {
        let v = match ctx.secrets().get(SECRET)?.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()) {
            Some(s) => Self::from_pkcs8(&decode(&s).ok_or_else(|| anyhow::anyhow!("{SECRET} isn't base64"))?)?,
            None if make => {
                let doc = EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_FIXED_SIGNING, rng()).map_err(|e| anyhow::anyhow!("making a VAPID key: {e}"))?;
                ctx.secrets().set(SECRET, &B64.encode(doc.as_ref()))?;
                tracing::info!("made the farm's Web Push key");
                Self::from_pkcs8(doc.as_ref())?
            }
            None => return Ok(None),
        };
        let mut s = settings(ctx).await?;
        if s.vapid_public_key.as_deref() != Some(v.public.as_str()) {
            s.vapid_public_key = Some(v.public.clone());
            ctx.store().set_setting(SETTING, &s).await?;
        }
        Ok(Some(v))
    }

    /// A signed VAPID JWT (ES256) for a push service at `aud` (its origin).
    pub fn jwt(&self, aud: &str, sub: &str, exp: i64) -> anyhow::Result<String> {
        let head = B64.encode(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = B64.encode(serde_json::to_vec(&json!({ "aud": aud, "exp": exp, "sub": sub }))?);
        let input = format!("{head}.{claims}");
        let sig = self.key.sign(rng(), input.as_bytes()).map_err(|e| anyhow::anyhow!("signing: {e}"))?;
        Ok(format!("{input}.{}", B64.encode(sig.as_ref())))
    }

    /// The `Authorization` header value (RFC 8292 §3).
    pub fn authorization(&self, aud: &str, sub: &str, exp: i64) -> anyhow::Result<String> {
        Ok(format!("vapid t={}, k={}", self.jwt(aud, sub, exp)?, self.public))
    }
}

// ---- message encryption (RFC 8291) --------------------------------------------------------

fn hmac256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        mac.update(p);
    }
    let mut out = [0u8; 32];
    out.copy_from_slice(&mac.finalize().into_bytes());
    out
}

/// The encrypted body for one browser, given the ECDH secret between our
/// one-time key (`as_public`) and its `p256dh`, its `auth` secret and a salt:
/// the aes128gcm header, then `plaintext` with the last-record delimiter,
/// sealed with AES-128-GCM (RFC 8291 §3.3–3.4, RFC 8188 §2).
pub fn seal(plaintext: &[u8], ecdh_secret: &[u8], ua_public: &[u8], as_public: &[u8], auth: &[u8], salt: &[u8; 16]) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(plaintext.len() <= MAX_PAYLOAD, "too long for a push message");
    anyhow::ensure!(as_public.len() == 65, "our key must be an uncompressed P-256 point");
    let prk_key = hmac256(auth, &[ecdh_secret]);
    let ikm = hmac256(&prk_key, &[b"WebPush: info\0", ua_public, as_public, &[1]]);
    let prk = hmac256(salt, &[&ikm]);
    let cek = hmac256(&prk, &[b"Content-Encoding: aes128gcm\0", &[1]]);
    let nonce = hmac256(&prk, &[b"Content-Encoding: nonce\0", &[1]]);
    let key = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek[..16]).map_err(|_| anyhow::anyhow!("AES key"))?);
    let nonce = aead::Nonce::try_assume_unique_for_key(&nonce[..12]).map_err(|_| anyhow::anyhow!("nonce"))?;
    let mut record = Vec::with_capacity(plaintext.len() + 17);
    record.extend_from_slice(plaintext);
    record.push(2);
    key.seal_in_place_append_tag(nonce, aead::Aad::empty(), &mut record).map_err(|_| anyhow::anyhow!("sealing"))?;
    let mut out = Vec::with_capacity(HEADER_LEN + record.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&RS.to_be_bytes());
    out.push(65);
    out.extend_from_slice(as_public);
    out.extend_from_slice(&record);
    Ok(out)
}

/// Encrypt `plaintext` for a browser's `p256dh` and `auth` with a fresh
/// one-time key and salt.
pub fn encrypt(plaintext: &[u8], ua_public: &[u8], auth: &[u8]) -> anyhow::Result<Vec<u8>> {
    let mine = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, rng()).map_err(|_| anyhow::anyhow!("making a key"))?;
    let as_public = mine.compute_public_key().map_err(|_| anyhow::anyhow!("our public key"))?;
    let mut salt = [0u8; 16];
    rng().fill(&mut salt).map_err(|_| anyhow::anyhow!("random salt"))?;
    let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, ua_public);
    let secret = agreement::agree_ephemeral(mine, &peer, |s| s.to_vec()).map_err(|_| anyhow::anyhow!("the browser's key isn't a P-256 point"))?;
    seal(plaintext, &secret, ua_public, as_public.as_ref(), auth, &salt)
}

/// A browser's keys, decoded and checked: `p256dh` a P-256 point, `auth` 16 bytes.
pub fn check_keys(p256dh: &str, auth: &str) -> Result<(Vec<u8>, Vec<u8>), String> {
    let p = decode(p256dh).ok_or("keys.p256dh isn't base64url.")?;
    let a = decode(auth).ok_or("keys.auth isn't base64url.")?;
    if a.len() != 16 {
        return Err("keys.auth must be 16 bytes.".into());
    }
    if p.len() != 65 || p[0] != 4 {
        return Err("keys.p256dh must be an uncompressed P-256 key.".into());
    }
    // A point off the curve fails the key agreement; find out now, not at the first alert.
    encrypt(b"", &p, &a).map_err(|_| "keys.p256dh isn't a P-256 key.".to_owned())?;
    Ok((p, a))
}

// ---- subscriptions -------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Subscription {
    pub id: String,
    pub user_id: String,
    pub endpoint: String,
    #[serde(skip)]
    pub p256dh: String,
    #[serde(skip)]
    pub auth: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ok: Option<DateTime<Utc>>,
}

fn sub_from(r: &SqliteRow) -> anyhow::Result<Subscription> {
    Ok(Subscription {
        id: r.try_get("id")?,
        user_id: r.try_get("user_id")?,
        endpoint: r.try_get("endpoint")?,
        p256dh: r.try_get("p256dh")?,
        auth: r.try_get("auth")?,
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
        last_ok: r.try_get::<Option<String>, _>("last_ok")?.map(|t| from_db(&t)).transpose()?,
    })
}

pub async fn get_subscription(ctx: &Ctx, id: &str) -> anyhow::Result<Option<Subscription>> {
    sqlx::query("SELECT * FROM push_subscriptions WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?.as_ref().map(sub_from).transpose()
}

/// A person's subscriptions, oldest first.
pub async fn subscriptions_of(ctx: &Ctx, user_id: &str) -> anyhow::Result<Vec<Subscription>> {
    sqlx::query("SELECT * FROM push_subscriptions WHERE user_id = ? ORDER BY created_at, id")
        .bind(user_id)
        .fetch_all(ctx.db())
        .await?
        .iter()
        .map(sub_from)
        .collect()
}

/// Where a push service is, for a VAPID `aud`: scheme, host and port.
pub fn origin(endpoint: &str) -> Option<String> {
    let u = reqwest::Url::parse(endpoint).ok()?;
    u.host_str()?;
    Some(u.origin().ascii_serialization())
}

fn loopback(u: &reqwest::Url) -> bool {
    u.host_str().is_some_and(|h| {
        h.eq_ignore_ascii_case("localhost") || h.trim_start_matches('[').trim_end_matches(']').parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
    })
}

/// Whether `endpoint` may be a push service for a request made as `id`:
/// https anywhere; plain http only on this machine and only when the request
/// came from this machine (whoever makes it already has every right here, so
/// the server's own requests to it give nothing away).
pub fn check_endpoint(endpoint: &str, id: &Identity) -> Result<(), &'static str> {
    if endpoint.len() > 2048 {
        return Err("That endpoint is too long.");
    }
    let Ok(u) = reqwest::Url::parse(endpoint) else { return Err("endpoint must be a URL.") };
    if u.host_str().is_none() || !u.username().is_empty() || u.password().is_some() {
        return Err("endpoint must be a push service URL.");
    }
    match u.scheme() {
        "https" => Ok(()),
        "http" if loopback(&u) && id.via == Via::Local => Ok(()),
        _ => Err("Push services are https."),
    }
}

/// Store a browser's subscription for a person (the same endpoint again
/// updates its keys and owner), keep their newest [`PER_PERSON`], and add
/// `push` to their alert channels.
pub async fn subscribe(ctx: &Ctx, user_id: &str, endpoint: &str, p256dh: &str, auth: &str) -> anyhow::Result<Subscription> {
    let t = to_db(&now());
    let row = sqlx::query(
        "INSERT INTO push_subscriptions (id, user_id, endpoint, p256dh, auth, created_at) VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(endpoint) DO UPDATE SET user_id = excluded.user_id, p256dh = excluded.p256dh, auth = excluded.auth
         RETURNING *",
    )
    .bind(id::new_id(SUBSCRIPTION))
    .bind(user_id)
    .bind(endpoint)
    .bind(p256dh)
    .bind(auth)
    .bind(&t)
    .fetch_one(ctx.db())
    .await?;
    let sub = sub_from(&row)?;
    sqlx::query(
        "DELETE FROM push_subscriptions WHERE user_id = ?1 AND id NOT IN
           (SELECT id FROM push_subscriptions WHERE user_id = ?1 ORDER BY created_at DESC, id DESC LIMIT ?2)",
    )
    .bind(user_id)
    .bind(PER_PERSON)
    .execute(ctx.db())
    .await?;
    let (prefs, _) = crate::routing::prefs::get(ctx, user_id).await?;
    if !prefs.channels.iter().any(|c| c == "push") {
        let mut channels = prefs.channels.clone();
        channels.push("push".into());
        crate::routing::prefs::put(ctx, user_id, &json!({ "channels": channels })).await.map_err(|e| anyhow::anyhow!("{}", e.message))?;
    }
    Ok(sub)
}

// ---- the channel -----------------------------------------------------------------------------

/// The notification a message becomes: JSON the service worker shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    pub title: String,
    pub body: String,
    /// One notification per alert: a new one for the same alert replaces it.
    pub tag: String,
    /// Where tapping it opens the app.
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
}

/// A text's words for a notification: the texting instructions go, since a
/// notification is answered in the app ("… 6m. Reply OK to ack" → "… 6m";
/// "Reply Y or N. Code 4821" and "Reply Y to keep, N to hold. Code 4821" at the
/// end; a brief's "Reply Y or N." line; "unless you reply N" → the time alone).
pub fn notice_text(text: &str) -> String {
    let code = |s: &str| s.len() == 4 && s.bytes().all(|b| b.is_ascii_digit());
    let lines: Vec<String> = text
        .lines()
        .filter(|l| l.trim() != "Reply Y or N.")
        .map(|l| {
            let l = l.trim_end();
            if let Some(i) = l.rfind("Reply ") {
                let tail = &l[i..];
                let known = tail == "Reply OK to ack"
                    || tail.strip_prefix("Reply Y or N. Code ").is_some_and(code)
                    || tail.strip_prefix("Reply Y to keep, N to hold. Code ").is_some_and(code);
                if known {
                    return l[..i].trim_end().trim_end_matches(['.', ',']).trim_end().to_owned();
                }
            }
            match (l.strip_prefix("Sends "), l.ends_with(" unless you reply N.")) {
                (Some(rest), true) => format!("Sends {}.", rest.trim_end_matches(" unless you reply N.")),
                _ => l.to_owned(),
            }
        })
        .collect();
    lines.join("\n")
}

/// The notification for a message: the farm's name over the text; an alert
/// opens the map on its herd.
pub async fn notice(ctx: &Ctx, msg: &MessageLog) -> anyhow::Result<Notice> {
    let farm = ctx.store().get_farm().await?.map(|f| f.name).unwrap_or_else(|| "openpasture".into());
    let herd_id: Option<String> = match &msg.alert_id {
        Some(a) => sqlx::query_scalar("SELECT herd_id FROM alerts WHERE id = ?").bind(a).fetch_optional(ctx.db()).await?.flatten(),
        None => None,
    };
    let url = match (&msg.alert_id, &herd_id) {
        (Some(a), _) => format!("/#/map?alert={a}"),
        _ => "/#/map".to_owned(),
    };
    Ok(Notice {
        title: farm,
        body: notice_text(&msg.text),
        tag: msg.alert_id.clone().unwrap_or_else(|| msg.id.clone()),
        url,
        alert_id: msg.alert_id.clone(),
        herd_id,
    })
}

/// How long a push service keeps a message for a phone that is off: an
/// alert matters for hours, the brief for the morning, a test briefly.
pub fn ttl(kind: &str) -> u32 {
    match kind {
        "alert" => 4 * 3600,
        "brief" => 12 * 3600,
        _ => 3600,
    }
}

pub struct Push {
    ctx: Ctx,
    vapid: Vapid,
    /// The VAPID `sub`: the farm's https address.
    contact: String,
}

impl Push {
    /// The channel, when push can send (https, on, a key).
    pub async fn load(ctx: &Ctx) -> anyhow::Result<Option<Self>> {
        if !over_https(ctx) || !settings(ctx).await?.enabled {
            return Ok(None);
        }
        let Some(vapid) = Vapid::load(ctx, false).await? else { return Ok(None) };
        Ok(Some(Self { ctx: ctx.clone(), vapid, contact: ctx.base_url() }))
    }

    /// Send `notice` to one subscription now.
    pub async fn deliver(&self, sub: &Subscription, notice: &Notice, kind: &str) -> Result<Delivery, ChannelError> {
        let gone = || ChannelError::Fail("That phone stopped taking notifications.".into());
        let payload = serde_json::to_vec(notice).map_err(|e| ChannelError::Fail(e.to_string()))?;
        if payload.len() > MAX_PAYLOAD {
            return Err(ChannelError::Fail("Too long for a notification.".into()));
        }
        let (ua, auth) = check_keys(&sub.p256dh, &sub.auth).map_err(ChannelError::Fail)?;
        let body = encrypt(&payload, &ua, &auth).map_err(|e| ChannelError::Fail(e.to_string()))?;
        let aud = origin(&sub.endpoint).ok_or_else(gone)?;
        let auth_header = self
            .vapid
            .authorization(&aud, &self.contact, (now() + chrono::Duration::seconds(JWT_S)).timestamp())
            .map_err(|e| ChannelError::Fail(e.to_string()))?;
        let mut req = http()
            .post(&sub.endpoint)
            .header("authorization", auth_header)
            .header("content-encoding", "aes128gcm")
            .header("content-type", "application/octet-stream")
            .header("ttl", ttl(kind).to_string())
            .header("urgency", if kind == "alert" { "high" } else { "normal" });
        // A topic is at most 32 base64url characters (RFC 8030 §5.4); alert ids are.
        if let Some(a) = notice.alert_id.as_deref().filter(|a| a.len() <= 32 && a.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')) {
            req = req.header("topic", a);
        }
        let res = req.body(body).send().await.map_err(|e| net_error("The push service", &e))?;
        let status = res.status().as_u16();
        let location = res.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_owned);
        match status {
            200..=299 => {
                let _ = sqlx::query("UPDATE push_subscriptions SET last_ok = ? WHERE id = ?").bind(to_db(&now())).bind(&sub.id).execute(self.ctx.db()).await;
                Ok(Delivery::sent(location))
            }
            404 | 410 => {
                // The browser unsubscribed or the subscription expired.
                let _ = sqlx::query("DELETE FROM push_subscriptions WHERE id = ?").bind(&sub.id).execute(self.ctx.db()).await;
                Err(gone())
            }
            413 => Err(ChannelError::Fail("Too long for a notification.".into())),
            408 | 429 | 500..=599 => Err(ChannelError::Retry(format!("The push service said {status}."))),
            _ => Err(ChannelError::Fail(format!("The push service refused it ({status})."))),
        }
    }
}

#[async_trait::async_trait]
impl Channel for Push {
    fn kind(&self) -> &'static str {
        "push"
    }

    async fn send(&self, msg: &MessageLog) -> Result<Delivery, ChannelError> {
        let sub = get_subscription(&self.ctx, &msg.address)
            .await
            .map_err(|e| ChannelError::Retry(e.to_string()))?
            .ok_or_else(|| ChannelError::Fail("That phone stopped taking notifications.".into()))?;
        let notice = notice(&self.ctx, msg).await.map_err(|e| ChannelError::Retry(e.to_string()))?;
        self.deliver(&sub, &notice, &msg.kind).await
    }
}

// ---- API ---------------------------------------------------------------------------------------

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/push", get(get_push))
        .route("/api/push/settings", put(put_settings))
        .route("/api/push/subscriptions", post(post_subscription))
        .route("/api/push/subscriptions/{id}", delete(delete_subscription))
        .route("/api/push/subscriptions/{id}/test", post(test_subscription))
}

/// One of your own subscriptions (the endpoint lets a browser find its own).
#[derive(Debug, Serialize)]
pub struct Mine {
    pub id: String,
    pub endpoint: String,
    pub created_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_ok: Option<DateTime<Utc>>,
}

/// `GET /api/push`: whether push can work here, the key to subscribe with,
/// and your own subscriptions.
#[derive(Debug, Serialize)]
pub struct PushView {
    /// Served over https and switched on: browsers can subscribe.
    pub available: bool,
    pub enabled: bool,
    /// Why not, when not available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vapid_public_key: Option<String>,
    /// Whether `vapid_private_key` is set (never its value).
    pub key_set: bool,
    pub mine: Vec<Mine>,
}

async fn view(ctx: &Ctx, id: &Identity) -> ApiResult<PushView> {
    let s = settings(ctx).await?;
    let https = over_https(ctx);
    let available = https && s.enabled;
    // The key is made the first time a browser could use it.
    let vapid = Vapid::load(ctx, available).await?;
    let mine = match &id.user_id {
        Some(u) => {
            subscriptions_of(ctx, u).await?.into_iter().map(|s| Mine { id: s.id, endpoint: s.endpoint, created_at: s.created_at, last_ok: s.last_ok }).collect()
        }
        None => vec![],
    };
    Ok(PushView {
        available,
        enabled: s.enabled,
        reason: if !https {
            Some("Push needs the server's https address (Settings > Server).")
        } else if !s.enabled {
            Some("Push is off for this farm.")
        } else {
            None
        },
        vapid_public_key: vapid.as_ref().filter(|_| available).map(|v| v.public.clone()),
        key_set: vapid.is_some(),
        mine,
    })
}

async fn get_push(State(ctx): State<Ctx>, id: Identity) -> ApiResult<Json<PushView>> {
    Ok(Json(view(&ctx, &id).await?))
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SettingsBody {
    #[serde(default)]
    pub enabled: Option<bool>,
    /// A new key pair: every browser has to turn alerts on again.
    #[serde(default)]
    pub new_keys: bool,
}

/// `PUT /api/push/settings` (owner): push on or off for the farm, or new keys.
async fn put_settings(State(ctx): State<Ctx>, id: Identity, ApiJson(b): ApiJson<SettingsBody>) -> ApiResult<Json<PushView>> {
    id.require(Role::Owner)?;
    let mut s = settings(&ctx).await?;
    if let Some(on) = b.enabled {
        s.enabled = on;
    }
    if b.new_keys {
        ctx.secrets().delete(SECRET)?;
        s.vapid_public_key = None;
        // Subscriptions were made for the old key; push services refuse them now.
        sqlx::query("DELETE FROM push_subscriptions").execute(ctx.db()).await.map_err(anyhow::Error::from)?;
    }
    ctx.store().set_setting(SETTING, &s).await?;
    Ok(Json(view(&ctx, &id).await?))
}

#[derive(Debug, Deserialize)]
pub struct Keys {
    pub p256dh: String,
    pub auth: String,
}

/// A browser's `PushSubscription.toJSON()`.
#[derive(Debug, Deserialize)]
pub struct SubscriptionBody {
    pub endpoint: String,
    pub keys: Keys,
    #[serde(default, rename = "expirationTime")]
    pub _expiration_time: Option<Value>,
}

/// `POST /api/push/subscriptions`: this browser takes alerts for you.
async fn post_subscription(State(ctx): State<Ctx>, id: Identity, ApiJson(b): ApiJson<SubscriptionBody>) -> ApiResult<(StatusCode, Json<Mine>)> {
    let Some(user_id) = id.user_id.clone() else {
        return Err(ApiError::conflict("Add yourself in Settings > People first, then turn alerts on here."));
    };
    if !over_https(&ctx) {
        return Err(ApiError::conflict("Push needs the server's https address (Settings > Server)."));
    }
    let endpoint = b.endpoint.trim();
    check_endpoint(endpoint, &id).map_err(ApiError::bad_request)?;
    check_keys(&b.keys.p256dh, &b.keys.auth).map_err(ApiError::bad_request)?;
    // The browser subscribed with the farm's key; make sure there is one to sign with.
    Vapid::load(&ctx, true).await?;
    let s = subscribe(&ctx, &user_id, endpoint, b.keys.p256dh.trim(), b.keys.auth.trim()).await?;
    Ok((StatusCode::CREATED, Json(Mine { id: s.id, endpoint: s.endpoint, created_at: s.created_at, last_ok: s.last_ok })))
}

/// A subscription the caller may act on: their own, or anyone's for the owner.
async fn own(ctx: &Ctx, id: &Identity, sub_id: &str) -> ApiResult<Subscription> {
    match get_subscription(ctx, sub_id).await? {
        Some(s) if id.can(Role::Owner) || id.user_id.as_deref() == Some(s.user_id.as_str()) => Ok(s),
        _ => Err(ApiError::not_found("No such subscription.")),
    }
}

/// `DELETE /api/push/subscriptions/{id}`: this browser stops taking alerts.
async fn delete_subscription(State(ctx): State<Ctx>, id: Identity, Path(sub_id): Path<String>) -> ApiResult<StatusCode> {
    let s = own(&ctx, &id, &sub_id).await?;
    sqlx::query("DELETE FROM push_subscriptions WHERE id = ?").bind(&s.id).execute(ctx.db()).await.map_err(anyhow::Error::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub detail: String,
}

/// The test notification's words.
pub const TEST_TEXT: &str = "openpasture test.";

/// `POST /api/push/subscriptions/{id}/test`: a test notification to it now.
async fn test_subscription(State(ctx): State<Ctx>, id: Identity, Path(sub_id): Path<String>) -> ApiResult<Json<TestResult>> {
    let s = own(&ctx, &id, &sub_id).await?;
    let Some(ch) = Push::load(&ctx).await? else {
        return Ok(Json(TestResult { ok: false, detail: view(&ctx, &id).await?.reason.unwrap_or("Push isn't set up.").into() }));
    };
    let msg = super::draft("push", &s.id, "test", TEST_TEXT, None);
    let notice = notice(&ctx, &msg).await?;
    let res = ch.deliver(&s, &notice, "test").await;
    let (status, provider_id, error) = match &res {
        Ok(d) => (d.status.clone(), d.provider_id.clone(), None),
        Err(e) => ("failed".to_owned(), None, Some(e.message().to_owned())),
    };
    record_sent(
        &ctx,
        Sent {
            channel: "push".into(),
            to: s.id.clone(),
            text: TEST_TEXT.into(),
            kind: "test".into(),
            user_id: Some(s.user_id.clone()),
            status,
            provider_id,
            error,
            ..Default::default()
        },
    )
    .await?;
    Ok(Json(match res {
        Ok(_) => TestResult { ok: true, detail: "Sent".into() },
        Err(e) => TestResult { ok: false, detail: e.message().into() },
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 8291 Appendix A.
    #[test]
    fn sealing_matches_the_rfc_8291_example() {
        let b = |s: &str| B64.decode(s).unwrap();
        let body = seal(
            b"When I grow up, I want to be a watermelon",
            &b("kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs"),
            &b("BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4"),
            &b("BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8"),
            &b("BTBZMqHH6r4Tts7J_aSIgg"),
            &b("DGv6ra1nlYgDCS1FRnbzlw").try_into().unwrap(),
        )
        .unwrap();
        assert_eq!(
            B64.encode(body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    #[test]
    fn a_payload_that_fits_makes_a_body_a_push_service_takes() {
        let ua = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, rng()).unwrap();
        let ua_public = ua.compute_public_key().unwrap();
        let body = encrypt(&vec![b'x'; MAX_PAYLOAD], ua_public.as_ref(), &[7u8; 16]).unwrap();
        assert_eq!(body.len(), 4096);
        assert!(encrypt(&vec![b'x'; MAX_PAYLOAD + 1], ua_public.as_ref(), &[7u8; 16]).is_err());
    }

    #[test]
    fn texting_instructions_leave_the_notification() {
        assert_eq!(notice_text("214 outside P3, 200 ft N of east gate, 6m. Reply OK to ack"), "214 outside P3, 200 ft N of east gate, 6m");
        assert_eq!(notice_text("31 outside P3 since 06:12. Reply OK to ack"), "31 outside P3 since 06:12");
        assert_eq!(notice_text("Cows: move to P4 (30.6 ac, 3 d)? Reply Y or N. Code 4821"), "Cows: move to P4 (30.6 ac, 3 d)?");
        assert_eq!(notice_text("Cows: strip 4 of 12 opens 07:00. Reply Y to keep, N to hold. Code 0042"), "Cows: strip 4 of 12 opens 07:00");
        assert_eq!(notice_text("Cows: MOVE to P4 (30.6 ac).\nReply Y or N.\nBattery low: 031 14%."), "Cows: MOVE to P4 (30.6 ac).\nBattery low: 031 14%.");
        assert_eq!(notice_text("Cows: MOVE to P4 (30.6 ac).\nSends 07:40 unless you reply N."), "Cows: MOVE to P4 (30.6 ac).\nSends 07:40.");
        // Anything else stays as written, a name with "Reply " in it too.
        assert_eq!(notice_text("Reply Creek: 3 silent 25m. Check coverage or the server"), "Reply Creek: 3 silent 25m. Check coverage or the server");
        assert_eq!(
            notice_text("Cows: 180 of 250 collars silent 25m. Check coverage or the server"),
            "Cows: 180 of 250 collars silent 25m. Check coverage or the server"
        );
    }

    #[test]
    fn endpoints_are_https_or_this_machine_asked_from_this_machine() {
        let local = Identity::owner(Via::Local);
        let remote = Identity { role: Role::Viewer, user_id: Some("usr_1".into()), name: None, via: Via::UserToken };
        let owner_token = Identity::owner(Via::AppToken);
        assert!(check_endpoint("https://fcm.googleapis.com/fcm/send/abc", &remote).is_ok());
        assert!(check_endpoint("https://web.push.apple.com/QGuQyavXutnMH", &remote).is_ok());
        assert!(check_endpoint("http://127.0.0.1:9/push/1", &local).is_ok());
        assert!(check_endpoint("http://localhost:9/push/1", &local).is_ok());
        assert!(check_endpoint("http://[::1]:9/push/1", &local).is_ok());
        // The server's own port is local, and local requests are the owner: never for anyone else.
        assert!(check_endpoint("http://127.0.0.1:7878/api/herds/h/move/stop", &remote).is_err());
        assert!(check_endpoint("http://127.0.0.1:7878/api/herds/h/move/stop", &owner_token).is_err());
        assert!(check_endpoint("http://push.example.com/1", &local).is_err());
        assert!(check_endpoint("ftp://push.example.com/1", &local).is_err());
        assert!(check_endpoint("https://user:pw@push.example.com/1", &remote).is_err());
        assert!(check_endpoint("not a url", &remote).is_err());
        assert_eq!(origin("https://fcm.googleapis.com/fcm/send/abc").as_deref(), Some("https://fcm.googleapis.com"));
        assert_eq!(origin("http://127.0.0.1:9123/p/1").as_deref(), Some("http://127.0.0.1:9123"));
    }

    #[test]
    fn browser_keys_are_checked_when_they_arrive() {
        let ua = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, rng()).unwrap();
        let p = B64.encode(ua.compute_public_key().unwrap().as_ref());
        let a = B64.encode([1u8; 16]);
        assert!(check_keys(&p, &a).is_ok());
        assert!(check_keys(&p, &B64.encode([1u8; 12])).is_err(), "auth is 16 bytes");
        let mut off = B64.decode(&p).unwrap();
        off[40] ^= 0xff;
        assert!(check_keys(&B64.encode(&off), &a).is_err(), "not on the curve");
        assert!(check_keys("!!", &a).is_err());
    }
}
