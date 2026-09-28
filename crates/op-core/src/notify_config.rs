//! Which notification channels can send right now: one reading of the
//! `notify.channels` setting (owned by op-alerts) and the secrets, shared by
//! routing, Settings and the sender.

use serde_json::Value;

use crate::Ctx;

/// The setting key op-alerts keeps its channel config under.
pub const CHANNELS_KEY: &str = "notify.channels";

/// Channels that can send now, in this order:
/// sms = `sms.from` + both Twilio secrets; whatsapp = `whatsapp.from` + Twilio
/// secrets; email = `email.host` + `email.from` (+ `smtp_password` when
/// `email.user` is set); webhook = `webhook.url` + `webhook_secret`; relay =
/// `relay.enabled` (set only after `GET {hosted_url}/v1/notify/recipients`
/// returned 200) + `hosted_api_key` (`hosted_url` defaults to the openpasture
/// service, as for the hosted brain); push = https + `push.enabled` + the VAPID
/// key + a subscription.
pub async fn configured_channels(ctx: &Ctx) -> anyhow::Result<Vec<&'static str>> {
    let cfg = ctx.store().get_setting_json(CHANNELS_KEY).await?.unwrap_or(Value::Null);
    let text = |path: &str| cfg.pointer(path).and_then(Value::as_str).is_some_and(|s| !s.trim().is_empty());
    let secret = |name: &str| -> anyhow::Result<bool> { Ok(ctx.secrets().get(name)?.is_some_and(|v| !v.trim().is_empty())) };
    let twilio = secret("twilio_account_sid")? && secret("twilio_auth_token")?;

    let mut out = Vec::new();
    if text("/sms/from") && twilio {
        out.push("sms");
    }
    if text("/whatsapp/from") && twilio {
        out.push("whatsapp");
    }
    if text("/email/host") && text("/email/from") && (!text("/email/user") || secret("smtp_password")?) {
        out.push("email");
    }
    if text("/webhook/url") && secret("webhook_secret")? {
        out.push("webhook");
    }
    if cfg.pointer("/relay/enabled").and_then(Value::as_bool) == Some(true) && secret("hosted_api_key")? {
        out.push("relay");
    }
    // @HUB
    // @HUB-UI
    // @E-lib
    // @E-srv
    // @J
    // @A-engine
    // @A-notify
    // @D
    // @K-animals
    // @K-files
    // @I
    // @B
    // @G
    // @P
    // @Q
    // @C
    // @F
    // @S
    // @A3
    // @H
    // @L
    // @M
    // push = served over https (the base URL, from settings or a tunnel) + `push.enabled`
    // (on unless set false) + the VAPID key (`vapid_private_key`, made on first use) + at
    // least one browser subscribed.
    let push = ctx.store().get_setting_json("push").await?.unwrap_or(Value::Null);
    if ctx.base_url().starts_with("https://") && push.pointer("/enabled").and_then(Value::as_bool) != Some(false) && secret("vapid_private_key")? {
        let any: Option<i64> = sqlx::query_scalar("SELECT 1 FROM push_subscriptions LIMIT 1").fetch_optional(ctx.db()).await?;
        if any.is_some() {
            out.push("push");
        }
    }
    // @Z
    Ok(out)
}
