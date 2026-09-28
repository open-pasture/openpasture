//! The farm's webhook: `POST url` with JSON `{ "type": "alert", "alert": Alert,
//! "text" }` for a notified alert (else `{ "type": "message", "message":
//! MessageLog }`), signed with
//! `x-openpasture-signature: t=<unix>,v1=<hex HMAC-SHA256(webhook_secret, "<t>.<body>")>`
//! (the brand is lowercase; header names are case-insensitive).
//! A receiver checks the signature over the raw body and refuses old `t`.
//! 408, 429, 5xx and network errors are retried at 1 s, 5 s and 25 s.

use hmac::{Hmac, Mac};
use op_core::Ctx;
use op_core::alert::{Alert, MessageLog};
use serde_json::{Map, Value, json};
use sha2::Sha256;
use sqlx::{Column, Row, SqlitePool, TypeInfo, ValueRef};

use super::{Channel, ChannelError, ChannelsConfig, Delivery, http, net_error, secret, set};

pub const SIGNATURE_HEADER: &str = "x-openpasture-signature";

pub struct Webhook {
    ctx: Ctx,
    url: String,
    secret: String,
}

impl Webhook {
    pub fn from_config(ctx: &Ctx, cfg: &ChannelsConfig) -> anyhow::Result<Option<Self>> {
        let (Some(url), Some(secret)) = (set(&cfg.webhook.url), secret(ctx, "webhook_secret")?) else { return Ok(None) };
        Ok(Some(Self { ctx: ctx.clone(), url: url.to_owned(), secret }))
    }

    /// The JSON body for `msg`.
    pub async fn body(&self, msg: &MessageLog) -> Value {
        if let Some(alert_id) = &msg.alert_id
            && let Some(alert) = alert_json(self.ctx.db(), alert_id).await
        {
            return json!({ "type": "alert", "alert": alert, "text": msg.text });
        }
        json!({ "type": "message", "message": msg })
    }
}

/// `t=<unix>,v1=<hex HMAC-SHA256(secret, "<t>.<body>")>`.
pub fn signature(secret: &str, t: i64, body: &[u8]) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).expect("HMAC takes any key length");
    mac.update(t.to_string().as_bytes());
    mac.update(b".");
    mac.update(body);
    format!("t={t},v1={}", hex::encode(mac.finalize().into_bytes()))
}

/// The alert `id` as the API shows it, read from the alert engine's `alerts`
/// table column by column (JSON columns parsed), or `None` when there is no
/// such table or row or it doesn't read as an [`Alert`].
pub async fn alert_json(db: &SqlitePool, id: &str) -> Option<Value> {
    let row = sqlx::query("SELECT * FROM alerts WHERE id = ?").bind(id).fetch_optional(db).await.ok()??;
    let mut obj = Map::new();
    for col in row.columns() {
        let i = col.ordinal();
        let raw = row.try_get_raw(i).ok()?;
        let v = if raw.is_null() {
            Value::Null
        } else {
            match raw.type_info().name() {
                "INTEGER" => row.try_get::<i64, _>(i).map(Value::from).unwrap_or(Value::Null),
                "REAL" => row.try_get::<f64, _>(i).map(Value::from).unwrap_or(Value::Null),
                _ => match row.try_get::<String, _>(i) {
                    Ok(s) if s.starts_with('[') || s.starts_with('{') => serde_json::from_str(&s).unwrap_or(Value::String(s)),
                    Ok(s) => Value::String(s),
                    Err(_) => Value::Null,
                },
            }
        };
        if !v.is_null() {
            obj.insert(col.name().to_owned(), v);
        }
    }
    // The approval code lives only in the alert and the text (as `store::public` leaves it out of the API).
    if let Some(d) = obj.get_mut("data").and_then(Value::as_object_mut) {
        for k in crate::engine::store::PRIVATE_DATA {
            d.remove(*k);
        }
    }
    // A point kept as two columns.
    if !obj.contains_key("at")
        && let (Some(lon), Some(lat)) = (obj.get("at_lon").and_then(Value::as_f64), obj.get("at_lat").and_then(Value::as_f64))
    {
        obj.insert("at".into(), json!([lon, lat]));
    }
    let alert: Alert = serde_json::from_value(Value::Object(obj)).ok()?;
    serde_json::to_value(alert).ok()
}

#[async_trait::async_trait]
impl Channel for Webhook {
    fn kind(&self) -> &'static str {
        "webhook"
    }

    async fn send(&self, msg: &MessageLog) -> Result<Delivery, ChannelError> {
        let body = serde_json::to_vec(&self.body(msg).await).map_err(|e| ChannelError::Fail(e.to_string()))?;
        let sig = signature(&self.secret, chrono::Utc::now().timestamp(), &body);
        let res = http()
            .post(&self.url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(SIGNATURE_HEADER, sig)
            .body(body)
            .send()
            .await
            .map_err(|e| net_error("The webhook", &e))?;
        let code = res.status().as_u16();
        match code {
            200..=299 => Ok(Delivery::delivered(None)),
            408 | 429 | 500..=599 => Err(ChannelError::Retry(format!("The webhook answered {code}."))),
            _ => Err(ChannelError::Fail(format!("The webhook answered {code}."))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use op_core::Severity;
    use op_core::alert::AlertStatus;
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn the_signature_header_keeps_the_brand_lowercase() {
        assert_eq!(SIGNATURE_HEADER, "x-openpasture-signature");
        assert!(axum::http::HeaderName::from_static(SIGNATURE_HEADER).as_str() == SIGNATURE_HEADER);
    }

    #[test]
    fn signature_is_hmac_sha256_over_time_dot_body() {
        // Known answer: HMAC-SHA256("whsec", "1700000000.{}").
        let s = signature("whsec", 1_700_000_000, b"{}");
        assert!(s.starts_with("t=1700000000,v1="));
        let mut mac = Hmac::<Sha256>::new_from_slice(b"whsec").unwrap();
        mac.update(b"1700000000.{}");
        mac.verify_slice(&hex::decode(s.split("v1=").nth(1).unwrap()).unwrap()).unwrap();
    }

    #[tokio::test]
    async fn alerts_read_from_their_table_whatever_the_column_types() {
        let db = SqlitePoolOptions::new().connect("sqlite::memory:").await.unwrap();
        assert_eq!(alert_json(&db, "alr_1").await, None, "no table yet");
        sqlx::query(
            "CREATE TABLE alerts (id TEXT PRIMARY KEY, kind TEXT, key TEXT, severity TEXT, status TEXT, herd_id TEXT, title TEXT, body TEXT,
             at_lon REAL, at_lat REAL, targets TEXT, data TEXT, opened_at TEXT, updated_at TEXT, acked_at TEXT, acked_by TEXT,
             resolved_at TEXT, resolved_by TEXT, rolled_into TEXT, notified INTEGER)",
        )
        .execute(&db)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO alerts VALUES ('alr_1', 'outside', 'outside:col_1', 'warning', 'open', 'herd_1', '214 outside P3', NULL, -93.62, 42.03,
             '[[\"collar\",\"col_1\"]]', '{}', '2026-09-27T06:12:00.000Z', '2026-09-27T06:12:00.000Z', NULL, NULL, NULL, NULL, NULL, 1)",
        )
        .execute(&db)
        .await
        .unwrap();
        let v = alert_json(&db, "alr_1").await.expect("alert");
        let a: Alert = serde_json::from_value(v).unwrap();
        assert_eq!(a.title, "214 outside P3");
        assert_eq!(a.severity, Severity::Warning);
        assert_eq!(a.status, AlertStatus::Open);
        assert_eq!(a.at, Some([-93.62, 42.03]));
        assert_eq!(a.targets, vec![("collar".to_owned(), "col_1".to_owned())]);
        assert_eq!(alert_json(&db, "alr_2").await, None);
    }
}
