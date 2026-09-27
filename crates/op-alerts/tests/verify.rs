//! Phone verification: the code text and its opt-out line, 5 tries, 10-minute
//! codes, `phone_verified_at`, a phone change clearing it, and verification
//! through the hosted relay.

mod notify_support;

use axum::http::StatusCode;
use notify_support::*;
use op_core::users::{self, NewUser, UserPatch};
use op_core::{Ctx, Role};
use serde_json::json;

const PHONE: &str = "+15155550123";

async fn person(ctx: &Ctx, phone: Option<&str>, role: Role) -> String {
    users::create_user(ctx, NewUser { name: "Hank".into(), role, phone: phone.map(Into::into), email: None }).await.unwrap().id
}

async fn verified_at(ctx: &Ctx, id: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    users::get_user(ctx, id).await.unwrap().unwrap().phone_verified_at
}

#[tokio::test]
async fn the_code_is_texted_with_the_stop_line_and_verifies_the_phone() {
    let (_d, ctx) = ctx().await;
    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, None).await;
    let hank = person(&ctx, Some(PHONE), Role::Hand).await;
    let a = app(&ctx);

    let (s, v) = call(&a, "POST", "/api/notify/verify", Some(json!({ "user_id": hank }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({ "via": "sms" }));
    let texts = twilio.texts();
    assert_eq!(texts.len(), 1);
    let form = texts[0].form();
    assert_eq!(form["To"], PHONE);
    let body = &form["Body"];
    assert!(body.starts_with("openpasture code "), "{body}");
    assert!(body.ends_with(". Reply STOP to opt out."), "{body}");
    assert!(op_alerts::notify::is_gsm7(body) && body.len() <= 160);
    let code = last_code(&twilio);

    // The log keeps the text with the code masked, and the code is only hashed.
    let log = messages(&ctx).await;
    assert_eq!(log.len(), 1);
    assert_eq!((log[0].kind.as_str(), log[0].status.as_str(), log[0].user_id.as_deref()), ("verify", "sent", Some(hank.as_str())));
    assert_eq!(log[0].text, "openpasture code ******. Reply STOP to opt out.");
    let (hash,): (String,) = sqlx::query_as("SELECT code_hash FROM phone_codes WHERE user_id = ?").bind(&hank).fetch_one(ctx.db()).await.unwrap();
    assert!(!hash.contains(&code));
    assert_eq!(hash.len(), 64);

    assert_eq!(verified_at(&ctx, &hank).await, None);
    let wrong = if code == "000000" { "111111" } else { "000000" };
    let (s, v) = call(&a, "POST", "/api/notify/verify/confirm", Some(json!({ "user_id": hank, "code": wrong }))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("That code isn't right.")));
    let (s, v) = call(&a, "POST", "/api/notify/verify/confirm", Some(json!({ "user_id": hank, "code": format!(" {} {} ", &code[..3], &code[3..]) }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["verified"], true);
    assert!(verified_at(&ctx, &hank).await.is_some());
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM phone_codes").fetch_one(ctx.db()).await.unwrap();
    assert_eq!(n, 0, "a used code is forgotten");

    // Verified: no second code.
    let (s, v) = call(&a, "POST", "/api/notify/verify", Some(json!({ "user_id": hank }))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::CONFLICT, Some("That phone is already verified.")));

    // Changing the phone clears it.
    users::update_user(&ctx, &hank, UserPatch { phone: Some(Some("+15155550999".into())), ..Default::default() }).await.unwrap();
    assert_eq!(verified_at(&ctx, &hank).await, None);
}

#[tokio::test]
async fn five_wrong_tries_use_up_the_code() {
    let (_d, ctx) = ctx().await;
    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, None).await;
    let hank = person(&ctx, Some(PHONE), Role::Hand).await;
    let a = app(&ctx);
    call(&a, "POST", "/api/notify/verify", Some(json!({ "user_id": hank }))).await;
    let code = last_code(&twilio);
    let wrong = if code == "000000" { "111111" } else { "000000" };
    for _ in 0..5 {
        let (s, _) = call(&a, "POST", "/api/notify/verify/confirm", Some(json!({ "user_id": hank, "code": wrong }))).await;
        assert_eq!(s, StatusCode::BAD_REQUEST);
    }
    let (s, v) = call(&a, "POST", "/api/notify/verify/confirm", Some(json!({ "user_id": hank, "code": code }))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some("Too many tries. Send a new code.")));
    assert_eq!(verified_at(&ctx, &hank).await, None);
}

#[tokio::test]
async fn codes_expire_after_ten_minutes_and_resends_wait_30s() {
    let (_d, ctx) = ctx().await;
    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, None).await;
    let hank = person(&ctx, Some(PHONE), Role::Hand).await;
    let a = app(&ctx);
    call(&a, "POST", "/api/notify/verify", Some(json!({ "user_id": hank }))).await;
    let (s, v) = call(&a, "POST", "/api/notify/verify", Some(json!({ "user_id": hank }))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::TOO_MANY_REQUESTS, Some("Wait a moment before sending another code.")));
    assert_eq!(twilio.texts().len(), 1);

    let code = last_code(&twilio);
    let old = op_core::time::to_db(&(op_core::time::now() - chrono::Duration::minutes(11)));
    sqlx::query("UPDATE phone_codes SET sent_at = ?").bind(&old).execute(ctx.db()).await.unwrap();
    let (s, v) = call(&a, "POST", "/api/notify/verify/confirm", Some(json!({ "user_id": hank, "code": code }))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::GONE, Some("That code has expired. Send a new one.")));

    // A new code works (and the old one is gone).
    let (s, _) = call(&a, "POST", "/api/notify/verify", Some(json!({ "user_id": hank }))).await;
    assert_eq!(s, StatusCode::OK);
    let fresh = last_code(&twilio);
    let (s, _) = call(&a, "POST", "/api/notify/verify/confirm", Some(json!({ "user_id": hank, "code": fresh }))).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn a_code_sent_to_the_old_phone_never_verifies_the_new_one() {
    let (_d, ctx) = ctx().await;
    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, None).await;
    let hank = person(&ctx, Some(PHONE), Role::Hand).await;
    let a = app(&ctx);
    call(&a, "POST", "/api/notify/verify", Some(json!({ "user_id": hank }))).await;
    let code = last_code(&twilio);
    users::update_user(&ctx, &hank, UserPatch { phone: Some(Some("+15155550999".into())), ..Default::default() }).await.unwrap();
    let (s, _) = call(&a, "POST", "/api/notify/verify/confirm", Some(json!({ "user_id": hank, "code": code }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(verified_at(&ctx, &hank).await, None);
}

#[tokio::test]
async fn verification_needs_a_phone_and_a_way_to_text() {
    let (_d, ctx) = ctx().await;
    let a = app(&ctx);
    let nophone = person(&ctx, None, Role::Hand).await;
    let (s, v) = call(&a, "POST", "/api/notify/verify", Some(json!({ "user_id": nophone }))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("Add a phone number first.")));
    let hank = users::create_user(&ctx, NewUser { name: "Hank".into(), role: Role::Hand, phone: Some(PHONE.into()), email: None }).await.unwrap().id;
    let (s, v) = call(&a, "POST", "/api/notify/verify", Some(json!({ "user_id": hank }))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::CONFLICT, Some("Set up Twilio or the relay first.")));
    let (s, _) = call(&a, "POST", "/api/notify/verify", Some(json!({ "user_id": "usr_nobody" }))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, v) = call(&a, "POST", "/api/notify/verify/confirm", Some(json!({ "user_id": hank, "code": "123456" }))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("Send a code first.")));

    // Twilio refusing the number: 502 with its words, and the code isn't kept.
    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, None).await;
    twilio.reply(400, json!({ "code": 21610, "message": "Attempt to send to unsubscribed recipient" }));
    let (s, v) = call(&a, "POST", "/api/notify/verify", Some(json!({ "user_id": hank }))).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::BAD_GATEWAY, Some("Twilio 21610: Attempt to send to unsubscribed recipient")));
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM phone_codes").fetch_one(ctx.db()).await.unwrap();
    assert_eq!(n, 0);
    assert_eq!(messages(&ctx).await[0].status, "failed");
}

#[tokio::test]
async fn with_only_the_relay_the_host_texts_and_checks_the_code() {
    let host = Host::start().await;
    let (_d, farm) = ctx().await;
    let a = app(&farm);
    let body = json!({ "secrets": { "hosted_url": host.url, "hosted_api_key": host.key }, "relay": { "enabled": true } });
    let (s, v) = call(&a, "PUT", "/api/notify/channels", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["configured"], json!(["relay"]));

    let boss = person(&farm, Some(PHONE), Role::Owner).await;
    let (s, v) = call(&a, "POST", "/api/notify/verify", Some(json!({ "user_id": boss }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v, json!({ "via": "relay" }));
    // The host's own Twilio texted the code, with the opt-out line.
    let form = host.twilio.texts()[0].form();
    assert_eq!(form["To"], PHONE);
    assert!(form["Body"].ends_with("Reply STOP to opt out."));
    let code = last_code(&host.twilio);
    assert_eq!(messages(&farm).await[0].text, "openpasture code ******. Reply STOP to opt out.");

    let (s, v) = call(&a, "POST", "/api/notify/verify/confirm", Some(json!({ "user_id": boss, "code": code }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(verified_at(&farm, &boss).await.is_some());
    let (verified, deadman): (Option<String>, i64) =
        sqlx::query_as("SELECT verified_at, deadman FROM notify_recipients WHERE address = ?").bind(PHONE).fetch_one(host.ctx.db()).await.unwrap();
    assert!(verified.is_some());
    assert_eq!(deadman, 1, "owners hear when the farm server goes quiet");
}
