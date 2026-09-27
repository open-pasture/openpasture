//! The hosted relay, farm side: texts and emails go to `POST {hosted_url}/v1/notify`
//! with `Authorization: Bearer <hosted_api_key>` and the host sends them from
//! its own number. The farm's message id is the idempotency key, so a retry
//! after a lost answer is sent once. The same key covers the hosted brain.

use op_core::Ctx;
use op_core::alert::MessageLog;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{Channel, ChannelError, Delivery, http, net_error, secret};

pub struct Relay {
    url: String,
    key: String,
}

/// One address the relay may text for this key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Recipient {
    pub channel: String,
    pub to: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verified_at: Option<String>,
    #[serde(default)]
    pub deadman: bool,
}

/// What the relay answered when it said no: status and its `error` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub message: String,
}

impl Relay {
    /// The hosted key (and URL, default the openpasture service), or `None`
    /// without a key.
    pub fn from_secrets(ctx: &Ctx) -> anyhow::Result<Option<Self>> {
        let Some(key) = secret(ctx, "hosted_api_key")? else { return Ok(None) };
        let url = secret(ctx, "hosted_url")?.unwrap_or_else(|| op_brain::api::HOSTED_DEFAULT_URL.into());
        Ok(Some(Self { url: url.trim_end_matches('/').to_owned(), key }))
    }

    /// Which relay channel carries an address: email when it has an `@`,
    /// WhatsApp when it says so, else SMS.
    pub fn channel_for(address: &str) -> &'static str {
        let a = address.trim();
        if a.contains('@') {
            "email"
        } else if a.starts_with("whatsapp:") {
            "whatsapp"
        } else {
            "sms"
        }
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> Result<Result<Value, Refusal>, ChannelError> {
        let mut req = http().request(method, format!("{}{path}", self.url)).bearer_auth(&self.key);
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.map_err(|e| net_error("The relay", &e))?;
        let status = res.status().as_u16();
        let body = res.bytes().await.map_err(|e| net_error("The relay", &e))?;
        let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        if (200..300).contains(&status) {
            return Ok(Ok(v));
        }
        let message = v.get("error").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("The relay answered {status}."));
        Ok(Err(Refusal { status, message }))
    }

    /// `GET /v1/notify/recipients`: a 200 is what lets Settings turn the relay on.
    pub async fn recipients(&self) -> Result<Vec<Recipient>, ChannelError> {
        match self.call(reqwest::Method::GET, "/v1/notify/recipients", None).await? {
            Ok(v) => serde_json::from_value(v).map_err(|_| ChannelError::Fail("The relay sent something unreadable.".into())),
            Err(r) => Err(refused(r)),
        }
    }

    /// `POST /v1/notify/recipients`: the relay texts `to` a code from its own number.
    pub async fn add_recipient(&self, channel: &str, to: &str, deadman: bool) -> Result<Result<Value, Refusal>, ChannelError> {
        self.call(reqwest::Method::POST, "/v1/notify/recipients", Some(json!({ "channel": channel, "to": to, "deadman": deadman }))).await
    }

    /// `POST /v1/notify/recipients/verify`.
    pub async fn verify_recipient(&self, channel: &str, to: &str, code: &str) -> Result<Result<Value, Refusal>, ChannelError> {
        self.call(reqwest::Method::POST, "/v1/notify/recipients/verify", Some(json!({ "channel": channel, "to": to, "code": code }))).await
    }

    // @A3
    /// `GET /v1/notify/inbox?since=&wait=`: texts to the relay's number meant
    /// for this server, the relay holding the request up to `wait_s` seconds
    /// while there are none.
    pub async fn inbox(&self, since: Option<&str>, wait_s: u64) -> Result<crate::hosting::inbox::Inbox, ChannelError> {
        let wait = wait_s.to_string();
        let res = http()
            .get(format!("{}/v1/notify/inbox", self.url))
            .bearer_auth(&self.key)
            .query(&[("since", since.unwrap_or("")), ("wait", wait.as_str())])
            .timeout(std::time::Duration::from_secs(wait_s + 20))
            .send()
            .await
            .map_err(|e| net_error("The relay", &e))?;
        let status = res.status().as_u16();
        let body = res.bytes().await.map_err(|e| net_error("The relay", &e))?;
        if !(200..300).contains(&status) {
            let v: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
            let message = v.get("error").and_then(Value::as_str).map(str::to_owned).unwrap_or_else(|| format!("The relay answered {status}."));
            return Err(refused(Refusal { status, message }));
        }
        serde_json::from_slice(&body).map_err(|_| ChannelError::Fail("The relay sent an inbox that can't be read.".into()))
    }
}

/// 401, 403 and 409 need a person; 429 and 5xx pass.
fn refused(r: Refusal) -> ChannelError {
    match r.status {
        401 => ChannelError::Fail("The relay didn't accept the key.".into()),
        429 | 500..=599 => ChannelError::Retry(r.message),
        _ => ChannelError::Fail(r.message),
    }
}

#[async_trait::async_trait]
impl Channel for Relay {
    fn kind(&self) -> &'static str {
        "relay"
    }

    async fn send(&self, msg: &MessageLog) -> Result<Delivery, ChannelError> {
        let channel = Self::channel_for(&msg.address);
        let to = msg.address.trim().trim_start_matches("whatsapp:");
        let mut body = json!({ "idempotency_key": msg.id, "channel": channel, "to": to, "text": msg.text, "kind": msg.kind });
        if let Some(s) = &msg.subject {
            body["subject"] = json!(s);
        }
        match self.call(reqwest::Method::POST, "/v1/notify", Some(body)).await? {
            Ok(v) => Ok(Delivery::sent(v.get("id").and_then(Value::as_str).map(str::to_owned))),
            Err(r) => Err(refused(r)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn addresses_pick_their_relay_channel() {
        assert_eq!(Relay::channel_for("+15155550123"), "sms");
        assert_eq!(Relay::channel_for("whatsapp:+15155550123"), "whatsapp");
        assert_eq!(Relay::channel_for("cody@farm.example"), "email");
    }

    #[test]
    fn refusals_retry_only_when_waiting_helps() {
        let r = |status: u16| refused(Refusal { status, message: "no".into() });
        assert!(matches!(r(429), ChannelError::Retry(_)));
        assert!(matches!(r(502), ChannelError::Retry(_)));
        assert!(matches!(r(403), ChannelError::Fail(_)));
        assert!(matches!(r(409), ChannelError::Fail(_)));
        assert_eq!(r(401), ChannelError::Fail("The relay didn't accept the key.".into()));
    }
}
