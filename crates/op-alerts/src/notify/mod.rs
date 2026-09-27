//! Delivery: the channels a farm brings (Twilio SMS and WhatsApp, SMTP email,
//! a signed webhook, the hosted relay), the sender that drains op-core's
//! `messages` outbox through them, Twilio delivery-status polling, phone
//! verification, and Settings > Texting's API.
//!
//! The alert engine only ever enqueues (`op_core::messages::enqueue`); this
//! module only ever delivers. Channel config lives in the `notify.channels`
//! setting ([`ChannelsConfig`]) and the secrets `twilio_account_sid`,
//! `twilio_auth_token`, `smtp_password`, `webhook_secret`, `hosted_url`,
//! `hosted_api_key`. Whether a channel can send is decided in one place,
//! `op_core::notify_config::configured_channels`, and [`channel`] follows it.

pub mod api;
pub mod email;
pub mod relay;
pub mod sender;
pub mod twilio;
pub mod verify;
pub mod webhook;
// @A3
// @M

use std::fmt;
use std::sync::OnceLock;
use std::time::Duration;

use chrono::{DateTime, Utc};
use op_core::alert::MessageLog;
use op_core::messages::{MESSAGE, message_from_row};
use op_core::notify_config::{CHANNELS_KEY, configured_channels};
use op_core::time::{now, to_db};
use op_core::{Ctx, Event, id};
use serde::{Deserialize, Serialize};

/// A way to reach a person (or a system). Each send is one attempt; the
/// sender decides about retries from the error.
#[async_trait::async_trait]
pub trait Channel: Send + Sync {
    /// `sms` | `whatsapp` | `email` | `webhook` | `relay` (M adds `push`).
    fn kind(&self) -> &'static str;
    async fn send(&self, msg: &MessageLog) -> Result<Delivery, ChannelError>;
}

/// What a provider said about a message it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delivery {
    /// Twilio's message SID, the relay's message id, …
    pub provider_id: Option<String>,
    /// `sent` (handed on; Twilio texts are then polled) or `delivered`.
    pub status: String,
}

impl Delivery {
    pub fn sent(provider_id: Option<String>) -> Self {
        Self { provider_id, status: "sent".into() }
    }
    pub fn delivered(provider_id: Option<String>) -> Self {
        Self { provider_id, status: "delivered".into() }
    }
}

/// Why a send didn't work, in words a farmer can act on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ChannelError {
    /// Worth another try later: a rate limit, a server error, a timeout.
    Retry(String),
    /// The provider couldn't be reached at all (no network, no DNS, the
    /// connection refused), so it never saw the message: it waits for the
    /// network to come back.
    Offline(String),
    /// Won't work however often it is tried: a bad number, wrong credentials.
    Fail(String),
}

impl ChannelError {
    pub fn message(&self) -> &str {
        match self {
            Self::Retry(m) | Self::Offline(m) | Self::Fail(m) => m,
        }
    }
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for ChannelError {}

/// The channels this module delivers, in the order the sender visits them.
pub const CHANNELS: [&str; 5] = ["sms", "whatsapp", "email", "webhook", "relay"];

/// Secrets the channels read. Settings reports `set` for each, never a value.
pub const SECRETS: [&str; 6] = ["twilio_account_sid", "twilio_auth_token", "smtp_password", "webhook_secret", "hosted_url", "hosted_api_key"];

/// Name of a channel in a sentence: "SMS isn't set up."
pub fn label(kind: &str) -> &'static str {
    match kind {
        "sms" => "SMS",
        "whatsapp" => "WhatsApp",
        "email" => "Email",
        "webhook" => "The webhook",
        "relay" => "The relay",
        "push" => "Push",
        _ => "That channel",
    }
}

// ---- config ---------------------------------------------------------------------

/// `notify.channels`. Empty strings and missing keys both mean "not set".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ChannelsConfig {
    pub sms: SmsConfig,
    pub whatsapp: WhatsappConfig,
    pub email: EmailConfig,
    pub webhook: WebhookConfig,
    pub relay: RelayConfig,
    /// Where Twilio's REST API is. Changed only to point at a Twilio-shaped
    /// test server.
    pub twilio_api_base: String,
}

impl Default for ChannelsConfig {
    fn default() -> Self {
        Self {
            sms: SmsConfig::default(),
            whatsapp: WhatsappConfig::default(),
            email: EmailConfig::default(),
            webhook: WebhookConfig::default(),
            relay: RelayConfig::default(),
            twilio_api_base: twilio::DEFAULT_API_BASE.into(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SmsConfig {
    /// The farm's Twilio number (E.164) or a Messaging Service SID (`MG…`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WhatsappConfig {
    /// The farm's WhatsApp sender number (E.164).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    /// An approved template (`HX…`) with one `{{1}}` body variable, used for
    /// business-initiated messages (alerts); replies go as plain text.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub template_sid: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tls {
    /// Plain connection upgraded with STARTTLS (port 587).
    #[default]
    Starttls,
    /// TLS from the first byte (port 465).
    Tls,
    /// No encryption: a mail server on the farm's own network.
    None,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EmailConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub port: u16,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    pub tls: Tls,
}

impl Default for EmailConfig {
    fn default() -> Self {
        Self { host: None, port: 587, user: None, from: None, tls: Tls::Starttls }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct WebhookConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RelayConfig {
    /// Set only after the relay answered `GET /v1/notify/recipients` with 200
    /// for this server's key.
    pub enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<DateTime<Utc>>,
}

/// A trimmed, non-empty string.
pub(crate) fn set(v: &Option<String>) -> Option<&str> {
    v.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

/// The stored channel config; defaults when there is none or it can't be read.
pub async fn load(ctx: &Ctx) -> anyhow::Result<ChannelsConfig> {
    let Some(v) = ctx.store().get_setting_json(CHANNELS_KEY).await? else { return Ok(ChannelsConfig::default()) };
    Ok(serde_json::from_value(v).unwrap_or_else(|e| {
        tracing::warn!("notify.channels is unreadable ({e}); using defaults");
        ChannelsConfig::default()
    }))
}

pub async fn save(ctx: &Ctx, cfg: &ChannelsConfig) -> anyhow::Result<()> {
    ctx.store().set_setting(CHANNELS_KEY, cfg).await
}

pub(crate) fn secret(ctx: &Ctx, name: &str) -> anyhow::Result<Option<String>> {
    Ok(ctx.secrets().get(name)?.map(|v| v.trim().to_owned()).filter(|v| !v.is_empty()))
}

/// The channel `kind` as it can send right now, or `None` when it isn't set
/// up (exactly when `configured_channels` leaves it out).
pub async fn channel(ctx: &Ctx, kind: &str) -> anyhow::Result<Option<Box<dyn Channel>>> {
    if !configured_channels(ctx).await?.contains(&kind) {
        return Ok(None);
    }
    let cfg = load(ctx).await?;
    Ok(match kind {
        "sms" | "whatsapp" => twilio::Twilio::from_config(ctx, &cfg, kind)?.map(|c| Box::new(c) as Box<dyn Channel>),
        "email" => email::Email::from_config(ctx, &cfg)?.map(|c| Box::new(c) as Box<dyn Channel>),
        "webhook" => webhook::Webhook::from_config(ctx, &cfg)?.map(|c| Box::new(c) as Box<dyn Channel>),
        "relay" => relay::Relay::from_secrets(ctx)?.map(|c| Box::new(c) as Box<dyn Channel>),
        // @A3
        // @M
        _ => None,
    })
}

/// Channels an alert or a brief may go out on: every configured channel, but
/// WhatsApp only with an approved template (`whatsapp.template_sid`). Twilio
/// lets a business start a WhatsApp conversation only with one; without it
/// WhatsApp alerts aren't offered, and replies inside the person's 24 h
/// window still go as plain text on `configured_channels`' WhatsApp.
pub async fn alert_channels(ctx: &Ctx) -> anyhow::Result<Vec<&'static str>> {
    let mut out = configured_channels(ctx).await?;
    if out.contains(&"whatsapp") && set(&load(ctx).await?.whatsapp.template_sid).is_none() {
        out.retain(|c| *c != "whatsapp");
    }
    Ok(out)
}

// ---- messages sent on the spot ------------------------------------------------------

/// A message that was sent (or failed) while someone waited, e.g. a test or
/// a verification code, recorded after the fact.
#[derive(Debug, Clone, Default)]
pub struct Sent {
    pub channel: String,
    pub to: String,
    /// As stored: codes are masked.
    pub text: String,
    pub subject: Option<String>,
    pub kind: String,
    pub user_id: Option<String>,
    pub idempotency_key: Option<String>,
    pub status: String,
    pub provider_id: Option<String>,
    pub error: Option<String>,
    /// For Twilio texts: when to poll the delivery status first.
    pub next_attempt_at: Option<DateTime<Utc>>,
}

/// Store a [`Sent`] message and publish it.
pub async fn record_sent(ctx: &Ctx, s: Sent) -> anyhow::Result<MessageLog> {
    anyhow::ensure!(op_core::messages::STATUSES.contains(&s.status.as_str()), "unknown message status {:?}", s.status);
    let t = to_db(&now());
    let row = sqlx::query(
        "INSERT INTO messages (id, direction, channel, address, user_id, kind, text, subject, status, error, provider_id, idempotency_key, attempts, next_attempt_at, created_at, updated_at)
         VALUES (?, 'out', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1, ?, ?, ?) RETURNING *",
    )
    .bind(id::new_id(MESSAGE))
    .bind(&s.channel)
    .bind(&s.to)
    .bind(&s.user_id)
    .bind(&s.kind)
    .bind(&s.text)
    .bind(&s.subject)
    .bind(&s.status)
    .bind(&s.error)
    .bind(&s.provider_id)
    .bind(&s.idempotency_key)
    .bind(s.next_attempt_at.as_ref().map(to_db))
    .bind(&t)
    .bind(&t)
    .fetch_one(ctx.db())
    .await?;
    let msg = message_from_row(&row)?;
    ctx.publish(Event::Message { message: msg.clone() });
    Ok(msg)
}

/// A message to hand straight to a channel (never stored as is).
pub(crate) fn draft(channel: &str, to: &str, kind: &str, text: &str, subject: Option<&str>) -> MessageLog {
    let t = now();
    MessageLog {
        id: id::new_id(MESSAGE),
        direction: "out".into(),
        channel: channel.into(),
        address: to.into(),
        kind: kind.into(),
        text: text.into(),
        subject: subject.map(Into::into),
        status: "sending".into(),
        attempts: 1,
        created_at: t,
        updated_at: t,
        ..Default::default()
    }
}

// ---- shared helpers -----------------------------------------------------------------

/// One HTTP client for every channel: rustls, 20 s per request.
pub(crate) fn http() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!("openpasture/", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("HTTP client")
    })
}

/// A network failure in words, without the URL (it may carry a key). Not
/// getting a connection at all is [`ChannelError::Offline`]; a timeout or a
/// dropped answer after connecting may have reached the provider, so it is a
/// [`ChannelError::Retry`].
pub(crate) fn net_error(what: &str, e: &reqwest::Error) -> ChannelError {
    if e.is_connect() {
        return ChannelError::Offline(format!("{what} can't be reached."));
    }
    ChannelError::Retry(format!("{what} {}.", if e.is_timeout() { "timed out" } else { "didn't answer" }))
}

/// A fresh 6-digit one-time code.
pub(crate) fn new_code() -> String {
    use rand::Rng;
    format!("{:06}", rand::rngs::OsRng.gen_range(0..1_000_000u32))
}

/// sha256 hex over the parts joined by ':'.
pub(crate) fn code_hash(parts: &[&str]) -> String {
    op_core::keys::hash_key(&parts.join(":"))
}

/// Equal without leaking where they first differ.
pub(crate) fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// The one-time-code text. It is the first text a number gets, so it carries
/// the opt-out line US carriers ask for.
pub fn code_text(code: &str) -> String {
    format!("openpasture code {code}. Reply STOP to opt out.")
}

/// What is stored instead of [`code_text`]: codes are never kept in the clear.
pub fn masked_code_text() -> String {
    code_text("******")
}

/// A test text. It may be the first text a number gets, so it carries the
/// opt-out line too.
pub const TEST_TEXT: &str = "openpasture test. Reply STOP to opt out.";

/// A test email or webhook (nothing to opt out of by reply).
pub const TEST_NOTE: &str = "openpasture test.";

/// Every character is in the GSM 03.38 default alphabet or its extension
/// table, so a text goes as 7-bit (160 characters a part) and not UCS-2.
pub fn is_gsm7(s: &str) -> bool {
    const BASIC: &str = "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞÆæßÉ !\"#¤%&'()*+,-./0123456789:;<=>?¡ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑÜ§¿abcdefghijklmnopqrstuvwxyzäöñüà";
    const EXTENDED: &str = "^{}\\[~]|€\u{c}";
    s.chars().all(|c| BASIC.contains(c) || EXTENDED.contains(c))
}

/// Length in GSM-7 septets (extension characters count two).
pub fn gsm7_len(s: &str) -> usize {
    const EXTENDED: &str = "^{}\\[~]|€\u{c}";
    s.chars().map(|c| if EXTENDED.contains(c) { 2 } else { 1 }).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn texts_are_gsm7_and_fit_one_part() {
        for t in [code_text("123456"), masked_code_text(), TEST_TEXT.to_owned()] {
            assert!(is_gsm7(&t), "{t}");
            assert!(gsm7_len(&t) <= 160, "{t}");
            assert!(t.ends_with("Reply STOP to opt out."), "{t}");
            assert!(t.starts_with("openpasture"), "{t}");
        }
        assert!(!is_gsm7("curly ’quote’"));
        assert!(!is_gsm7("em — dash"));
        assert!(!is_gsm7("cow 🐄"));
        assert_eq!(gsm7_len("a[b]"), 6);
    }

    #[test]
    fn codes_are_six_digits_and_hashes_compare_whole() {
        for _ in 0..200 {
            let c = new_code();
            assert_eq!(c.len(), 6);
            assert!(c.bytes().all(|b| b.is_ascii_digit()));
        }
        let h = code_hash(&["usr_1", "+15155550123", "123456"]);
        assert!(same(&h, &code_hash(&["usr_1", "+15155550123", "123456"])));
        assert!(!same(&h, &code_hash(&["usr_1", "+15155550124", "123456"])));
        assert!(!same(&h, "relay"));
    }

    #[test]
    fn config_defaults_match_the_contract() {
        let c = ChannelsConfig::default();
        assert_eq!(c.email.port, 587);
        assert_eq!(c.email.tls, Tls::Starttls);
        assert!(!c.relay.enabled);
        assert_eq!(c.twilio_api_base, "https://api.twilio.com");
        let v = serde_json::to_value(&c).unwrap();
        assert_eq!(v["email"]["tls"], "starttls");
        assert!(v["sms"].as_object().unwrap().is_empty());
        // Old or partial rows fill from the defaults.
        let c: ChannelsConfig = serde_json::from_value(serde_json::json!({"sms": {"from": "+15155550100"}})).unwrap();
        assert_eq!(c.sms.from.as_deref(), Some("+15155550100"));
        assert_eq!(c.email.port, 587);
    }
}
