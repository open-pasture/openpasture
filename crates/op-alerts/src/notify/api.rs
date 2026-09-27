//! Settings > Texting and the message log:
//! `GET/PUT /api/notify/channels`, `POST /api/notify/test`,
//! `POST /api/notify/verify`, `POST /api/notify/verify/confirm` (owner), and
//! `GET /api/messages` (manager: it shows phone numbers).

use axum::extract::{Query, State};
use axum::routing::{get, post};
use axum::{Json, Router};
use op_core::alert::MessageLog;
use op_core::messages::message_from_row;
use op_core::notify_config::configured_channels;
use op_core::secrets::SecretStatus;
use op_core::time::{now, to_db};
use op_core::users::normalize_phone;
use op_core::{ApiError, ApiJson, ApiResult, Ctx, patch};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::relay::Relay;
use super::twilio::Twilio;
use super::verify::{self, CodeSent, Verified};
use super::{ChannelsConfig, RelayConfig, SECRETS, Sent, TEST_TEXT, channel, draft, label, load, record_sent, save, secret, set};

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/notify/channels", get(get_channels).put(put_channels))
        .route("/api/notify/test", post(test))
        .route("/api/notify/verify", post(send_code))
        .route("/api/notify/verify/confirm", post(confirm_code))
        .route("/api/messages", get(list_messages))
}

/// The channel config, whether each secret is set, and which channels can
/// send now.
#[derive(Debug, Serialize)]
pub struct ChannelsView {
    #[serde(flatten)]
    pub config: ChannelsConfig,
    pub secrets: Vec<SecretStatus>,
    pub configured: Vec<&'static str>,
}

async fn view(ctx: &Ctx) -> ApiResult<ChannelsView> {
    let names = ctx.secrets().list_names()?;
    Ok(ChannelsView {
        config: load(ctx).await?,
        secrets: SECRETS
            .iter()
            .map(|n| SecretStatus { name: (*n).to_owned(), set: names.iter().any(|s| s == n) && secret(ctx, n).ok().flatten().is_some() })
            .collect(),
        configured: configured_channels(ctx).await?,
    })
}

async fn get_channels(State(ctx): State<Ctx>) -> ApiResult<Json<ChannelsView>> {
    Ok(Json(view(&ctx).await?))
}

/// A merge patch over [`ChannelsConfig`] plus `secrets: { name: value | null }`
/// (null or "" removes). Secrets are saved first. `relay.enabled: true` asks
/// the relay (`GET {hosted_url}/v1/notify/recipients`) and turns it on only on
/// a 200; so does a new relay key or URL while it is on. When the relay says
/// no, the rest is saved, the relay is off, and the answer is a 400 with the
/// relay's words.
async fn put_channels(State(ctx): State<Ctx>, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<ChannelsView>> {
    let Value::Object(mut body) = body else { return Err(ApiError::bad_request("Expected a JSON object.")) };
    let secrets = match body.remove("secrets") {
        None | Some(Value::Null) => Map::new(),
        Some(Value::Object(m)) => m,
        Some(_) => return Err(ApiError::bad_request("secrets is an object of names and values.")),
    };
    // Read-only parts of the view, in case a client sends the view back.
    body.remove("configured");
    let want_relay = match body.remove("relay") {
        None | Some(Value::Null) => None,
        Some(Value::Object(m)) => match m.get("enabled") {
            None => None,
            Some(Value::Bool(b)) => Some(*b),
            Some(_) => return Err(ApiError::bad_request("relay.enabled is true or false.")),
        },
        Some(_) => return Err(ApiError::bad_request("relay is an object.")),
    };
    let current = load(&ctx).await?;
    let mut next: ChannelsConfig = patch::apply(&current, &Value::Object(body), &[])?;
    next.relay = current.relay.clone();
    clean(&mut next)?;

    let mut values = Vec::with_capacity(secrets.len());
    let mut relay_secret_changed = false;
    for (name, v) in &secrets {
        if !SECRETS.contains(&name.as_str()) {
            return Err(ApiError::bad_request(format!("{name} isn't a texting secret.")));
        }
        let value = match v {
            Value::Null => None,
            Value::String(s) => Some(s.trim().to_owned()).filter(|s| !s.is_empty()),
            _ => return Err(ApiError::bad_request(format!("{name} is text."))),
        };
        check_secret(name, value.as_deref())?;
        if name == "hosted_api_key" || name == "hosted_url" {
            relay_secret_changed |= secret(&ctx, name)? != value;
        }
        values.push((name.clone(), value));
    }
    for (name, value) in values {
        match value {
            Some(v) => ctx.secrets().set(&name, &v)?,
            None => {
                ctx.secrets().delete(&name)?;
            }
        }
    }

    let mut refused = None;
    match want_relay {
        Some(false) => next.relay = RelayConfig::default(),
        _ if (want_relay == Some(true) && !current.relay.enabled) || (next.relay.enabled && relay_secret_changed) => match check_relay(&ctx).await {
            Ok(()) => next.relay = RelayConfig { enabled: true, checked_at: Some(now()) },
            Err(why) => {
                next.relay = RelayConfig::default();
                refused = Some(why);
            }
        },
        _ => {}
    }
    save(&ctx, &next).await?;
    if let Some(why) = refused {
        return Err(ApiError::bad_request(why));
    }
    Ok(Json(view(&ctx).await?))
}

/// The relay answers `GET /v1/notify/recipients` with 200 for this key.
async fn check_relay(ctx: &Ctx) -> Result<(), String> {
    let Some(relay) = Relay::from_secrets(ctx).map_err(|e| e.to_string())? else { return Err("Add the relay key first.".into()) };
    relay.recipients().await.map(|_| ()).map_err(|e| e.message().to_owned())
}

/// Trim, drop empties and check the shapes a farmer types.
fn clean(c: &mut ChannelsConfig) -> ApiResult<()> {
    fn tidy(v: &mut Option<String>) {
        *v = v.as_deref().map(str::trim).filter(|s| !s.is_empty()).map(str::to_owned);
    }
    tidy(&mut c.sms.from);
    tidy(&mut c.whatsapp.from);
    tidy(&mut c.whatsapp.template_sid);
    tidy(&mut c.email.host);
    tidy(&mut c.email.user);
    tidy(&mut c.email.from);
    tidy(&mut c.webhook.url);
    // A number, or a Messaging Service SID (MG…) for farms that registered one for 10DLC.
    if let Some(f) = &c.sms.from
        && !is_service_sid(f)
    {
        c.sms.from = Some(normalize_phone(f).ok_or_else(|| ApiError::bad_request("The SMS number looks like +15155550123."))?);
    }
    if let Some(f) = &c.whatsapp.from {
        let f = f.trim_start_matches("whatsapp:");
        c.whatsapp.from = Some(normalize_phone(f).ok_or_else(|| ApiError::bad_request("The WhatsApp number looks like +15155550123."))?);
    }
    if let Some(t) = &c.whatsapp.template_sid
        && !sid_like(t, "HX")
    {
        return Err(ApiError::bad_request("Template SIDs start with HX."));
    }
    if let Some(from) = &c.email.from
        && !email_like(from)
    {
        return Err(ApiError::bad_request("The from address doesn't look right."));
    }
    if let Some(h) = &c.email.host
        && (h.contains("://") || h.contains('/') || h.chars().any(char::is_whitespace))
    {
        return Err(ApiError::bad_request("The mail server is a host name, like smtp.example.com."));
    }
    if c.email.port == 0 {
        return Err(ApiError::bad_request("The mail server port is 1 to 65535."));
    }
    if let Some(u) = &c.webhook.url
        && !http_url(u)
    {
        return Err(ApiError::bad_request("The webhook URL starts with https://."));
    }
    c.twilio_api_base = c.twilio_api_base.trim().trim_end_matches('/').to_owned();
    if c.twilio_api_base.is_empty() {
        c.twilio_api_base = super::twilio::DEFAULT_API_BASE.into();
    }
    if !http_url(&c.twilio_api_base) {
        return Err(ApiError::bad_request("twilio_api_base is an http(s) URL."));
    }
    Ok(())
}

fn check_secret(name: &str, value: Option<&str>) -> ApiResult<()> {
    let Some(v) = value else { return Ok(()) };
    match name {
        "twilio_account_sid" if !sid_like(v, "AC") => Err(ApiError::bad_request("Account SIDs start with AC.")),
        "hosted_api_key" if !v.starts_with(op_brain::hosted::KEY_PREFIX) => Err(ApiError::bad_request("Relay keys start with oph_.")),
        "hosted_url" if !http_url(v) => Err(ApiError::bad_request("The relay URL starts with https://.")),
        _ if v.len() > 4096 => Err(ApiError::bad_request("That is too long.")),
        _ => Ok(()),
    }
}

/// `AC…`, `HX…`, `MG…`: two letters and 32 hex digits.
fn sid_like(s: &str, prefix: &str) -> bool {
    s.len() == 34 && s.starts_with(prefix) && s[2..].bytes().all(|b| b.is_ascii_hexdigit())
}

fn is_service_sid(s: &str) -> bool {
    sid_like(s, "MG")
}

pub(crate) fn email_like(e: &str) -> bool {
    e.len() <= 254
        && !e.chars().any(char::is_whitespace)
        && e.split_once('@').is_some_and(|(l, d)| !l.is_empty() && d.contains('.') && !d.starts_with('.') && !d.ends_with('.'))
}

fn http_url(u: &str) -> bool {
    let rest = u.strip_prefix("https://").or_else(|| u.strip_prefix("http://"));
    rest.is_some_and(|r| !r.is_empty() && !r.starts_with('/') && !r.chars().any(char::is_whitespace))
}

// ---- test ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct TestBody {
    channel: String,
    #[serde(default)]
    to: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub detail: String,
}

fn result(ok: bool, detail: impl Into<String>) -> Json<TestResult> {
    Json(TestResult { ok, detail: detail.into() })
}

/// Send a test now and say what happened. Without `to`, Twilio checks the
/// account and the relay lists its recipients (nothing is sent).
async fn test(State(ctx): State<Ctx>, ApiJson(b): ApiJson<TestBody>) -> ApiResult<Json<TestResult>> {
    let kind = b.channel.trim();
    if !super::CHANNELS.contains(&kind) {
        return Err(ApiError::bad_request("channel is sms, whatsapp, email, webhook or relay."));
    }
    let to = b.to.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let Some(ch) = channel(&ctx, kind).await? else {
        return Ok(result(false, format!("{} isn't set up.", label(kind))));
    };
    let to = match (kind, to) {
        ("sms" | "whatsapp", None) => {
            let cfg = load(&ctx).await?;
            let tw = Twilio::from_config(&ctx, &cfg, kind)?.ok_or_else(|| ApiError::conflict("Twilio isn't set up."))?;
            return Ok(match tw.check().await {
                Ok(d) => result(true, d),
                Err(e) => result(false, e.message()),
            });
        }
        ("relay", None) => {
            let relay = Relay::from_secrets(&ctx)?.ok_or_else(|| ApiError::conflict("The relay isn't set up."))?;
            return Ok(match relay.recipients().await {
                Ok(list) => {
                    let n = list.iter().filter(|r| r.verified_at.is_some()).count();
                    result(true, if n == 1 { "1 verified recipient".to_owned() } else { format!("{n} verified recipients") })
                }
                Err(e) => result(false, e.message()),
            });
        }
        ("sms" | "whatsapp", Some(t)) => normalize_phone(t).ok_or_else(|| ApiError::bad_request("Phone numbers look like +15155550123."))?,
        ("email", Some(t)) if email_like(t) => t.to_owned(),
        ("email", _) => return Err(ApiError::bad_request("Give an email address to send the test to.")),
        ("relay", Some(t)) if t.contains('@') => {
            if !email_like(t) {
                return Err(ApiError::bad_request("That email address doesn't look right."));
            }
            t.to_owned()
        }
        ("relay", Some(t)) => normalize_phone(t).ok_or_else(|| ApiError::bad_request("Phone numbers look like +15155550123."))?,
        ("webhook", _) => set(&load(&ctx).await?.webhook.url).unwrap_or_default().to_owned(),
        _ => return Err(ApiError::bad_request("Nothing to test.")),
    };
    let subject = (kind == "email" || (kind == "relay" && to.contains('@'))).then_some("openpasture test");
    let msg = draft(kind, &to, "test", TEST_TEXT, subject);
    let res = ch.send(&msg).await;
    let (status, provider_id, error) = match &res {
        Ok(d) => (d.status.clone(), d.provider_id.clone(), None),
        Err(e) => ("failed".to_owned(), None, Some(e.message().to_owned())),
    };
    let poll = (matches!(kind, "sms" | "whatsapp") && status == "sent" && provider_id.is_some())
        .then(|| now() + chrono::Duration::seconds(super::sender::STATUS_POLLS[0]));
    record_sent(
        &ctx,
        Sent {
            channel: kind.into(),
            to: to.clone(),
            text: TEST_TEXT.into(),
            subject: subject.map(Into::into),
            kind: "test".into(),
            status: status.clone(),
            provider_id,
            error,
            next_attempt_at: poll,
            ..Default::default()
        },
    )
    .await?;
    Ok(match res {
        Ok(d) if d.status == "delivered" => result(true, if kind == "webhook" { "Delivered".to_owned() } else { format!("Delivered to {to}") }),
        Ok(_) => result(true, format!("Sent to {to}")),
        Err(e) => result(false, e.message()),
    })
}

// ---- verification -------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct VerifyBody {
    user_id: String,
}

#[derive(Debug, Deserialize)]
struct ConfirmBody {
    user_id: String,
    code: String,
}

async fn send_code(State(ctx): State<Ctx>, ApiJson(b): ApiJson<VerifyBody>) -> ApiResult<Json<CodeSent>> {
    Ok(Json(verify::send_code(&ctx, &b.user_id).await?))
}

async fn confirm_code(State(ctx): State<Ctx>, ApiJson(b): ApiJson<ConfirmBody>) -> ApiResult<Json<Verified>> {
    Ok(Json(verify::confirm(&ctx, &b.user_id, &b.code).await?))
}

// ---- messages -----------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct MessagesQuery {
    #[serde(default)]
    direction: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    /// Created at or after (RFC 3339).
    #[serde(default)]
    from: Option<chrono::DateTime<chrono::Utc>>,
    /// Created before (RFC 3339).
    #[serde(default)]
    to: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default)]
    user_id: Option<String>,
}

/// Newest first; `limit` 100 by default, at most 1,000.
async fn list_messages(State(ctx): State<Ctx>, Query(q): Query<MessagesQuery>) -> ApiResult<Json<Vec<MessageLog>>> {
    let direction = match q.direction.as_deref().map(str::trim) {
        None | Some("") | Some("all") => None,
        Some(d @ ("in" | "out")) => Some(d.to_owned()),
        Some(_) => return Err(ApiError::bad_request("direction is in or out.")),
    };
    let limit = q.limit.unwrap_or(100).clamp(1, 1000);
    let rows = sqlx::query(
        "SELECT * FROM messages
         WHERE (?1 IS NULL OR direction = ?1) AND (?2 IS NULL OR created_at >= ?2) AND (?3 IS NULL OR created_at < ?3) AND (?4 IS NULL OR user_id = ?4)
         ORDER BY created_at DESC, rowid DESC LIMIT ?5",
    )
    .bind(direction)
    .bind(q.from.as_ref().map(to_db))
    .bind(q.to.as_ref().map(to_db))
    .bind(q.user_id)
    .bind(limit)
    .fetch_all(ctx.db())
    .await?;
    Ok(Json(rows.iter().map(message_from_row).collect::<anyhow::Result<Vec<_>>>()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_farmers_type() {
        assert!(sid_like("AC0123456789abcdef0123456789abcdef", "AC"));
        assert!(!sid_like("AC0123", "AC"));
        assert!(!sid_like("XX0123456789abcdef0123456789abcdef", "AC"));
        assert!(http_url("https://hooks.example.com/op"));
        assert!(http_url("http://127.0.0.1:17202/hook"));
        assert!(!http_url("ftp://x"));
        assert!(!http_url("https://"));
        assert!(email_like("cody@farm.example"));
        assert!(!email_like("cody@farm"));
    }
}
