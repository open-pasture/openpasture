//! Channels and the sender, against in-test Twilio-shaped, webhook and SMTP
//! servers: request shapes, retries and error words, delivery-status polling,
//! each message claimed once, the message log and Settings > Texting's API.

mod notify_support;

use std::collections::HashSet;
use std::time::Duration;

use axum::http::StatusCode;
use base64::Engine;
use chrono::Duration as Span;
use hmac::{Hmac, Mac};
use notify_support::*;
use op_alerts::notify::sender::{self, poll_statuses, run_once};
use op_core::messages::{self, enqueue};
use op_core::notify_config::configured_channels;
use op_core::time::now;
use op_core::{Ctx, Event};
use serde_json::{Value, json};
use sha2::Sha256;

const TO: &str = "+15155550123";

async fn queued(ctx: &Ctx, key: &str, channel: &str, to: &str, text: &str) -> String {
    enqueue(ctx, out(key, channel, to, text)).await.unwrap().id
}

#[tokio::test]
async fn sms_posts_twilios_form_with_basic_auth_and_is_read_back_later() {
    let (_d, ctx) = ctx().await;
    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, None).await;
    let mut rx = ctx.subscribe();
    let id = queued(&ctx, "a1", "sms", TO, "214 outside P3, 200 ft N of east gate, 6m. Reply OK to ack").await;

    assert_eq!(run_once(&ctx, now()).await.unwrap(), 1);
    let texts = twilio.texts();
    assert_eq!(texts.len(), 1);
    let hit = &texts[0];
    assert_eq!(hit.path, format!("/2010-04-01/Accounts/{SID}/Messages.json"));
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{SID}:{TOKEN}"));
    assert_eq!(hit.header("authorization"), Some(format!("Basic {basic}")));
    assert!(hit.header("content-type").unwrap().starts_with("application/x-www-form-urlencoded"));
    let form = hit.form();
    assert_eq!(form["To"], TO);
    assert_eq!(form["From"], FROM);
    assert_eq!(form["Body"], "214 outside P3, 200 ft N of east gate, 6m. Reply OK to ack");
    assert!(!form.contains_key("ContentSid"));

    let m = message(&ctx, &id).await;
    assert_eq!(m.status, "sent");
    assert_eq!(m.attempts, 1);
    assert!(m.provider_id.as_deref().unwrap().starts_with("SM"));
    let poll = next_attempt_at(&ctx, &id).await.expect("a status check is due");
    let after = (poll - m.updated_at).num_seconds();
    assert!((29..=31).contains(&after), "first read-back {after} s after sending");

    // queued (enqueue), then sent; the claim itself isn't published.
    let mut seen = vec![];
    while let Ok(ev) = rx.try_recv() {
        if let Event::Message { message } = ev {
            seen.push(message.status);
        }
    }
    assert_eq!(seen, vec!["queued", "sent"]);
}

#[tokio::test]
async fn sms_from_a_messaging_service_sends_its_sid() {
    let (_d, ctx) = ctx().await;
    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, None).await;
    let mg = "MG0123456789abcdef0123456789abcdef";
    let (s, v) = call(&app(&ctx), "PUT", "/api/notify/channels", Some(json!({ "sms": { "from": mg } }))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    queued(&ctx, "a1", "sms", TO, "hello").await;
    run_once(&ctx, now()).await.unwrap();
    let form = twilio.texts()[0].form();
    assert_eq!(form["MessagingServiceSid"], mg);
    assert!(!form.contains_key("From"));
}

#[tokio::test]
async fn whatsapp_alerts_use_the_template_and_replies_plain_text() {
    let (_d, ctx) = ctx().await;
    let twilio = Receiver::start().await;
    let template = "HX0123456789abcdef0123456789abcdef";
    setup_twilio(&ctx, &twilio, Some(json!({ "from": "whatsapp:+15155550199", "template_sid": template }))).await;
    assert!(configured_channels(&ctx).await.unwrap().contains(&"whatsapp"));

    queued(&ctx, "a1", "whatsapp", TO, "31 outside P3 since 06:12. Reply OK to ack").await;
    let mut reply = out("r1", "whatsapp", TO, "214 is in P3, 3m ago.");
    reply.kind = "reply".into();
    enqueue(&ctx, reply).await.unwrap();
    assert_eq!(run_once(&ctx, now()).await.unwrap(), 2);

    let texts = twilio.texts();
    assert_eq!(texts.len(), 2);
    let alert = texts.iter().map(|h| h.form()).find(|f| f.contains_key("ContentSid")).expect("template send");
    assert_eq!(alert["To"], format!("whatsapp:{TO}"));
    assert_eq!(alert["From"], "whatsapp:+15155550199");
    assert_eq!(alert["ContentSid"], template);
    let vars: Value = serde_json::from_str(&alert["ContentVariables"]).unwrap();
    assert_eq!(vars, json!({ "1": "31 outside P3 since 06:12. Reply OK to ack" }));
    assert!(!alert.contains_key("Body"));
    let plain = texts.iter().map(|h| h.form()).find(|f| f.contains_key("Body")).expect("reply send");
    assert_eq!(plain["Body"], "214 is in P3, 3m ago.");
    assert_eq!(plain["To"], format!("whatsapp:{TO}"));
}

#[tokio::test]
async fn twilio_429_and_5xx_retry_at_5s_30s_2min_then_give_up() {
    let (_d, ctx) = ctx().await;
    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, None).await;
    twilio.reply(429, json!({ "code": 20429, "message": "Too Many Requests", "status": 429 }));
    twilio.reply(503, json!({}));
    twilio.reply(500, json!({}));
    twilio.reply(503, json!({ "code": 20500, "message": "Internal Server Error" }));
    let id = queued(&ctx, "a1", "sms", TO, "214 outside P3").await;

    let t0 = now();
    run_once(&ctx, t0).await.unwrap();
    let m = message(&ctx, &id).await;
    assert_eq!((m.status.as_str(), m.attempts), ("queued", 1));
    assert_eq!(m.error.as_deref(), Some("Twilio 20429: Too Many Requests"));
    assert_eq!(next_attempt_at(&ctx, &id).await, Some(t0 + Span::seconds(5)));

    // Not due yet: nothing is claimed.
    assert_eq!(run_once(&ctx, t0 + Span::seconds(4)).await.unwrap(), 0);
    assert_eq!(twilio.texts().len(), 1);

    let t1 = t0 + Span::seconds(5);
    run_once(&ctx, t1).await.unwrap();
    assert_eq!(next_attempt_at(&ctx, &id).await, Some(t1 + Span::seconds(30)));
    let t2 = t1 + Span::seconds(30);
    run_once(&ctx, t2).await.unwrap();
    assert_eq!(next_attempt_at(&ctx, &id).await, Some(t2 + Span::seconds(120)));
    run_once(&ctx, t2 + Span::seconds(120)).await.unwrap();

    let m = message(&ctx, &id).await;
    assert_eq!(m.status, "failed");
    assert_eq!(m.attempts, 4);
    assert_eq!(m.error.as_deref(), Some("Twilio 20500: Internal Server Error Gave up after 4 tries."));
    assert_eq!(twilio.texts().len(), 4);
    assert_eq!(run_once(&ctx, t2 + Span::hours(1)).await.unwrap(), 0);
}

#[tokio::test]
async fn twilio_4xx_fails_at_once_with_twilios_words() {
    let (_d, ctx) = ctx().await;
    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, None).await;
    twilio.reply(400, json!({ "code": 21211, "message": "The 'To' number +1555 is not a valid phone number.", "status": 400 }));
    let id = queued(&ctx, "a1", "sms", "+1555", "hi").await;
    run_once(&ctx, now()).await.unwrap();
    let m = message(&ctx, &id).await;
    assert_eq!(m.status, "failed");
    assert_eq!(m.error.as_deref(), Some("Twilio 21211: The 'To' number +1555 is not a valid phone number."));
    assert_eq!(twilio.texts().len(), 1);
}

#[tokio::test]
async fn delivery_status_is_read_back_at_30s_2min_10min() {
    let (_d, ctx) = ctx().await;
    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, None).await;
    let undelivered = queued(&ctx, "a1", "sms", TO, "one").await;
    let delivered = queued(&ctx, "a2", "sms", "+15155550124", "two").await;
    let silent = queued(&ctx, "a3", "sms", "+15155550125", "three").await;
    run_once(&ctx, now()).await.unwrap();
    let sid = |id: &str| {
        let ctx = ctx.clone();
        let id = id.to_owned();
        async move { message(&ctx, &id).await.provider_id.unwrap() }
    };
    twilio.set_status(&sid(&undelivered).await, json!({ "status": "undelivered", "error_code": 30007, "error_message": "Message filtered" }));
    twilio.set_status(&sid(&delivered).await, json!({ "status": "delivered" }));

    // Nothing is due before 30 s.
    let sent_at = message(&ctx, &silent).await.updated_at;
    assert_eq!(poll_statuses(&ctx, sent_at + Span::seconds(10)).await.unwrap(), 0);

    assert_eq!(poll_statuses(&ctx, sent_at + Span::seconds(31)).await.unwrap(), 3);
    let m = message(&ctx, &undelivered).await;
    assert_eq!(m.status, "failed");
    assert_eq!(m.error.as_deref(), Some("Twilio 30007: Message filtered"));
    assert_eq!(message(&ctx, &delivered).await.status, "delivered");
    let m = message(&ctx, &silent).await;
    assert_eq!(m.status, "sent");
    assert_eq!(next_attempt_at(&ctx, &silent).await, Some(m.updated_at + Span::seconds(120)));

    poll_statuses(&ctx, m.updated_at + Span::seconds(121)).await.unwrap();
    assert_eq!(next_attempt_at(&ctx, &silent).await, Some(m.updated_at + Span::seconds(600)));
    poll_statuses(&ctx, m.updated_at + Span::seconds(601)).await.unwrap();
    assert_eq!(next_attempt_at(&ctx, &silent).await, None, "no more checks after 10 min");
    assert_eq!(message(&ctx, &silent).await.status, "sent");
    let reads = twilio.hits().iter().filter(|h| h.method == "GET").count();
    assert_eq!(reads, 5);
}

#[tokio::test]
async fn email_goes_to_the_smtp_server_as_plain_text_with_login() {
    let (_d, ctx) = ctx().await;
    let sink = SmtpSink::start().await;
    let body = json!({
        "email": { "host": "127.0.0.1", "port": sink.port, "user": "farm", "from": "alerts@farm.example", "tls": "none" },
        "secrets": { "smtp_password": "hunter2" },
    });
    let (s, v) = call(&app(&ctx), "PUT", "/api/notify/channels", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(configured_channels(&ctx).await.unwrap().contains(&"email"));

    let mut m = out("e1", "email", "cody@farm.example", "214 outside P3 since 06:12.");
    m.subject = Some("214 outside P3".into());
    let id = enqueue(&ctx, m).await.unwrap().id;
    run_once(&ctx, now()).await.unwrap();

    let mails = sink.mails();
    assert_eq!(mails.len(), 1, "{:?}", message(&ctx, &id).await.error);
    let mail = &mails[0];
    assert_eq!(mail.from, "<alerts@farm.example>");
    assert_eq!(mail.to, vec!["<cody@farm.example>"]);
    assert!(mail.data.contains("Subject: 214 outside P3"), "{}", mail.data);
    assert!(mail.data.contains("Content-Type: text/plain"), "{}", mail.data);
    assert!(mail.data.contains("214 outside P3 since 06:12."), "{}", mail.data);
    let plain = base64::engine::general_purpose::STANDARD.encode("\0farm\0hunter2");
    assert_eq!(mail.auth.as_deref(), Some(plain.as_str()));
    assert_eq!(message(&ctx, &id).await.status, "sent");
}

#[tokio::test]
async fn smtp_refusals_fail_for_5xx_and_retry_for_4xx() {
    let (_d, ctx) = ctx().await;
    let sink = SmtpSink::start().await;
    let body = json!({ "email": { "host": "127.0.0.1", "port": sink.port, "from": "alerts@farm.example", "tls": "none" } });
    assert_eq!(call(&app(&ctx), "PUT", "/api/notify/channels", Some(body)).await.0, StatusCode::OK);

    sink.refuse_rcpt("550 5.1.1 No such user");
    let id = queued(&ctx, "e1", "email", "nobody@farm.example", "x").await;
    run_once(&ctx, now()).await.unwrap();
    let m = message(&ctx, &id).await;
    assert_eq!(m.status, "failed");
    assert!(m.error.as_deref().unwrap().contains("No such user"), "{:?}", m.error);

    sink.refuse_rcpt("451 4.3.0 Try again later");
    let t0 = now();
    let id = queued(&ctx, "e2", "email", "busy@farm.example", "x").await;
    run_once(&ctx, t0).await.unwrap();
    let m = message(&ctx, &id).await;
    assert_eq!(m.status, "queued", "{:?}", m.error);
    assert_eq!(next_attempt_at(&ctx, &id).await, Some(t0 + Span::seconds(5)));
}

fn verify_signature(secret: &str, header: &str, body: &[u8]) -> i64 {
    let (t, v1) = header.split_once(',').unwrap();
    let t = t.strip_prefix("t=").unwrap();
    let v1 = v1.strip_prefix("v1=").unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(format!("{t}.").as_bytes());
    mac.update(body);
    mac.verify_slice(&hex::decode(v1).unwrap()).expect("signature verifies");
    t.parse().unwrap()
}

async fn setup_webhook(ctx: &Ctx, hook: &Receiver) {
    let body = json!({ "webhook": { "url": format!("{}/hook", hook.url) }, "secrets": { "webhook_secret": "whsec_test" } });
    let (s, v) = call(&app(ctx), "PUT", "/api/notify/channels", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

#[tokio::test]
async fn webhook_is_signed_and_retried_at_1s_5s_25s() {
    let (_d, ctx) = ctx().await;
    let hook = Receiver::start().await;
    setup_webhook(&ctx, &hook).await;
    hook.reply(500, json!({}));
    hook.reply(503, json!({}));
    let id = queued(&ctx, "w1", "webhook", "farm webhook", "214 outside P3").await;

    let t0 = now();
    run_once(&ctx, t0).await.unwrap();
    assert_eq!(next_attempt_at(&ctx, &id).await, Some(t0 + Span::seconds(1)));
    assert_eq!(message(&ctx, &id).await.error.as_deref(), Some("The webhook answered 500."));
    run_once(&ctx, t0 + Span::seconds(1)).await.unwrap();
    assert_eq!(next_attempt_at(&ctx, &id).await, Some(t0 + Span::seconds(6)));
    run_once(&ctx, t0 + Span::seconds(6)).await.unwrap();
    let m = message(&ctx, &id).await;
    assert_eq!(m.status, "delivered");
    assert_eq!(m.error, None);

    let hits = hook.hits();
    assert_eq!(hits.len(), 3);
    for h in &hits {
        assert_eq!(h.path, "/hook");
        assert_eq!(h.header("content-type").as_deref(), Some("application/json"));
        let t = verify_signature("whsec_test", &h.header("x-openpasture-signature").unwrap(), &h.body);
        assert!((now().timestamp() - t).abs() < 60);
        let body = h.json();
        assert_eq!(body["type"], "message");
        assert_eq!(body["message"]["id"], id.as_str());
        assert_eq!(body["message"]["text"], "214 outside P3");
    }
    // A different secret doesn't verify.
    let h = &hits[0];
    let header = h.header("x-openpasture-signature").unwrap();
    let v1 = header.split("v1=").nth(1).unwrap();
    let mut mac = Hmac::<Sha256>::new_from_slice(b"other").unwrap();
    mac.update(format!("{}.", header.split(',').next().unwrap().trim_start_matches("t=")).as_bytes());
    mac.update(&h.body);
    assert!(mac.verify_slice(&hex::decode(v1).unwrap()).is_err());
}

#[tokio::test]
async fn webhook_gives_up_after_three_retries_and_fails_4xx_at_once() {
    let (_d, ctx) = ctx().await;
    let hook = Receiver::start().await;
    setup_webhook(&ctx, &hook).await;
    for _ in 0..4 {
        hook.reply(502, json!({}));
    }
    let id = queued(&ctx, "w1", "webhook", "hook", "x").await;
    let mut t = now();
    for wait in [0, 1, 5, 25] {
        t += Span::seconds(wait);
        run_once(&ctx, t).await.unwrap();
    }
    let m = message(&ctx, &id).await;
    assert_eq!((m.status.as_str(), m.attempts), ("failed", 4));
    assert_eq!(hook.hits().len(), 4);

    hook.reply(410, json!({}));
    let id = queued(&ctx, "w2", "webhook", "hook", "x").await;
    run_once(&ctx, now()).await.unwrap();
    let m = message(&ctx, &id).await;
    assert_eq!(m.status, "failed");
    assert_eq!(m.error.as_deref(), Some("The webhook answered 410."));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_queued_message_is_sent_once_by_racing_senders() {
    let (_d, ctx) = ctx().await;
    let hook = Receiver::start().await;
    hook.delay(15);
    setup_webhook(&ctx, &hook).await;
    let mut ids = HashSet::new();
    for i in 0..60 {
        ids.insert(queued(&ctx, &format!("k{i}"), "webhook", "hook", &format!("text {i}")).await);
    }
    let mut tasks = vec![];
    for _ in 0..4 {
        let ctx = ctx.clone();
        tasks.push(tokio::spawn(async move {
            let mut n = 0;
            loop {
                let got = run_once(&ctx, now()).await.unwrap();
                if got == 0 {
                    break n;
                }
                assert!(got <= sender::PER_CHANNEL, "one channel claims at most 4 at a time");
                n += got;
            }
        }));
    }
    let mut total = 0;
    for t in tasks {
        total += t.await.unwrap();
    }
    assert_eq!(total, 60);
    let hits = hook.hits();
    assert_eq!(hits.len(), 60);
    let sent: HashSet<String> = hits.iter().map(|h| h.json()["message"]["id"].as_str().unwrap().to_owned()).collect();
    assert_eq!(sent, ids);
    for m in messages(&ctx).await {
        assert_eq!((m.status.as_str(), m.attempts), ("delivered", 1), "{}", m.id);
    }
}

#[tokio::test]
async fn a_pass_claims_at_most_ten_and_four_per_channel() {
    let (_d, ctx) = ctx().await;
    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, Some(json!({ "from": "+15155550199" }))).await;
    let hook = Receiver::start().await;
    setup_webhook(&ctx, &hook).await;
    let sink = SmtpSink::start().await;
    let body = json!({ "email": { "host": "127.0.0.1", "port": sink.port, "from": "a@farm.example", "tls": "none" } });
    assert_eq!(call(&app(&ctx), "PUT", "/api/notify/channels", Some(body)).await.0, StatusCode::OK);
    for i in 0..6 {
        queued(&ctx, &format!("s{i}"), "sms", TO, "s").await;
        queued(&ctx, &format!("w{i}"), "whatsapp", TO, "w").await;
        queued(&ctx, &format!("e{i}"), "email", "a@farm.example", "e").await;
        queued(&ctx, &format!("h{i}"), "webhook", "hook", "h").await;
    }
    assert_eq!(run_once(&ctx, now()).await.unwrap(), 10);
    let sent = |c: &str| {
        let c = c.to_owned();
        let ctx = ctx.clone();
        async move { messages(&ctx).await.iter().filter(|m| m.channel == c && m.status != "queued").count() }
    };
    assert_eq!(sent("sms").await, 4);
    assert_eq!(sent("whatsapp").await, 4);
    assert_eq!(sent("email").await, 2);
    assert_eq!(sent("webhook").await, 0);
}

#[tokio::test]
async fn messages_for_a_channel_that_isnt_set_up_fail() {
    let (_d, ctx) = ctx().await;
    let id = queued(&ctx, "a1", "sms", TO, "hi").await;
    let r = queued(&ctx, "a2", "relay", TO, "hi").await;
    run_once(&ctx, now()).await.unwrap();
    assert_eq!(message(&ctx, &id).await.error.as_deref(), Some("SMS isn't set up."));
    assert_eq!(message(&ctx, &id).await.status, "failed");
    assert_eq!(message(&ctx, &r).await.error.as_deref(), Some("The relay isn't set up."));
}

#[tokio::test]
async fn the_background_sender_delivers_new_and_stranded_messages() {
    let (_d, ctx) = ctx().await;
    let hook = Receiver::start().await;
    setup_webhook(&ctx, &hook).await;
    // A server that stopped mid-send left this one `sending`.
    let stranded = queued(&ctx, "w0", "webhook", "hook", "left behind").await;
    let claimed = messages::claim(&ctx, &["webhook"], now(), 10).await.unwrap();
    assert_eq!(claimed.len(), 1);
    assert_eq!(message(&ctx, &stranded).await.status, "sending");

    op_alerts::start(ctx.clone()).await.unwrap();
    let fresh = queued(&ctx, "w1", "webhook", "hook", "new").await;
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let (a, b) = (message(&ctx, &stranded).await.status, message(&ctx, &fresh).await.status);
        if a == "delivered" && b == "delivered" {
            break;
        }
        assert!(std::time::Instant::now() < deadline, "still {a} / {b}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(hook.hits().len(), 2);
    ctx.shutdown();
}

#[tokio::test]
async fn settings_report_secrets_as_set_and_never_their_values() {
    let (_d, ctx) = ctx().await;
    let a = app(&ctx);
    let (s, v) = call(&a, "GET", "/api/notify/channels", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["email"]["port"], 587);
    assert_eq!(v["email"]["tls"], "starttls");
    assert_eq!(v["relay"]["enabled"], false);
    assert_eq!(v["twilio_api_base"], "https://api.twilio.com");
    assert_eq!(v["configured"], json!([]));
    let names: Vec<&str> = v["secrets"].as_array().unwrap().iter().map(|s| s["name"].as_str().unwrap()).collect();
    assert_eq!(names, ["twilio_account_sid", "twilio_auth_token", "smtp_password", "webhook_secret", "hosted_url", "hosted_api_key"]);

    let body = json!({ "sms": { "from": "(515) 555-0100" }, "secrets": { "twilio_account_sid": SID, "twilio_auth_token": " tok-secret " } });
    let (s, v) = call(&a, "PUT", "/api/notify/channels", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["sms"]["from"], "+15155550100", "numbers are stored as E.164");
    assert_eq!(v["configured"], json!(["sms"]));
    let text = v.to_string();
    assert!(!text.contains("tok-secret") && !text.contains(SID));
    assert_eq!(ctx.secrets().get("twilio_auth_token").unwrap().as_deref(), Some("tok-secret"));
    let set: Vec<bool> = v["secrets"].as_array().unwrap().iter().map(|s| s["set"].as_bool().unwrap()).collect();
    assert_eq!(set, [true, true, false, false, false, false]);

    // null removes a secret; the channel stops.
    let (s, v) = call(&a, "PUT", "/api/notify/channels", Some(json!({ "secrets": { "twilio_auth_token": null } }))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v["configured"], json!([]));
    assert_eq!(ctx.secrets().get("twilio_auth_token").unwrap(), None);

    for (body, words) in [
        (json!({ "sms": { "from": "555" } }), "The SMS number looks like +15155550123."),
        (json!({ "whatsapp": { "template_sid": "abc" } }), "Template SIDs start with HX."),
        (json!({ "email": { "from": "nope" } }), "The from address doesn't look right."),
        (json!({ "email": { "tls": "ssl" } }), ""),
        (json!({ "webhook": { "url": "hooks.example.com" } }), "The webhook URL starts with https://."),
        (json!({ "secrets": { "anthropic_api_key": "x" } }), "anthropic_api_key isn't a texting secret."),
        (json!({ "secrets": { "twilio_account_sid": "12345" } }), "Account SIDs start with AC."),
        (json!({ "secrets": { "hosted_api_key": "abc" } }), "Relay keys start with oph_."),
    ] {
        let (s, v) = call(&a, "PUT", "/api/notify/channels", Some(body.clone())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
        if !words.is_empty() {
            assert_eq!(v["error"], words, "{body}");
        }
    }
    // The view sent back as it came changes nothing.
    let (_, view) = call(&a, "GET", "/api/notify/channels", None).await;
    let (s, again) = call(&a, "PUT", "/api/notify/channels", Some(view.clone())).await;
    assert_eq!(s, StatusCode::OK, "{again}");
    assert_eq!(again, view);

    // Refused requests change nothing.
    assert_eq!(call(&a, "GET", "/api/notify/channels", None).await.1["sms"]["from"], "+15155550100");
}

#[tokio::test]
async fn test_sends_now_and_says_what_happened() {
    let (_d, ctx) = ctx().await;
    let a = app(&ctx);
    let (s, v) = call(&a, "POST", "/api/notify/test", Some(json!({ "channel": "sms", "to": TO }))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({ "ok": false, "detail": "SMS isn't set up." }));

    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, None).await;
    let (_, v) = call(&a, "POST", "/api/notify/test", Some(json!({ "channel": "sms" }))).await;
    assert_eq!(v, json!({ "ok": true, "detail": "Account active, sending from +15155550100" }));
    assert!(twilio.texts().is_empty(), "an account check sends nothing");

    let (_, v) = call(&a, "POST", "/api/notify/test", Some(json!({ "channel": "sms", "to": "515 555 0123" }))).await;
    assert_eq!(v, json!({ "ok": true, "detail": "Sent to +15155550123" }));
    let form = twilio.texts()[0].form();
    assert_eq!(form["Body"], "openpasture test. Reply STOP to opt out.");
    let log = messages(&ctx).await;
    assert_eq!(log.len(), 1);
    assert_eq!((log[0].kind.as_str(), log[0].status.as_str(), log[0].address.as_str()), ("test", "sent", TO));

    twilio.reply(401, json!({ "code": 20003, "message": "Authenticate" }));
    let (_, v) = call(&a, "POST", "/api/notify/test", Some(json!({ "channel": "sms", "to": TO }))).await;
    assert_eq!(v, json!({ "ok": false, "detail": "Twilio 20003: Authenticate" }));
    assert_eq!(messages(&ctx).await[1].status, "failed");

    let hook = Receiver::start().await;
    setup_webhook(&ctx, &hook).await;
    let (_, v) = call(&a, "POST", "/api/notify/test", Some(json!({ "channel": "webhook" }))).await;
    assert_eq!(v, json!({ "ok": true, "detail": "Delivered" }));
    let body = hook.hits()[0].json();
    assert_eq!(body["message"]["kind"], "test");
    assert_eq!(body["message"]["text"], "openpasture test.");

    let (s, _) = call(&a, "POST", "/api/notify/test", Some(json!({ "channel": "pigeon" }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn the_message_log_lists_newest_first_with_filters() {
    let (_d, ctx) = ctx().await;
    for i in 0..5 {
        queued(&ctx, &format!("k{i}"), "sms", TO, &format!("out {i}")).await;
    }
    op_core::messages::record_inbound(
        &ctx,
        op_core::messages::Inbound { channel: "sms".into(), from: TO.into(), text: "OK".into(), provider_id: Some("SMin".into()), at: now() },
        None,
        "received",
    )
    .await
    .unwrap();
    let a = app(&ctx);
    let (s, v) = call(&a, "GET", "/api/messages", None).await;
    assert_eq!(s, StatusCode::OK);
    let texts: Vec<&str> = v.as_array().unwrap().iter().map(|m| m["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["OK", "out 4", "out 3", "out 2", "out 1", "out 0"]);
    let (_, v) = call(&a, "GET", "/api/messages?direction=out&limit=2", None).await;
    let texts: Vec<&str> = v.as_array().unwrap().iter().map(|m| m["text"].as_str().unwrap()).collect();
    assert_eq!(texts, ["out 4", "out 3"]);
    let (_, v) = call(&a, "GET", "/api/messages?direction=in", None).await;
    assert_eq!(v.as_array().unwrap().len(), 1);
    let future = (now() + Span::hours(1)).to_rfc3339().replace('+', "%2B");
    let (_, v) = call(&a, "GET", &format!("/api/messages?from={future}"), None).await;
    assert_eq!(v, json!([]));
    let (s, _) = call(&a, "GET", "/api/messages?direction=sideways", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

/// A local port nothing listens on: connecting is refused at once, as when
/// the farm's internet is down.
fn closed_port() -> String {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    format!("http://127.0.0.1:{port}")
}

#[tokio::test]
async fn a_text_waits_out_an_internet_outage_and_goes_when_twilio_is_back() {
    let (_d, ctx) = ctx().await;
    let twilio = Receiver::start().await;
    setup_twilio(&ctx, &twilio, None).await;
    let (s, _) = call(&app(&ctx), "PUT", "/api/notify/channels", Some(json!({ "twilio_api_base": closed_port() }))).await;
    assert_eq!(s, StatusCode::OK);
    let id = queued(&ctx, "p1", "sms", TO, "Cows: move to P4 (30.6 ac, 3 d)? Reply Y or N. Code 4821").await;

    // Five minutes (and more) without a connection: still queued, soon at first, then every minute.
    let t0 = now();
    let mut t = t0;
    let mut waits = Vec::new();
    while t - t0 < Span::minutes(90) {
        run_once(&ctx, t).await.unwrap();
        let m = message(&ctx, &id).await;
        assert_eq!((m.status.as_str(), m.error.as_deref(), m.attempts), ("queued", Some("Twilio can't be reached."), 0), "the provider never saw it");
        let next = next_attempt_at(&ctx, &id).await.unwrap();
        waits.push((next - t).num_seconds());
        t = next;
    }
    assert_eq!(waits[0], 5);
    assert!(waits.iter().all(|w| (5..=60).contains(w)), "{waits:?}");
    assert_eq!(*waits.last().unwrap(), 60);

    // The internet is back: it goes on the next try, once.
    let (s, _) = call(&app(&ctx), "PUT", "/api/notify/channels", Some(json!({ "twilio_api_base": twilio.url }))).await;
    assert_eq!(s, StatusCode::OK);
    run_once(&ctx, t).await.unwrap();
    let m = message(&ctx, &id).await;
    assert_eq!((m.status.as_str(), m.attempts), ("sent", 1));
    assert_eq!(twilio.texts().len(), 1);

    // A 5xx after an outage still gets its own 4 tries.
    twilio.reply(503, json!({}));
    let id2 = queued(&ctx, "p2", "sms", TO, "Cows: 180 of 250 collars silent 25m").await;
    run_once(&ctx, t + Span::seconds(1)).await.unwrap();
    assert_eq!(message(&ctx, &id2).await.status, "queued");

    // Six hours without a connection: failed, and it says so.
    let (s, _) = call(&app(&ctx), "PUT", "/api/notify/channels", Some(json!({ "twilio_api_base": closed_port() }))).await;
    assert_eq!(s, StatusCode::OK);
    let id3 = queued(&ctx, "p3", "sms", TO, "214 outside P3").await;
    run_once(&ctx, now() + Span::hours(6)).await.unwrap();
    let m = message(&ctx, &id3).await;
    assert_eq!((m.status.as_str(), m.error.as_deref()), ("failed", Some("Twilio can't be reached. Gave up after 6 h.")));
}
