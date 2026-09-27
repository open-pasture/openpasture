//! Twilio SMS and WhatsApp: `POST {base}/2010-04-01/Accounts/{sid}/Messages.json`
//! with basic auth and a form (`To`, `From` or `MessagingServiceSid`, `Body`,
//! or `ContentSid` + `ContentVariables` for a WhatsApp template), and the
//! delivery status read back from `…/Messages/{sid}.json`.
//!
//! 429 and 5xx are worth retrying; any other 4xx fails with Twilio's own
//! message, which is what a farmer needs to fix a number or an account.

use op_core::Ctx;
use op_core::alert::MessageLog;
use serde::Deserialize;
use serde_json::json;

use super::{Channel, ChannelError, ChannelsConfig, Delivery, http, net_error, secret, set};

pub const DEFAULT_API_BASE: &str = "https://api.twilio.com";

/// One Twilio sender: SMS or WhatsApp, with the farm's account.
#[derive(Debug, Clone)]
pub struct Twilio {
    kind: &'static str,
    base: String,
    sid: String,
    token: String,
    from: String,
    template_sid: Option<String>,
}

/// What Twilio says about a message it took.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// Still on its way (queued, accepted, sending, sent to the carrier).
    Pending(String),
    Delivered,
    /// Undelivered, failed or canceled, with why.
    Failed(String),
}

#[derive(Deserialize)]
struct TwilioError {
    #[serde(default)]
    code: Option<i64>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Deserialize)]
struct TwilioMessage {
    #[serde(default)]
    sid: String,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    error_code: Option<i64>,
    #[serde(default)]
    error_message: Option<String>,
}

impl Twilio {
    /// The account and number for `kind` (`sms` | `whatsapp`), or `None` when
    /// any of them is missing.
    pub fn from_config(ctx: &Ctx, cfg: &ChannelsConfig, kind: &str) -> anyhow::Result<Option<Self>> {
        let (kind, from, template_sid) = match kind {
            "sms" => ("sms", set(&cfg.sms.from), None),
            "whatsapp" => ("whatsapp", set(&cfg.whatsapp.from), set(&cfg.whatsapp.template_sid)),
            _ => return Ok(None),
        };
        let (Some(from), Some(sid), Some(token)) = (from, secret(ctx, "twilio_account_sid")?, secret(ctx, "twilio_auth_token")?) else {
            return Ok(None);
        };
        Ok(Some(Self {
            kind,
            base: cfg.twilio_api_base.trim().trim_end_matches('/').to_owned(),
            sid,
            token,
            from: from.to_owned(),
            template_sid: template_sid.map(Into::into),
        }))
    }

    fn account_url(&self) -> String {
        format!("{}/2010-04-01/Accounts/{}", self.base, self.sid)
    }

    /// Twilio's address for a phone on this channel.
    fn address(&self, phone: &str) -> String {
        let phone = phone.trim();
        if self.kind == "whatsapp" && !phone.starts_with("whatsapp:") { format!("whatsapp:{phone}") } else { phone.to_owned() }
    }

    /// The form Twilio gets for `msg`.
    pub fn form(&self, msg: &MessageLog) -> Vec<(&'static str, String)> {
        let mut form = vec![("To", self.address(&msg.address))];
        if self.kind == "sms" && self.from.starts_with("MG") {
            form.push(("MessagingServiceSid", self.from.clone()));
        } else {
            form.push(("From", self.address(&self.from)));
        }
        // WhatsApp only lets a business start a conversation with an approved
        // template; a reply inside the customer's 24 h window may be free text.
        match &self.template_sid {
            Some(t) if self.kind == "whatsapp" && msg.kind != "reply" => {
                form.push(("ContentSid", t.clone()));
                form.push(("ContentVariables", json!({ "1": msg.text }).to_string()));
            }
            _ => form.push(("Body", msg.text.clone())),
        }
        form
    }

    /// Read back what happened to message `message_sid`.
    pub async fn status(&self, message_sid: &str) -> Result<Status, ChannelError> {
        let url = format!("{}/Messages/{}.json", self.account_url(), message_sid);
        let res = http().get(url).basic_auth(&self.sid, Some(&self.token)).send().await.map_err(|e| net_error("Twilio", &e))?;
        let code = res.status();
        let body = res.bytes().await.map_err(|e| net_error("Twilio", &e))?;
        if !code.is_success() {
            return Err(error_for(code.as_u16(), &body));
        }
        let m: TwilioMessage = serde_json::from_slice(&body).map_err(|_| ChannelError::Retry("Twilio sent something unreadable.".into()))?;
        Ok(status_of(&m))
    }

    /// Check the account and number without sending anything.
    pub async fn check(&self) -> Result<String, ChannelError> {
        let res =
            http().get(format!("{}.json", self.account_url())).basic_auth(&self.sid, Some(&self.token)).send().await.map_err(|e| net_error("Twilio", &e))?;
        let code = res.status();
        let body = res.bytes().await.map_err(|e| net_error("Twilio", &e))?;
        if !code.is_success() {
            return Err(error_for(code.as_u16(), &body));
        }
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
        let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("active");
        if status != "active" {
            return Err(ChannelError::Fail(format!("The Twilio account is {status}.")));
        }
        Ok(format!("Account active, sending from {}", self.from))
    }
}

fn status_of(m: &TwilioMessage) -> Status {
    let status = m.status.as_deref().unwrap_or("queued");
    match status {
        "delivered" | "read" => Status::Delivered,
        "undelivered" | "failed" | "canceled" => {
            let why = match (&m.error_message, m.error_code) {
                (Some(msg), Some(code)) => format!("Twilio {code}: {msg}"),
                (Some(msg), None) => msg.clone(),
                (None, Some(code)) => format!("Twilio {code}: {status}"),
                (None, None) => format!("Twilio: {status}"),
            };
            Status::Failed(why)
        }
        other => Status::Pending(other.to_owned()),
    }
}

/// Retry on 429 and 5xx; anything else fails with Twilio's message.
pub(crate) fn error_for(status: u16, body: &[u8]) -> ChannelError {
    let e: Option<TwilioError> = serde_json::from_slice(body).ok();
    let msg = e.as_ref().and_then(|e| e.message.clone()).filter(|m| !m.trim().is_empty());
    let text = match (e.and_then(|e| e.code), msg) {
        (Some(code), Some(m)) => format!("Twilio {code}: {m}"),
        (None, Some(m)) => format!("Twilio: {m}"),
        (_, None) => format!("Twilio answered {status}."),
    };
    if status == 429 || status >= 500 { ChannelError::Retry(text) } else { ChannelError::Fail(text) }
}

#[async_trait::async_trait]
impl Channel for Twilio {
    fn kind(&self) -> &'static str {
        self.kind
    }

    async fn send(&self, msg: &MessageLog) -> Result<Delivery, ChannelError> {
        let url = format!("{}/Messages.json", self.account_url());
        let res = http().post(url).basic_auth(&self.sid, Some(&self.token)).form(&self.form(msg)).send().await.map_err(|e| net_error("Twilio", &e))?;
        let code = res.status();
        let body = res.bytes().await.map_err(|e| net_error("Twilio", &e))?;
        if !code.is_success() {
            return Err(error_for(code.as_u16(), &body));
        }
        let m: TwilioMessage =
            serde_json::from_slice(&body).map_err(|_| ChannelError::Fail("Twilio took the text but sent back something unreadable.".into()))?;
        let sid = Some(m.sid.clone()).filter(|s| !s.is_empty());
        match status_of(&m) {
            Status::Failed(why) => Err(ChannelError::Fail(why)),
            Status::Delivered => Ok(Delivery::delivered(sid)),
            Status::Pending(_) => Ok(Delivery::sent(sid)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_map_to_retry_or_fail() {
        let body = br#"{"code": 21211, "message": "The 'To' number +1555 is not a valid phone number.", "status": 400}"#;
        assert_eq!(error_for(400, body), ChannelError::Fail("Twilio 21211: The 'To' number +1555 is not a valid phone number.".into()));
        assert!(matches!(error_for(429, br#"{"code": 20429, "message": "Too Many Requests"}"#), ChannelError::Retry(_)));
        assert_eq!(error_for(503, b"<html>"), ChannelError::Retry("Twilio answered 503.".into()));
        assert!(matches!(error_for(401, b""), ChannelError::Fail(_)));
    }

    #[test]
    fn statuses_read_back() {
        let m = |s: &str, code: Option<i64>, msg: Option<&str>| TwilioMessage {
            sid: "SM1".into(),
            status: Some(s.into()),
            error_code: code,
            error_message: msg.map(Into::into),
        };
        assert_eq!(status_of(&m("delivered", None, None)), Status::Delivered);
        assert_eq!(status_of(&m("read", None, None)), Status::Delivered);
        assert_eq!(status_of(&m("sent", None, None)), Status::Pending("sent".into()));
        assert_eq!(status_of(&m("undelivered", Some(30007), None)), Status::Failed("Twilio 30007: undelivered".into()));
        assert_eq!(
            status_of(&m("failed", Some(30003), Some("Unreachable destination handset"))),
            Status::Failed("Twilio 30003: Unreachable destination handset".into())
        );
    }
}
