//! The hosted relay: a farm server sending through an in-process hosting
//! server (its own data dir, its own Twilio-shaped receiver). Keys, hosting
//! off, unverified recipients, the code flow, caps, idempotency, the loop
//! guard, and Settings turning the relay on only after the host said 200.

mod notify_support;

use axum::http::StatusCode;
use notify_support::*;
use op_alerts::notify::sender::run_once;
use op_core::Ctx;
use op_core::messages::enqueue;
use op_core::notify_config::configured_channels;
use op_core::time::now;
use serde_json::{Value, json};

const PHONE: &str = "+15155550123";

async fn relay_farm(host: &Host) -> (tempfile::TempDir, Ctx) {
    let (dir, farm) = ctx().await;
    let body = json!({ "secrets": { "hosted_url": host.url, "hosted_api_key": host.key }, "relay": { "enabled": true } });
    let (s, v) = call(&app(&farm), "PUT", "/api/notify/channels", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    (dir, farm)
}

async fn post(host: &Host, auth: Option<&str>, path: &str, body: Value) -> (StatusCode, Value) {
    let headers: Vec<(&str, &str)> = auth.map(|a| vec![("authorization", a)]).unwrap_or_default();
    call_with(&app(&host.ctx), "POST", path, Some(body), &headers).await
}

fn notify(key: &str, to: &str, text: &str) -> Value {
    json!({ "idempotency_key": key, "channel": "sms", "to": to, "text": text })
}

#[tokio::test]
async fn keys_hosting_and_recipients_gate_every_send() {
    let host = Host::start().await;
    let auth = host.bearer();
    let (s, v) = post(&host, None, "/v1/notify", notify("k1", PHONE, "hi")).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("Missing key.")));
    let (s, v) = post(&host, Some("Bearer oph_0000"), "/v1/notify", notify("k1", PHONE, "hi")).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::UNAUTHORIZED, Some("Key not accepted.")));

    let (s, v) = post(&host, Some(&auth), "/v1/notify", notify("k1", PHONE, "hi")).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("That recipient isn't verified.")));

    call(&app(&host.ctx), "PUT", "/api/notify/hosting", Some(json!({ "enabled": false }))).await;
    for (path, body) in [("/v1/notify", notify("k1", PHONE, "hi")), ("/v1/notify/recipients", json!({ "channel": "sms", "to": PHONE }))] {
        let (s, v) = post(&host, Some(&auth), path, body).await;
        assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("This server doesn't relay texts.")), "{path}");
    }
    let (s, _) = call_with(&app(&host.ctx), "GET", "/v1/notify/recipients", None, &[("authorization", &auth)]).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    assert!(host.twilio.texts().is_empty());
}

#[tokio::test]
async fn recipients_prove_their_number_with_a_code() {
    let host = Host::start().await;
    let auth = host.bearer();
    let a = app(&host.ctx);
    let (s, v) = post(&host, Some(&auth), "/v1/notify/recipients", json!({ "channel": "sms", "to": "515-555-0123", "deadman": true })).await;
    assert_eq!((s, v), (StatusCode::ACCEPTED, json!({ "status": "sent" })));
    let form = host.twilio.texts()[0].form();
    assert_eq!(form["To"], PHONE);
    assert!(form["Body"].starts_with("openpasture code ") && form["Body"].ends_with("Reply STOP to opt out."));
    let code = last_code(&host.twilio);
    // The host's log masks the code.
    let log = messages(&host.ctx).await;
    assert_eq!(log[0].text, "openpasture code ******. Reply STOP to opt out.");

    let (s, v) = call_with(&a, "GET", "/v1/notify/recipients", None, &[("authorization", &auth)]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!([{ "channel": "sms", "to": PHONE, "deadman": true }]));

    let (s, v) = post(&host, Some(&auth), "/v1/notify/recipients", json!({ "channel": "sms", "to": PHONE })).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some("Wait a moment before asking for another code.")));

    let wrong = if code == "000000" { "111111" } else { "000000" };
    let (s, v) = post(&host, Some(&auth), "/v1/notify/recipients/verify", json!({ "channel": "sms", "to": PHONE, "code": wrong })).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("That code isn't right.")));
    let (s, v) = post(&host, Some(&auth), "/v1/notify/recipients/verify", json!({ "channel": "sms", "to": PHONE, "code": code })).await;
    assert_eq!((s, v), (StatusCode::OK, json!({ "verified": true })));
    let (_, v) = call_with(&a, "GET", "/v1/notify/recipients", None, &[("authorization", &auth)]).await;
    assert!(v[0]["verified_at"].is_string());

    // Asking again for a verified number texts nothing and keeps it verified.
    let (s, v) = post(&host, Some(&auth), "/v1/notify/recipients", json!({ "channel": "sms", "to": PHONE, "deadman": false })).await;
    assert_eq!((s, v), (StatusCode::ACCEPTED, json!({ "status": "verified" })));
    assert_eq!(host.twilio.texts().len(), 1);

    // Unknown number, expiry and the 5-try limit.
    let (s, _) = post(&host, Some(&auth), "/v1/notify/recipients/verify", json!({ "channel": "sms", "to": "+15155550777", "code": "123456" })).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    post(&host, Some(&auth), "/v1/notify/recipients", json!({ "channel": "sms", "to": "+15155550777" })).await;
    let code = last_code(&host.twilio);
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for _ in 0..5 {
        post(&host, Some(&auth), "/v1/notify/recipients/verify", json!({ "channel": "sms", "to": "+15155550777", "code": wrong })).await;
    }
    let (s, _) = post(&host, Some(&auth), "/v1/notify/recipients/verify", json!({ "channel": "sms", "to": "+15155550777", "code": code })).await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS);
    let old = op_core::time::to_db(&(now() - chrono::Duration::minutes(11)));
    sqlx::query("UPDATE notify_recipients SET sent_at = ?, attempts = 0 WHERE address = '+15155550777'").bind(&old).execute(host.ctx.db()).await.unwrap();
    let (s, _) = post(&host, Some(&auth), "/v1/notify/recipients/verify", json!({ "channel": "sms", "to": "+15155550777", "code": code })).await;
    assert_eq!(s, StatusCode::GONE);

    // Another key sees none of this key's recipients.
    let (_, other) = op_brain::hosted::create_key(&host.ctx, "other farm").await.unwrap();
    let (_, v) = call_with(&a, "GET", "/v1/notify/recipients", None, &[("authorization", &format!("Bearer {other}"))]).await;
    assert_eq!(v, json!([]));
}

#[tokio::test]
async fn a_farm_text_goes_out_once_from_the_hosts_number_even_when_retried() {
    let host = Host::start().await;
    host.verify_recipient("sms", PHONE).await;
    let codes = host.twilio.texts().len();
    let (_d, farm) = relay_farm(&host).await;
    assert!(configured_channels(&farm).await.unwrap().contains(&"relay"));

    let id = enqueue(&farm, out("alert:alr_1:usr_1", "relay", PHONE, "214 outside P3, 6m. Reply OK to ack")).await.unwrap().id;
    assert_eq!(run_once(&farm, now()).await.unwrap(), 1);
    let m = message(&farm, &id).await;
    assert_eq!(m.status, "sent", "{:?}", m.error);
    let host_id = m.provider_id.clone().expect("the host's message id");

    // On the host: queued under the farm's message id, then sent from its own number.
    let queued = message(&host.ctx, &host_id).await;
    assert_eq!((queued.channel.as_str(), queued.address.as_str(), queued.kind.as_str()), ("sms", PHONE, "alert"));
    assert_eq!(run_once(&host.ctx, now()).await.unwrap(), 1);
    let texts = host.twilio.texts();
    assert_eq!(texts.len(), codes + 1);
    let form = texts.last().unwrap().form();
    assert_eq!(form["Body"], "214 outside P3, 6m. Reply OK to ack");
    assert_eq!(form["From"], FROM);

    // The farm lost the answer and sends again: the host says duplicate and sends nothing.
    let auth = host.bearer();
    let (s, v) = post(&host, Some(&auth), "/v1/notify", notify(&id, PHONE, "214 outside P3, 6m. Reply OK to ack")).await;
    assert_eq!((s, v), (StatusCode::ACCEPTED, json!({ "id": host_id, "status": "duplicate" })));
    op_core::messages::mark(&farm, &id, "queued", None, Some("lost the answer"), None).await.unwrap();
    run_once(&farm, now()).await.unwrap();
    let m = message(&farm, &id).await;
    assert_eq!((m.status.as_str(), m.provider_id.as_deref()), ("sent", Some(host_id.as_str())));
    assert_eq!(run_once(&host.ctx, now()).await.unwrap(), 0);
    assert_eq!(host.twilio.texts().len(), codes + 1, "sent once");

    // The same idempotency key from another key is another message.
    let (_, other) = op_brain::hosted::create_key(&host.ctx, "other").await.unwrap();
    let (s, v) = post(&host, Some(&format!("Bearer {other}")), "/v1/notify", notify(&id, PHONE, "x")).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::FORBIDDEN, Some("That recipient isn't verified.")));
}

#[tokio::test]
async fn caps_per_minute_and_per_day_answer_429() {
    let host = Host::start().await;
    host.verify_recipient("sms", PHONE).await;
    let auth = host.bearer();
    // The code text counted as one.
    call(&app(&host.ctx), "PUT", "/api/notify/hosting", Some(json!({ "per_key_minute": 3 }))).await;
    for i in 0..2 {
        let (s, v) = post(&host, Some(&auth), "/v1/notify", notify(&format!("k{i}"), PHONE, "hi")).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    }
    let (s, v) = post(&host, Some(&auth), "/v1/notify", notify("k2", PHONE, "hi")).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some("Too many texts this minute. Try again shortly.")));
    // A retry of an accepted message is still a duplicate, not a 429.
    let (s, v) = post(&host, Some(&auth), "/v1/notify", notify("k0", PHONE, "hi")).await;
    assert_eq!((s, v["status"].as_str()), (StatusCode::ACCEPTED, Some("duplicate")));

    call(&app(&host.ctx), "PUT", "/api/notify/hosting", Some(json!({ "per_key_minute": 100, "per_key_day": 4 }))).await;
    let (s, _) = post(&host, Some(&auth), "/v1/notify", notify("k3", PHONE, "hi")).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    let (s, v) = post(&host, Some(&auth), "/v1/notify", notify("k4", PHONE, "hi")).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some("The daily text limit for this key is reached.")));
    let (day, count): (String, i64) =
        sqlx::query_as("SELECT day, count FROM notify_usage WHERE key_id = ?").bind(&host.key_id).fetch_one(host.ctx.db()).await.unwrap();
    assert_eq!(day, now().format("%Y-%m-%d").to_string());
    assert_eq!(count, 4);

    // The farm side keeps a 429 queued for later instead of failing it.
    let (_d, farm) = relay_farm(&host).await;
    let t0 = now();
    let id = enqueue(&farm, out("k5", "relay", PHONE, "hi")).await.unwrap().id;
    run_once(&farm, t0).await.unwrap();
    let m = message(&farm, &id).await;
    assert_eq!((m.status.as_str(), m.error.as_deref()), ("queued", Some("The daily text limit for this key is reached.")));
    assert_eq!(next_attempt_at(&farm, &id).await, Some(t0 + chrono::Duration::seconds(5)));
}

#[tokio::test]
async fn a_server_that_sends_through_a_relay_itself_doesnt_relay() {
    let upstream = Host::start().await;
    let host = Host::start().await;
    host.verify_recipient("sms", PHONE).await;
    // The host drops its own Twilio and turns its relay on instead.
    let body = json!({
        "secrets": { "twilio_auth_token": null, "hosted_url": upstream.url, "hosted_api_key": upstream.key },
        "relay": { "enabled": true },
    });
    let (s, v) = call(&app(&host.ctx), "PUT", "/api/notify/channels", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(configured_channels(&host.ctx).await.unwrap(), vec!["relay"]);
    let (s, v) = post(&host, Some(&host.bearer()), "/v1/notify", notify("k1", PHONE, "hi")).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::CONFLICT, Some("This server sends through a relay itself, so it can't relay.")));

    // Without any channel it just can't send.
    call(&app(&host.ctx), "PUT", "/api/notify/channels", Some(json!({ "relay": { "enabled": false } }))).await;
    let (s, _) = post(&host, Some(&host.bearer()), "/v1/notify", notify("k1", PHONE, "hi")).await;
    assert_eq!(s, StatusCode::SERVICE_UNAVAILABLE);
}

#[tokio::test]
async fn settings_turn_the_relay_on_only_after_the_host_answers_200() {
    let host = Host::start().await;
    let (_d, farm) = ctx().await;
    let a = app(&farm);

    let (s, v) = call(&a, "PUT", "/api/notify/channels", Some(json!({ "relay": { "enabled": true } }))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("Add the relay key first.")));

    // A key the host doesn't know.
    let body = json!({ "secrets": { "hosted_url": host.url, "hosted_api_key": "oph_notakey" }, "relay": { "enabled": true } });
    let (s, v) = call(&a, "PUT", "/api/notify/channels", Some(body)).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("The relay didn't accept the key.")));
    assert!(!configured_channels(&farm).await.unwrap().contains(&"relay"));

    // The right key while the host doesn't relay.
    call(&host_app(&host), "PUT", "/api/notify/hosting", Some(json!({ "enabled": false }))).await;
    let body = json!({ "secrets": { "hosted_api_key": host.key }, "relay": { "enabled": true } });
    let (s, v) = call(&a, "PUT", "/api/notify/channels", Some(body)).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("This server doesn't relay texts.")));
    let (_, v) = call(&a, "GET", "/api/notify/channels", None).await;
    assert_eq!(v["relay"], json!({ "enabled": false }));
    assert_eq!(v["configured"], json!([]));

    call(&host_app(&host), "PUT", "/api/notify/hosting", Some(json!({ "enabled": true }))).await;
    let (s, v) = call(&a, "PUT", "/api/notify/channels", Some(json!({ "relay": { "enabled": true } }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["relay"]["enabled"], true);
    assert!(v["relay"]["checked_at"].is_string());
    assert_eq!(v["configured"], json!(["relay"]));

    // The Test button lists the host's verified recipients.
    host.verify_recipient("sms", PHONE).await;
    let (_, v) = call(&a, "POST", "/api/notify/test", Some(json!({ "channel": "relay" }))).await;
    assert_eq!(v, json!({ "ok": true, "detail": "1 verified recipient" }));

    // A new key while on is asked again; a refusal turns the relay off.
    let body = json!({ "secrets": { "hosted_api_key": "oph_revoked" } });
    let (s, _) = call(&a, "PUT", "/api/notify/channels", Some(body)).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!configured_channels(&farm).await.unwrap().contains(&"relay"));

    // Off is off.
    let body = json!({ "secrets": { "hosted_api_key": host.key }, "relay": { "enabled": true } });
    assert_eq!(call(&a, "PUT", "/api/notify/channels", Some(body)).await.0, StatusCode::OK);
    let (_, v) = call(&a, "PUT", "/api/notify/channels", Some(json!({ "relay": { "enabled": false } }))).await;
    assert_eq!(v["configured"], json!([]));
}

fn host_app(host: &Host) -> axum::Router {
    app(&host.ctx)
}

#[tokio::test]
async fn hosting_settings_have_defaults_and_bounds() {
    let (_d, ctx) = ctx().await;
    let a = app(&ctx);
    let (s, v) = call(&a, "GET", "/api/notify/hosting", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({ "enabled": false, "per_key_minute": 30, "per_key_day": 500, "deadman_after_min": 15 }));
    let (s, v) = call(&a, "PUT", "/api/notify/hosting", Some(json!({ "enabled": true, "per_key_day": 800 }))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({ "enabled": true, "per_key_minute": 30, "per_key_day": 800, "deadman_after_min": 15 }));
    for bad in [json!({ "per_key_minute": 0 }), json!({ "per_key_day": 0 }), json!({ "deadman_after_min": 1 }), json!({ "enabled": "yes" })] {
        let (s, _) = call(&a, "PUT", "/api/notify/hosting", Some(bad.clone())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad}");
    }
}
