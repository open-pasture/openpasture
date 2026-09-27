//! Texts coming in, and what each one does.
//!
//! Three ways in, one path after that ([`receive`]):
//! - **Webhook** (with `server.public_url`): Twilio posts to
//!   `/hooks/twilio/sms` and `/hooks/twilio/whatsapp`, signed with the
//!   farm's auth token over `public_url` + path + query ([`hook`]).
//! - **Polling** (without a public URL, e.g. a farm behind NAT): Twilio's
//!   message list is read every `texting.poll_s` seconds and new texts are
//!   taken once each, by `MessageSid` ([`poll`]).
//! - **Relay**: the hosted relay's inbox, long-polled while the relay channel
//!   is on ([`relay`]).
//!
//! A text from a verified phone of a person on the farm is a command
//! ([`commands`], [`act`]) or a question for the brain; anything else is
//! logged `ignored` and gets no reply. A 6-digit code texted back confirms a
//! pending phone verification. Replies are queued on the outbox with kind
//! `reply`, on the channel the text came in on.

pub mod act;
pub mod api;
pub mod commands;
pub mod hook;
pub mod poll;
pub mod questions;
pub mod relay;
pub mod reminders;
pub mod state;

use chrono::Timelike;
use op_core::alert::MessageLog;
use op_core::messages::{self, Inbound, Outbound};
use op_core::notify_config::configured_channels;
use op_core::users::{self, User, normalize_phone};
use op_core::{Ctx, Role};
use serde::{Deserialize, Serialize};

use commands::Command;

/// The setting key.
pub const TEXTING_KEY: &str = "texting";

/// `texting`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TextingConfig {
    /// Act on texts that come in. Off: no polling, the webhook answers 403,
    /// and texts from the relay's inbox (still read, so the relay's dead-man
    /// knows the farm is up) only count for STOP and START.
    pub inbound: bool,
    /// Seconds between checks of Twilio for new texts (polling only).
    pub poll_s: u32,
    /// Hours after our last alert or brief to a number during which a Y, N or
    /// STOP MOVE without the decision's code counts.
    pub approve_window_h: u32,
    pub brief: BriefConfig,
}

impl Default for TextingConfig {
    fn default() -> Self {
        Self { inbound: true, poll_s: 10, approve_window_h: 12, brief: BriefConfig::default() }
    }
}

/// The morning brief by text.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BriefConfig {
    pub enabled: bool,
    /// Farm time, `HH:MM`.
    pub time: String,
}

impl Default for BriefConfig {
    fn default() -> Self {
        Self { enabled: false, time: "06:30".into() }
    }
}

/// The stored config; defaults when there is none or it can't be read.
pub async fn load(ctx: &Ctx) -> anyhow::Result<TextingConfig> {
    let Some(v) = ctx.store().get_setting_json(TEXTING_KEY).await? else { return Ok(TextingConfig::default()) };
    Ok(serde_json::from_value(v).unwrap_or_else(|e| {
        tracing::warn!("texting is unreadable ({e}); using defaults");
        TextingConfig::default()
    }))
}

pub async fn save(ctx: &Ctx, cfg: &TextingConfig) -> anyhow::Result<()> {
    ctx.store().set_setting(TEXTING_KEY, cfg).await
}

/// How texts reach this server now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Twilio posts them to `/hooks/twilio/*` (a public URL is set).
    Webhook,
    /// This server reads them from Twilio every `poll_s` seconds.
    Polling,
    /// The hosted relay holds them; this server long-polls its inbox.
    Relay,
    /// Nothing comes in: texting in is off, or no channel can text.
    Off,
}

/// The farm's own Twilio channels that can text now.
pub async fn twilio_channels(ctx: &Ctx) -> anyhow::Result<Vec<&'static str>> {
    Ok(configured_channels(ctx).await?.into_iter().filter(|c| matches!(*c, "sms" | "whatsapp")).collect())
}

/// `server.public_url`, without a trailing slash.
pub async fn public_url(ctx: &Ctx) -> anyhow::Result<Option<String>> {
    Ok(ctx.settings().await?.server.public_url.map(|u| u.trim().trim_end_matches('/').to_owned()).filter(|u| !u.is_empty()))
}

/// The farm's own Twilio decides the mode (webhook with a public URL, else
/// polling); without it the relay's inbox, when the relay is on.
pub async fn mode(ctx: &Ctx) -> anyhow::Result<Mode> {
    if !load(ctx).await?.inbound {
        return Ok(Mode::Off);
    }
    let configured = configured_channels(ctx).await?;
    Ok(if configured.iter().any(|c| matches!(*c, "sms" | "whatsapp")) {
        if public_url(ctx).await?.is_some() { Mode::Webhook } else { Mode::Polling }
    } else if configured.contains(&"relay") {
        Mode::Relay
    } else {
        Mode::Off
    })
}

/// The channel a reply to a text on `channel` goes out on.
pub fn reply_channel(channel: &str) -> &'static str {
    match channel {
        "whatsapp" => "whatsapp",
        "relay" => "relay",
        _ => "sms",
    }
}

/// Longest reply on a channel, in characters (SMS: GSM-7 septets).
pub fn reply_max(channel: &str) -> usize {
    if channel == "whatsapp" { 1000 } else { 320 }
}

/// What came of one text.
#[derive(Debug, Clone)]
pub struct Received {
    /// The stored text: status `received`, or `ignored` with the reason in `error`.
    pub message: MessageLog,
    /// The reply queued for it. A question is answered in the background, so
    /// its reply comes later.
    pub reply: Option<MessageLog>,
}

/// Take one text: store it (once per provider id: a webhook retry or an
/// overlapping poll returns `None`), work out who sent it, and act. A
/// question is answered in the background.
pub async fn receive(ctx: &Ctx, m: Inbound) -> anyhow::Result<Option<Received>> {
    let raw = m.from.trim().trim_start_matches("whatsapp:").trim().to_owned();
    let phone = normalize_phone(&raw);
    let user = match &phone {
        Some(p) => users::user_by_phone(ctx, p).await?,
        None => None,
    };
    let inbound = Inbound { from: phone.clone().unwrap_or(raw), ..m };
    let Some(msg) = messages::record_inbound(ctx, inbound, user.as_ref().map(|u| u.id.as_str()), "received").await? else { return Ok(None) };
    if phone.is_none() {
        return ignored(ctx, msg, "Not a phone number.").await;
    }
    let cmd = commands::parse(&msg.text);

    // A relay host: a text from a farm's recipient goes to the farm it answers.
    let mut forwarded = 0;
    if matches!(msg.channel.as_str(), "sms" | "whatsapp") && crate::hosting::load(ctx).await?.enabled {
        match crate::hosting::inbox::route(ctx, &msg, &cmd, user.as_ref()).await? {
            crate::hosting::inbox::Route::Asked(reply) => return Ok(Some(Received { message: msg, reply: Some(reply) })),
            crate::hosting::inbox::Route::Farms(n) => forwarded = n,
            crate::hosting::inbox::Route::Host => {}
        }
        if forwarded > 0 && !matches!(cmd, Command::OptOut | Command::OptIn) {
            return Ok(Some(Received { message: msg, reply: None }));
        }
    }

    let Some(user) = user else {
        return if forwarded > 0 { Ok(Some(Received { message: msg, reply: None })) } else { ignored(ctx, msg, "Unknown number.").await };
    };
    let cfg = load(ctx).await?;
    if !cfg.inbound {
        // Texting in is off (only the relay's inbox is still read, for its dead-man).
        return match cmd {
            Command::OptOut | Command::OptIn => {
                crate::routing::prefs::set_sms_opt_out(ctx, &user.id, matches!(cmd, Command::OptOut)).await?;
                Ok(Some(Received { message: msg, reply: None }))
            }
            _ => ignored(ctx, msg, "Texts in are off.").await,
        };
    }
    handle(ctx, &cfg, msg, &user, cmd).await
}

/// A known person's text.
async fn handle(ctx: &Ctx, cfg: &TextingConfig, msg: MessageLog, user: &User, cmd: Command) -> anyhow::Result<Option<Received>> {
    use crate::routing::prefs;

    if user.phone_verified_at.is_none() {
        return match cmd {
            Command::Code(code) => match act::confirm_code(ctx, user, &code).await? {
                Some(text) => replied(ctx, msg, user, text, None, false).await,
                None => ignored(ctx, msg, "That code didn't verify the phone.").await,
            },
            // Mirror the carrier's opt-out even before the phone is verified.
            Command::OptOut => {
                prefs::set_sms_opt_out(ctx, &user.id, true).await?;
                Ok(Some(Received { message: msg, reply: None }))
            }
            Command::OptIn => {
                prefs::set_sms_opt_out(ctx, &user.id, false).await?;
                Ok(Some(Received { message: msg, reply: None }))
            }
            _ => ignored(ctx, msg, "Phone not verified.").await,
        };
    }

    let (_, opted_out) = prefs::get(ctx, &user.id).await?;
    if opted_out {
        // Twilio opts a number back in on START, YES or UNSTOP; nothing else counts.
        if matches!(cmd, Command::OptIn) || commands::twilio_opt_in(&msg.text) {
            prefs::set_sms_opt_out(ctx, &user.id, false).await?;
            return match msg.channel.as_str() {
                "whatsapp" => replied(ctx, msg, user, act::OPTED_IN.into(), None, false).await,
                _ => Ok(Some(Received { message: msg, reply: None })),
            };
        }
        return ignored(ctx, msg, "Opted out: only START counts.").await;
    }

    match cmd {
        Command::OptOut => {
            prefs::set_sms_opt_out(ctx, &user.id, true).await?;
            // On SMS the carrier (Twilio) confirms STOP itself, and nothing more may go to the number.
            match msg.channel.as_str() {
                "whatsapp" => replied(ctx, msg, user, act::OPTED_OUT.into(), None, false).await,
                _ => Ok(Some(Received { message: msg, reply: None })),
            }
        }
        // Already opted in; HELP and INFO are Twilio's to answer.
        Command::OptIn | Command::Reserved => Ok(Some(Received { message: msg, reply: None })),
        // Six digits from a verified phone aren't a code any more: a question like any other.
        Command::Question(q) | Command::Code(q) => match questions::admit(ctx, &user.id, op_core::time::now()) {
            questions::Admit::Ask(turn) => {
                spawn_question(ctx.clone(), cfg.clone(), msg.clone(), user.clone(), q, turn);
                Ok(Some(Received { message: msg, reply: None }))
            }
            questions::Admit::Refuse { tell: Some(text), why } => {
                messages::mark(ctx, &msg.id, "ignored", None, Some(why), None).await?;
                replied(ctx, msg, user, text, None, false).await
            }
            questions::Admit::Refuse { tell: None, why } => ignored(ctx, msg, why).await,
        },
        cmd => {
            let out = act::run(ctx, cfg, &msg, user, cmd).await?;
            if let Some(why) = &out.note {
                messages::mark(ctx, &msg.id, "received", None, Some(why), None).await?;
            }
            match out.text {
                Some(text) => replied(ctx, msg, user, text, out.decision_id, out.prompt).await,
                None => Ok(Some(Received { message: msg, reply: None })),
            }
        }
    }
}

/// Answer a question in the background, in its turn ([`questions`]).
fn spawn_question(ctx: Ctx, cfg: TextingConfig, msg: MessageLog, user: User, q: String, turn: questions::Turn) {
    tokio::spawn(async move {
        let text = {
            let _slot = turn.run_slot().await;
            act::ask(&ctx, &cfg, &msg, &user, &q).await
        };
        // Answered: their next question may go (before they can see this answer).
        drop(turn);
        if let Err(e) = reply(&ctx, &msg, &user, text, None).await {
            tracing::warn!(message = %msg.id, "queueing the answer to a text: {e:#}");
        }
    });
}

async fn ignored(ctx: &Ctx, msg: MessageLog, why: &str) -> anyhow::Result<Option<Received>> {
    messages::mark(ctx, &msg.id, "ignored", None, Some(why), None).await?;
    let message = messages::get_message(ctx, &msg.id).await?.unwrap_or(msg);
    Ok(Some(Received { message, reply: None }))
}

async fn replied(ctx: &Ctx, msg: MessageLog, user: &User, text: String, decision_id: Option<String>, prompt: bool) -> anyhow::Result<Option<Received>> {
    let r = send_reply(ctx, &msg, user, text, decision_id, prompt).await?;
    let message = messages::get_message(ctx, &msg.id).await?.unwrap_or(msg);
    Ok(Some(Received { message, reply: Some(r) }))
}

/// Queue the reply to text `msg`, on its channel, to its number (once per text).
pub async fn reply(ctx: &Ctx, msg: &MessageLog, user: &User, text: String, decision_id: Option<String>) -> anyhow::Result<MessageLog> {
    send_reply(ctx, msg, user, text, decision_id, false).await
}

/// [`reply`]; a `prompt` asks about `decision_id` (its idempotency key starts
/// `prompt:`, see [`act::ASKS`]), so the next bare Y or N answers that one.
async fn send_reply(ctx: &Ctx, msg: &MessageLog, user: &User, text: String, decision_id: Option<String>, prompt: bool) -> anyhow::Result<MessageLog> {
    messages::enqueue(
        ctx,
        Outbound {
            idempotency_key: format!("{}:{}", if prompt && decision_id.is_some() { "prompt" } else { "reply" }, msg.id),
            channel: reply_channel(&msg.channel).into(),
            to: msg.address.clone(),
            text,
            subject: None,
            kind: "reply".into(),
            alert_id: None,
            decision_id,
            user_id: Some(user.id.clone()),
        },
    )
    .await
}

/// The role a command needs, for the refusal words.
pub(crate) fn can(user: &User, need: Role) -> bool {
    user.role >= need
}

/// Background loops: Twilio polling, the relay inbox, "later" reminders.
pub fn start(ctx: Ctx) {
    poll::spawn(ctx.clone());
    relay::spawn(ctx.clone());
    reminders::spawn(ctx);
}

/// Routes: `/hooks/twilio/*`, `/api/texting*`.
pub fn router() -> axum::Router<Ctx> {
    hook::router().merge(api::router())
}

/// `HH:MM` as a time of day.
pub(crate) fn hhmm(s: &str) -> Option<chrono::NaiveTime> {
    let t = chrono::NaiveTime::parse_from_str(s.trim(), "%H:%M").ok()?;
    (t.second() == 0).then_some(t)
}
