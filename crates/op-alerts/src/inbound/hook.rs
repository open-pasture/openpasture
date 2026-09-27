//! `POST /hooks/twilio/sms` and `POST /hooks/twilio/whatsapp`: Twilio's
//! webhook for a text to the farm's number, with a public URL.
//!
//! Twilio signs each request: `X-Twilio-Signature` is base64(HMAC-SHA1(auth
//! token, URL + every POST parameter name and value, sorted by name)), where
//! URL is what Twilio asked for. Behind a proxy the Host this server sees
//! isn't that, so the URL is always `server.public_url` + path + query. A
//! bad or missing signature is 403; so is a request while texting in is off
//! or there is no public URL (without one, texts come in by polling). A good
//! one is 204 at once; the reply goes out through the channel.

use axum::Router;
use axum::body::Bytes;
use axum::extract::{OriginalUri, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use op_core::messages::Inbound;
use op_core::time::now;
use op_core::{ApiError, Ctx};
use sha1::Sha1;

pub fn router() -> Router<Ctx> {
    Router::new().route("/hooks/twilio/sms", post(sms)).route("/hooks/twilio/whatsapp", post(whatsapp))
}

async fn sms(State(ctx): State<Ctx>, OriginalUri(uri): OriginalUri, headers: HeaderMap, body: Bytes) -> Response {
    take(&ctx, "sms", &uri, &headers, &body).await
}

async fn whatsapp(State(ctx): State<Ctx>, OriginalUri(uri): OriginalUri, headers: HeaderMap, body: Bytes) -> Response {
    take(&ctx, "whatsapp", &uri, &headers, &body).await
}

fn mac(auth_token: &str, url: &str, params: &[(String, String)]) -> Hmac<Sha1> {
    let mut sorted = params.to_vec();
    sorted.sort();
    let mut mac = Hmac::<Sha1>::new_from_slice(auth_token.as_bytes()).expect("HMAC takes any key");
    mac.update(url.as_bytes());
    for (k, v) in &sorted {
        mac.update(k.as_bytes());
        mac.update(v.as_bytes());
    }
    mac
}

/// base64(HMAC-SHA1(token, url + name-value pairs sorted by name)).
pub fn signature(auth_token: &str, url: &str, params: &[(String, String)]) -> String {
    base64::engine::general_purpose::STANDARD.encode(mac(auth_token, url, params).finalize().into_bytes())
}

/// The URL as given, and with the scheme's default port added or taken away
/// (Twilio's own helpers accept both forms).
fn url_forms(url: &str) -> Vec<String> {
    let mut out = vec![url.to_owned()];
    let Some((scheme, rest)) = url.split_once("://") else { return out };
    let port = match scheme {
        "https" => ":443",
        "http" => ":80",
        _ => return out,
    };
    let (host, path) = rest.find('/').map_or((rest, ""), |i| rest.split_at(i));
    let alt = match host.strip_suffix(port) {
        Some(bare) => format!("{scheme}://{bare}{path}"),
        None if !host.contains(':') => format!("{scheme}://{host}{port}{path}"),
        None => return out,
    };
    out.push(alt);
    out
}

/// Whether `given` is Twilio's signature of this request (compared in constant time).
pub fn verify(auth_token: &str, url: &str, params: &[(String, String)], given: &str) -> bool {
    let Ok(given) = base64::engine::general_purpose::STANDARD.decode(given.trim()) else { return false };
    url_forms(url).iter().any(|u| mac(auth_token, u, params).verify_slice(&given).is_ok())
}

fn refuse(msg: &str) -> Response {
    ApiError::forbidden(msg).into_response()
}

async fn take(ctx: &Ctx, channel: &str, uri: &axum::http::Uri, headers: &HeaderMap, body: &[u8]) -> Response {
    match check_and_take(ctx, channel, uri, headers, body).await {
        Ok(r) => r,
        Err(e) => ApiError::from(e).into_response(),
    }
}

async fn check_and_take(ctx: &Ctx, channel: &str, uri: &axum::http::Uri, headers: &HeaderMap, body: &[u8]) -> anyhow::Result<Response> {
    if !super::load(ctx).await?.inbound {
        return Ok(refuse("Texts in are off."));
    }
    let Some(public) = super::public_url(ctx).await? else {
        return Ok(refuse("This server has no public URL, so it checks Twilio for texts itself."));
    };
    let Some(token) = crate::notify::secret(ctx, "twilio_auth_token")? else { return Ok(refuse("Twilio isn't set up.")) };
    let Some(given) = headers.get("x-twilio-signature").and_then(|v| v.to_str().ok()) else { return Ok(refuse("Signature missing.")) };
    let params: Vec<(String, String)> = form_urlencoded::parse(body).into_owned().collect();
    let url = match uri.query() {
        Some(q) => format!("{public}{}?{q}", uri.path()),
        None => format!("{public}{}", uri.path()),
    };
    if !verify(&token, &url, &params, given) {
        tracing::warn!(path = %uri.path(), "a Twilio webhook with a bad signature");
        return Ok(refuse("Signature doesn't match."));
    }
    let get = |name: &str| params.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());
    // Signed with our token, but for another account: not ours to act on.
    if let (Some(sid), Some(ours)) = (get("AccountSid"), crate::notify::secret(ctx, "twilio_account_sid")?)
        && sid != ours
    {
        return Ok(refuse("That's another Twilio account."));
    }
    let (Some(from), Some(sid)) = (get("From"), get("MessageSid").or_else(|| get("SmsMessageSid")).or_else(|| get("SmsSid"))) else {
        return Ok((StatusCode::BAD_REQUEST, axum::Json(serde_json::json!({ "error": "From and MessageSid are required." }))).into_response());
    };
    let inbound = Inbound { channel: channel.to_owned(), from, text: get("Body").unwrap_or_default(), provider_id: Some(sid), at: now() };
    super::receive(ctx, inbound).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Twilio's documented example (auth token 12345); the value is also what
    /// `openssl dgst -sha1 -hmac 12345 -binary | base64` gives.
    #[test]
    fn twilios_example_signature() {
        let params: Vec<(String, String)> =
            [("CallSid", "CA1234567890ABCDE"), ("Caller", "+12349013030"), ("Digits", "1234"), ("From", "+12349013030"), ("To", "+18005551212")]
                .iter()
                .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
                .collect();
        let url = "https://mycompany.com/myapp.php?foo=1&bar=2";
        assert_eq!(signature("12345", url, &params), "0/KCTR6DLpKmkAf8muzZqo1nDgQ=");
        assert!(verify("12345", url, &params, "0/KCTR6DLpKmkAf8muzZqo1nDgQ="));
        assert!(!verify("12345", "https://mycompany.com/myapp.php?foo=1", &params, "0/KCTR6DLpKmkAf8muzZqo1nDgQ="));
        assert!(!verify("54321", url, &params, "0/KCTR6DLpKmkAf8muzZqo1nDgQ="));
        assert!(!verify("12345", url, &params, "not base64 !"));
    }

    #[test]
    fn default_ports_are_tried_both_ways() {
        assert_eq!(
            url_forms("https://farm.example/hooks/twilio/sms?x=1"),
            vec!["https://farm.example/hooks/twilio/sms?x=1", "https://farm.example:443/hooks/twilio/sms?x=1"]
        );
        assert_eq!(url_forms("https://farm.example:443/h"), vec!["https://farm.example:443/h", "https://farm.example/h"]);
        assert_eq!(url_forms("http://10.0.0.2:7878/h"), vec!["http://10.0.0.2:7878/h"]);
    }
}
