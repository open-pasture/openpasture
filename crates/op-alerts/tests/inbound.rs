//! Texts coming in (A3): the signed Twilio webhook, polling Twilio without a
//! public URL, the commands and their refusals, the reply window and codes,
//! verification by reply, questions, the relay's inbox and dead-man, and the
//! morning brief. Twilio, the relay host and the model API are in-test HTTP
//! servers; everything else is the product code.

mod a_engine_fixture;
mod notify_support;

use std::collections::HashMap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use a_engine_fixture::{Farm, mins, t0};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use base64::Engine as _;
use chrono::{DateTime, Duration, Utc};
use hmac::{Hmac, Mac};
use http_body_util::BodyExt;
use notify_support::{FROM, Receiver, TOKEN, call, call_with, serve, setup_twilio};
use op_alerts::inbound::{self, Received};
use op_core::alert::MessageLog;
use op_core::messages::Inbound;
use op_core::time::{now, to_db};
use op_core::{Ctx, Role, Via};
use serde_json::{Value, json};
use tower::ServiceExt;

const CODY: &str = "+15155550123"; // owner
const MIA: &str = "+15155550124"; // manager
const HANK: &str = "+15155550125"; // hand
const VERA: &str = "+15155550126"; // viewer
const PAT: &str = "+15155550127"; // hand, phone not verified

struct T {
    f: Farm,
    twilio: Receiver,
    people: HashMap<&'static str, String>,
}

impl T {
    fn ctx(&self) -> &Ctx {
        &self.f.ctx
    }
    fn id(&self, phone: &str) -> String {
        self.people[phone].clone()
    }
}

/// The §6 farm (P1–P3, Cows 250 in P1), the farm's Twilio pointed at an
/// in-test Twilio, the engine's tools, and five people.
async fn farm() -> T {
    let f = Farm::new().await;
    op_engine::register_tools(&f.ctx);
    let twilio = Receiver::start().await;
    setup_twilio(&f.ctx, &twilio, None).await;
    let mut people = HashMap::new();
    for (name, role, phone, verified) in [
        ("Cody", Role::Owner, CODY, true),
        ("Mia", Role::Manager, MIA, true),
        ("Hank", Role::Hand, HANK, true),
        ("Vera", Role::Viewer, VERA, true),
        ("Pat", Role::Hand, PAT, false),
    ] {
        people.insert(phone, f.person(name, role, Some(phone), verified, None).await);
    }
    T { f, twilio, people }
}

static SID: AtomicU32 = AtomicU32::new(1);

/// Message SIDs for texts coming in (the in-test Twilio numbers its outbound ones from 1).
fn sid() -> String {
    format!("SMin{:030}", SID.fetch_add(1, Ordering::SeqCst))
}

/// A text from `from` as the webhook or a poll hands it over.
async fn text(ctx: &Ctx, channel: &str, from: &str, body: &str) -> Received {
    let m = Inbound { channel: channel.into(), from: from.into(), text: body.into(), provider_id: Some(sid()), at: now() };
    inbound::receive(ctx, m).await.unwrap().expect("a new text")
}

async fn sms(t: &T, from: &str, body: &str) -> Received {
    text(t.ctx(), "sms", from, body).await
}

/// The reply's words (a text with no reply fails the test).
async fn reply(t: &T, from: &str, body: &str) -> String {
    let r = sms(t, from, body).await;
    let m = r.reply.unwrap_or_else(|| panic!("no reply to {body:?}: {:?}", r.message));
    assert_eq!(m.kind, "reply");
    assert_eq!(m.address, from);
    assert!(op_alerts::text::is_gsm7(&m.text), "{}", m.text);
    assert!(op_alerts::text::septets(&m.text) <= 320, "{}", m.text);
    m.text
}

async fn decision_status(ctx: &Ctx, id: &str) -> (String, Value) {
    let (s, inputs): (String, String) = sqlx::query_as("SELECT status, inputs FROM decisions WHERE id = ?").bind(id).fetch_one(ctx.db()).await.unwrap();
    (s, serde_json::from_str(&inputs).unwrap())
}

/// An alert-kind text we sent `to` `ago` before now: what opens the
/// code-less reply window.
async fn prompted(ctx: &Ctx, to: &str, ago: Duration) {
    let m = op_core::messages::enqueue(ctx, notify_support::out(&format!("prompt:{to}:{}", sid()), "sms", to, "Cows: move to P2? Reply Y or N")).await.unwrap();
    op_core::messages::mark(ctx, &m.id, "sent", Some(&sid()), None, None).await.unwrap();
    sqlx::query("UPDATE messages SET created_at = ? WHERE id = ?").bind(to_db(&(now() - ago))).bind(&m.id).execute(ctx.db()).await.unwrap();
}

/// A MOVE to P2 proposed 40 minutes before `t0`, with its `decision_waiting`
/// alert open (and so its code).
async fn proposal(t: &T, herd: &str) -> (String, String) {
    let id = t.f.decision("MOVE", "proposed", t0() - mins(40), None).await;
    sqlx::query("UPDATE decisions SET herd_id = ? WHERE id = ?").bind(herd).bind(&id).execute(t.ctx().db()).await.unwrap();
    t.f.eval(t0()).await;
    let a = t.f.open_kind("decision_waiting").await.into_iter().find(|a| a.targets.contains(&("decision".into(), id.clone()))).expect("decision_waiting");
    (id, a.data["code"].as_str().unwrap().to_owned())
}

async fn replies_to(ctx: &Ctx, to: &str) -> Vec<MessageLog> {
    let rows = sqlx::query("SELECT * FROM messages WHERE direction = 'out' AND kind = 'reply' AND address = ? ORDER BY created_at, rowid")
        .bind(to)
        .fetch_all(ctx.db())
        .await
        .unwrap();
    rows.iter().map(|r| op_core::messages::message_from_row(r).unwrap()).collect()
}

async fn inbound_rows(ctx: &Ctx) -> Vec<MessageLog> {
    let rows = sqlx::query("SELECT * FROM messages WHERE direction = 'in' ORDER BY created_at, rowid").fetch_all(ctx.db()).await.unwrap();
    rows.iter().map(|r| op_core::messages::message_from_row(r).unwrap()).collect()
}

// ---- the webhook -------------------------------------------------------------------------

/// Twilio's signature, computed here from the spec (not by the product).
fn twilio_sig(token: &str, url: &str, params: &[(&str, &str)]) -> String {
    let mut p: Vec<(&str, &str)> = params.to_vec();
    p.sort();
    let mut data = url.to_owned();
    for (k, v) in p {
        data.push_str(k);
        data.push_str(v);
    }
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(token.as_bytes()).unwrap();
    mac.update(data.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

async fn hook(ctx: &Ctx, path: &str, host: &str, params: &[(&str, &str)], sig: Option<&str>) -> (StatusCode, Value) {
    let body = form_urlencoded::Serializer::new(String::new()).extend_pairs(params.iter().copied()).finish();
    let mut req = Request::builder().method("POST").uri(path).header("host", host).header("content-type", "application/x-www-form-urlencoded");
    if let Some(s) = sig {
        req = req.header("x-twilio-signature", s);
    }
    let res = op_alerts::router().with_state(ctx.clone()).oneshot(req.body(Body::from(body)).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

fn form<'a>(sid: &'a str, from: &'a str, body: &'a str) -> Vec<(&'a str, &'a str)> {
    vec![
        ("AccountSid", notify_support::SID),
        ("ApiVersion", "2010-04-01"),
        ("Body", body),
        ("From", from),
        ("MessageSid", sid),
        ("NumMedia", "0"),
        ("SmsMessageSid", sid),
        ("To", FROM),
    ]
}

#[tokio::test]
async fn the_webhook_takes_only_what_twilio_signed_over_the_public_url() {
    let t = farm().await;
    let ctx = t.ctx();
    let (dec, _) = proposal(&t, &t.f.herd).await;
    prompted(ctx, MIA, mins(5)).await;
    let path = "/hooks/twilio/sms?farm=cows";
    let signed_url = "https://farm.example/hooks/twilio/sms?farm=cows";
    let s1 = sid();
    let params = form(&s1, MIA, "Y");
    let good = twilio_sig(TOKEN, signed_url, &params);

    // No public URL yet: texts come in by polling, the webhook refuses.
    let (s, v) = hook(ctx, path, "farm.example", &params, Some(&good)).await;
    assert_eq!(s, StatusCode::FORBIDDEN, "{v}");
    ctx.update_settings(&json!({"server": {"public_url": "https://farm.example/"}})).await.unwrap();
    assert_eq!(inbound::mode(ctx).await.unwrap(), inbound::Mode::Webhook);

    // Signed over what this server sees behind a proxy (another Host): no.
    let proxy_view = twilio_sig(TOKEN, "http://10.0.0.5:7878/hooks/twilio/sms?farm=cows", &params);
    assert_eq!(hook(ctx, path, "10.0.0.5:7878", &params, Some(&proxy_view)).await.0, StatusCode::FORBIDDEN);
    // Signed without the query string, with another token, over another body, or not at all: no.
    let no_query = twilio_sig(TOKEN, "https://farm.example/hooks/twilio/sms", &params);
    assert_eq!(hook(ctx, path, "farm.example", &params, Some(&no_query)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(hook(ctx, path, "farm.example", &params, Some(&twilio_sig("other-token", signed_url, &params))).await.0, StatusCode::FORBIDDEN);
    let tampered = form(&s1, MIA, "N");
    assert_eq!(hook(ctx, path, "farm.example", &tampered, Some(&good)).await.0, StatusCode::FORBIDDEN);
    assert_eq!(hook(ctx, path, "farm.example", &params, None).await.0, StatusCode::FORBIDDEN);
    assert!(inbound_rows(ctx).await.is_empty());
    assert_eq!(decision_status(ctx, &dec).await.0, "proposed");

    // Signed over public_url + path + query, whatever Host the proxy sent: taken.
    let (s, v) = hook(ctx, path, "10.0.0.5:7878", &params, Some(&good)).await;
    assert_eq!(s, StatusCode::NO_CONTENT, "{v}");
    let (status, inputs) = decision_status(ctx, &dec).await;
    assert_eq!(status, "applied");
    assert_eq!(inputs["farmer_response"]["by"], json!({"via": "text", "user_id": t.id(MIA), "name": "Mia"}));
    let r = replies_to(ctx, MIA).await;
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].text, "Approved. Cows moving to P2.");
    assert_eq!(r[0].channel, "sms");
    assert_eq!(r[0].decision_id.as_deref(), Some(dec.as_str()));
    // Twilio retries the same MessageSid: taken once.
    assert_eq!(hook(ctx, path, "farm.example", &params, Some(&good)).await.0, StatusCode::NO_CONTENT);
    assert_eq!(inbound_rows(ctx).await.len(), 1);
    assert_eq!(replies_to(ctx, MIA).await.len(), 1);
    let m = &inbound_rows(ctx).await[0];
    assert_eq!((m.channel.as_str(), m.address.as_str(), m.kind.as_str(), m.status.as_str()), ("sms", MIA, "inbound", "received"));
    assert_eq!(m.user_id.as_deref(), Some(t.id(MIA).as_str()));

    // The default port is the same URL to Twilio.
    let s2 = sid();
    let p2 = form(&s2, CODY, "status");
    let with_port = twilio_sig(TOKEN, "https://farm.example:443/hooks/twilio/sms", &p2);
    assert_eq!(hook(ctx, "/hooks/twilio/sms", "farm.example", &p2, Some(&with_port)).await.0, StatusCode::NO_CONTENT);
    // WhatsApp: its own path and `whatsapp:` numbers; the reply goes back on WhatsApp.
    let s3 = sid();
    let wa_from = format!("whatsapp:{CODY}");
    let p3 = form(&s3, &wa_from, "status");
    let sig3 = twilio_sig(TOKEN, "https://farm.example/hooks/twilio/whatsapp", &p3);
    assert_eq!(hook(ctx, "/hooks/twilio/whatsapp", "farm.example", &p3, Some(&sig3)).await.0, StatusCode::NO_CONTENT);
    let r = replies_to(ctx, CODY).await;
    assert_eq!(r.iter().map(|m| m.channel.as_str()).collect::<Vec<_>>(), ["sms", "whatsapp"]);
    // Another Twilio account's request, even correctly signed: no.
    let s4 = sid();
    let mut p4 = form(&s4, CODY, "status");
    p4[0] = ("AccountSid", "AC99999999999999999999999999999999");
    let sig4 = twilio_sig(TOKEN, "https://farm.example/hooks/twilio/sms", &p4);
    assert_eq!(hook(ctx, "/hooks/twilio/sms", "farm.example", &p4, Some(&sig4)).await.0, StatusCode::FORBIDDEN);
    // Texting in switched off: refused.
    inbound::save(ctx, &inbound::TextingConfig { inbound: false, ..Default::default() }).await.unwrap();
    let s5 = sid();
    let p5 = form(&s5, CODY, "status");
    let sig5 = twilio_sig(TOKEN, "https://farm.example/hooks/twilio/sms", &p5);
    assert_eq!(hook(ctx, "/hooks/twilio/sms", "farm.example", &p5, Some(&sig5)).await.0, StatusCode::FORBIDDEN);
}

// ---- polling -----------------------------------------------------------------------------

/// Twilio's message list: serves what the test puts in, records each query.
#[derive(Clone, Default)]
struct TwilioList {
    url: String,
    queries: Arc<Mutex<Vec<(String, String, Option<String>)>>>,
    messages: Arc<Mutex<Vec<Value>>>,
}

impl TwilioList {
    async fn start() -> Self {
        let mut l = TwilioList::default();
        let state = l.clone();
        let app = axum::Router::new().fallback(move |req: axum::extract::Request| {
            let s = state.clone();
            async move {
                let auth = req.headers().get("authorization").and_then(|v| v.to_str().ok()).map(str::to_owned);
                s.queries.lock().unwrap().push((req.uri().path().to_owned(), req.uri().query().unwrap_or_default().to_owned(), auth));
                axum::Json(json!({ "messages": *s.messages.lock().unwrap(), "next_page_uri": null, "page": 0 }))
            }
        });
        l.url = serve(app).await;
        l
    }
    fn add(&self, sid: &str, from: &str, body: &str, at: DateTime<Utc>, direction: &str) {
        let date = at.to_rfc2822();
        // Newest first, as Twilio lists them.
        self.messages.lock().unwrap().insert(
            0,
            json!({ "sid": sid, "from": from, "to": FROM, "body": body, "direction": direction, "status": "received", "date_sent": date, "date_created": date }),
        );
    }
}

#[tokio::test]
async fn polling_takes_a_y_from_twilio_once_without_a_public_url() {
    let t = farm().await;
    let ctx = t.ctx();
    let list = TwilioList::start().await;
    let (s, v) = call(&notify_support::app(ctx), "PUT", "/api/notify/channels", Some(json!({"twilio_api_base": list.url}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(inbound::mode(ctx).await.unwrap(), inbound::Mode::Polling);
    let (dec, _) = proposal(&t, &t.f.herd).await;
    prompted(ctx, MIA, mins(5)).await;

    // Yesterday's "Y" (before checking began) is never acted on.
    let start = now();
    list.add(&sid(), MIA, "Y", start - Duration::hours(3), "inbound");
    assert_eq!(inbound::poll::run_once(ctx, start).await.unwrap(), 0);
    assert_eq!(decision_status(ctx, &dec).await.0, "proposed");
    let (path, query, auth) = list.queries.lock().unwrap()[0].clone();
    assert_eq!(path, format!("/2010-04-01/Accounts/{}/Messages.json", notify_support::SID));
    let q: HashMap<String, String> = form_urlencoded::parse(query.as_bytes()).into_owned().collect();
    assert_eq!(q["To"], FROM);
    assert_eq!(q["DateSent>"], (start - Duration::days(1)).format("%Y-%m-%d").to_string());
    let basic = base64::engine::general_purpose::STANDARD.encode(format!("{}:{TOKEN}", notify_support::SID));
    assert_eq!(auth.as_deref(), Some(format!("Basic {basic}").as_str()));

    // A new "Y", one of our own outbound texts in the list, and a stranger.
    let y = sid();
    list.add(&y, MIA, "Y", start + Duration::seconds(5), "inbound");
    list.add(&sid(), FROM, "Cows: move to P2?", start + Duration::seconds(6), "outbound-api");
    list.add(&sid(), "+15005550006", "hello", start + Duration::seconds(7), "inbound");
    assert_eq!(inbound::poll::run_once(ctx, start + Duration::seconds(10)).await.unwrap(), 2);
    let (status, inputs) = decision_status(ctx, &dec).await;
    assert_eq!(status, "applied");
    assert_eq!(inputs["farmer_response"]["by"]["via"], "text");
    assert_eq!(replies_to(ctx, MIA).await.len(), 1);
    // The next checks see the same list: nothing is taken twice.
    assert_eq!(inbound::poll::run_once(ctx, start + Duration::seconds(20)).await.unwrap(), 0);
    assert_eq!(inbound::poll::run_once(ctx, start + Duration::seconds(30)).await.unwrap(), 0);
    let rows = inbound_rows(ctx).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].provider_id.as_deref(), Some(y.as_str()));
    assert_eq!((rows[1].status.as_str(), rows[1].error.as_deref()), ("ignored", Some("Unknown number.")));
    assert_eq!(replies_to(ctx, MIA).await.len(), 1);

    // GET /api/texting says how replies come in, and when Twilio was last read.
    let owner = |m, p, b| t.f.owner(m, p, b);
    let (_, v) = owner("GET", "/api/texting", None).await;
    assert_eq!(v["inbound_mode"], "polling");
    assert!(v["checked"]["ok_at"].is_string() && v["checked"].get("error").is_none(), "{v}");
    // A failing check is reported, and the next good one clears it.
    list.messages.lock().unwrap().clear();
    let (s, _) = call(&notify_support::app(ctx), "PUT", "/api/notify/channels", Some(json!({"twilio_api_base": "http://127.0.0.1:9"}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(inbound::poll::run_once(ctx, start + Duration::seconds(40)).await.unwrap(), 0);
    let (_, v) = owner("GET", "/api/texting", None).await;
    assert_eq!(v["checked"]["error"], "Twilio can't be reached.", "{v}");
}

// ---- commands ------------------------------------------------------------------------------

#[tokio::test]
async fn each_command_answers_in_words_and_roles_are_kept() {
    let t = farm().await;
    let ctx = t.ctx();
    let tag_collar = t.f.collar(Some("214"), now() - mins(3)).await;
    let _other = t.f.collar(Some("215"), now() - mins(1)).await;
    let (dec, code) = proposal(&t, &t.f.herd).await;
    for p in [CODY, MIA, HANK, VERA] {
        prompted(ctx, p, mins(10)).await;
    }

    // STATUS: anyone.
    let s = reply(&t, VERA, "status").await;
    assert!(s.starts_with("Cows: 250 hd in P1."), "{s}");
    assert!(s.contains("Waiting: move to P2."), "{s}");
    // WHERE: the place in words, how old, a map link (the farm is imperial).
    let w = reply(&t, HANK, "where is 214").await;
    let fix: String = sqlx::query_scalar("SELECT last_fix FROM collars WHERE id = ?").bind(&tag_collar).fetch_one(ctx.db()).await.unwrap();
    let fix: Value = serde_json::from_str(&fix).unwrap();
    let (lon, lat) = (fix["point"][0].as_f64().unwrap(), fix["point"][1].as_f64().unwrap());
    assert_eq!(w, format!("214, in P1, 3m ago. https://maps.google.com/?q={lat:.6},{lon:.6}"));
    assert_eq!(reply(&t, HANK, "Where 999?").await, "No animal 999.");

    // Y / N / LATER / STOP MOVE need a manager.
    assert_eq!(reply(&t, HANK, "y").await, "Your role can't answer decisions.");
    assert_eq!(reply(&t, VERA, &format!("Y {code}")).await, "Your role can't answer decisions.");
    assert_eq!(reply(&t, HANK, "later").await, "Your role can't answer decisions.");
    assert_eq!(reply(&t, HANK, "stop move").await, "Your role can't stop moves.");
    assert_eq!(reply(&t, VERA, "ok").await, "Your role can't ack alerts.");
    assert_eq!(decision_status(ctx, &dec).await.0, "proposed");

    // LATER: asked again in an hour, on the same channel, with the code.
    let l = reply(&t, MIA, "Later").await;
    assert!(l.starts_with("OK. I'll ask again at "), "{l}");
    assert!(op_alerts::inbound::reminders::run_due(ctx, now() + mins(59)).await.unwrap().is_empty());
    let again = op_alerts::inbound::reminders::run_due(ctx, now() + mins(61)).await.unwrap();
    assert_eq!(again.len(), 1);
    assert_eq!((again[0].address.as_str(), again[0].channel.as_str(), again[0].kind.as_str()), (MIA, "sms", "reply"));
    assert!(again[0].text.contains("Reply Y or N") && again[0].text.ends_with(&format!("Code {code}")), "{}", again[0].text);
    assert!(op_alerts::inbound::reminders::run_due(ctx, now() + mins(90)).await.unwrap().is_empty());

    // Sí approves, as the manager, by text.
    assert_eq!(reply(&t, MIA, "Sí").await, "Approved. Cows moving to P2.");
    let (status, inputs) = decision_status(ctx, &dec).await;
    assert_eq!(status, "applied");
    assert_eq!(inputs["farmer_response"]["action"], "approve");
    assert_eq!(inputs["farmer_response"]["by"], json!({"via": "text", "user_id": t.id(MIA), "name": "Mia"}));
    assert_eq!(reply(&t, MIA, "y").await, "No decision is waiting for an answer.");
    // A reminder for an answered decision goes nowhere.
    let late = op_alerts::inbound::reminders::run_due(ctx, now() + mins(200)).await.unwrap();
    assert!(late.is_empty());

    // STATUS during the move says where the herd is going.
    let s = reply(&t, VERA, "status").await;
    assert!(s.starts_with("Cows: 250 hd moving to P2, ") && s.contains(" ft to go."), "{s}");
    // STOP MOVE: the move the approval started stops where it is.
    let s = reply(&t, CODY, "stop move").await;
    assert!(s.starts_with("Stopped. Cows keep the boundary they have"), "{s}");
    let st: String = sqlx::query_scalar("SELECT status FROM moves WHERE decision_id = ?").bind(&dec).fetch_one(ctx.db()).await.unwrap();
    assert_eq!(st, "stopped");
    assert_eq!(reply(&t, CODY, "STOP MOVE").await, "No move is running.");

    // OK: acks the alerts the last alert text to this person covered.
    let c = t.f.collar(Some("031"), now()).await;
    sqlx::query("UPDATE collars SET last_seen = ?").bind(to_db(&t0())).execute(ctx.db()).await.unwrap();
    t.f.outside(&c, t0() - mins(30), t0() - mins(1)).await;
    t.f.escape(&c, "returning", t0() - mins(30), None).await;
    t.f.eval(t0()).await;
    let texts = t.f.route(t0() + mins(2)).await;
    let escaped = t.f.open_kind("escaped").await;
    assert_eq!(escaped.len(), 1);
    // The escape's text is the last alert text Hank got (others still queued don't count).
    let to_hank: Vec<&MessageLog> = texts.iter().filter(|m| m.address == HANK && m.alert_id.as_deref() == Some(escaped[0].id.as_str())).collect();
    assert_eq!(to_hank.len(), 1, "{texts:?}");
    op_core::messages::mark(ctx, &to_hank[0].id, "sent", Some(&sid()), None, None).await.unwrap();
    assert_eq!(reply(&t, HANK, "OK").await, format!("Acked: {}.", escaped[0].title));
    let a = t.f.alert(&escaped[0].id).await;
    assert_eq!(a.acked_by.unwrap(), op_core::Actor { via: Via::Text, user_id: Some(t.id(HANK)), name: Some("Hank".into()) });
    assert_eq!(reply(&t, HANK, "ok").await, format!("{} is already acked.", escaped[0].title));
    assert_eq!(reply(&t, MIA, "ok").await, "No alert has been texted to you.");

    // Unknown and unverified numbers: logged, never answered.
    for (from, why) in [("+15005550006", "Unknown number."), (PAT, "Phone not verified.")] {
        let r = sms(&t, from, "status").await;
        assert!(r.reply.is_none());
        assert_eq!((r.message.status.as_str(), r.message.error.as_deref()), ("ignored", Some(why)));
    }
    let r = text(ctx, "sms", "OPENPASTURE", "hi").await;
    assert_eq!((r.message.status.as_str(), r.reply.is_none()), ("ignored", true));
    // HELP and INFO are Twilio's to answer.
    assert!(sms(&t, CODY, "HELP").await.reply.is_none());
}

#[tokio::test]
async fn a_code_is_needed_after_the_reply_window_and_accepted_with_it() {
    let t = farm().await;
    let ctx = t.ctx();
    let (dec, code) = proposal(&t, &t.f.herd).await;
    // Nothing sent to Mia yet: a bare Y could be anyone's.
    assert_eq!(reply(&t, MIA, "y").await, "Add the code from the decision's text, like Y 4821.");
    // The last alert 13 h ago: the window (12 h) is over. Replies don't reopen it.
    prompted(ctx, MIA, Duration::hours(13)).await;
    assert_eq!(reply(&t, MIA, "status").await.lines().count(), 1);
    assert_eq!(reply(&t, MIA, "Y").await, "Add the code from the decision's text, like Y 4821.");
    assert_eq!(reply(&t, MIA, "n 1").await, "Add the code from the decision's text, like N 4821.");
    assert_eq!(reply(&t, MIA, "stop move").await, "Add the code from the decision's text, like STOP MOVE 4821.");
    assert_eq!(decision_status(ctx, &dec).await.0, "proposed");
    // A wrong code is logged as one.
    let wrong = if code == "1111" { "2222" } else { "1111" };
    let r = sms(&t, MIA, &format!("Y {wrong}")).await;
    assert_eq!(r.reply.unwrap().text, format!("No decision waits on code {wrong}."));
    assert_eq!(r.message.error.as_deref(), Some("Wrong code."));
    // With the code: approved.
    assert_eq!(reply(&t, MIA, &format!("Y {code}")).await, "Approved. Cows moving to P2.");
    assert_eq!(decision_status(ctx, &dec).await.0, "applied");

    // Within the window a bare N is enough; the window is a setting.
    let (dec2, _) = {
        let id = t.f.decision("STAY", "proposed", now(), None).await;
        (id, ())
    };
    prompted(ctx, CODY, Duration::hours(2)).await;
    assert_eq!(reply(&t, CODY, "n").await, "Rejected. Nothing sent for Cows.");
    assert_eq!(decision_status(ctx, &dec2).await.0, "rejected");
    let (s, v) = t.f.owner("PUT", "/api/texting", Some(json!({"approve_window_h": 1}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let _dec3 = t.f.decision("STAY", "proposed", now(), None).await;
    assert_eq!(reply(&t, CODY, "n").await, "Add the code from the decision's text, like N 4821.");

    // Five wrong codes in an hour and codes from that number stop counting.
    for _ in 0..4 {
        sms(&t, MIA, &format!("N {wrong}")).await;
    }
    assert_eq!(reply(&t, MIA, &format!("N {wrong}")).await, "Too many wrong codes from this number. Try again in an hour.");
}

#[tokio::test]
async fn several_decisions_get_a_numbered_list() {
    let t = farm().await;
    let ctx = t.ctx();
    let (_, h) = t.f.core("POST", "/api/herds", Some(json!({"name": "Heifers", "species": "cattle", "count": 12, "paddock_id": t.f.paddocks[2]}))).await;
    let heifers = h["id"].as_str().unwrap().to_owned();
    let (cows_dec, _) = proposal(&t, &t.f.herd).await;
    let stay = t.f.decision("STAY", "proposed", t0() - mins(20), None).await;
    sqlx::query("UPDATE decisions SET herd_id = ? WHERE id = ?").bind(&heifers).bind(&stay).execute(ctx.db()).await.unwrap();
    prompted(ctx, CODY, mins(1)).await;

    assert_eq!(reply(&t, CODY, "y").await, "2 decisions waiting. 1. Cows: move to P2. 2. Heifers: stay in P3. Reply Y or N and the number, like Y 1.");
    assert_eq!(decision_status(ctx, &cows_dec).await.0, "proposed");
    assert_eq!(reply(&t, CODY, "y 2").await, "Approved. Heifers stays in P3.");
    assert_eq!(decision_status(ctx, &stay).await.0, "approved");
    // The list stays as it was texted: 2 is still the Heifers' decision.
    assert_eq!(reply(&t, CODY, "n 2").await, "Decision 2 is already answered.");
    assert_eq!(reply(&t, CODY, "y 3").await, "There's no decision 3. Reply Y for the list.");
    assert_eq!(reply(&t, CODY, "N 1").await, "Rejected. Nothing sent for Cows.");
    assert_eq!(decision_status(ctx, &cows_dec).await.0, "rejected");
    let (_, inputs) = decision_status(ctx, &cows_dec).await;
    assert_eq!(inputs["farmer_response"]["by"]["name"], "Cody");
}

// ---- opting out, verification --------------------------------------------------------------

#[tokio::test]
async fn opting_out_stops_alerts_and_briefs_and_start_restores_them() {
    let t = farm().await;
    let ctx = t.ctx();
    // Brief at 06:30 farm time for Hank.
    let (s, v) = t.f.owner("PUT", "/api/texting", Some(json!({"brief": {"enabled": true, "time": "06:30"}}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let (s, v) = t.f.owner("PUT", &format!("/api/texting/people/{}", t.id(HANK)), Some(json!({"brief": true}))).await;
    assert_eq!((s, v["brief"].clone()), (StatusCode::OK, json!(true)));

    // STOP: mirrored, and no reply (Twilio confirms STOP itself).
    let r = sms(&t, HANK, "STOP").await;
    assert!(r.reply.is_none());
    let (_, v) = t.f.owner("GET", "/api/texting", None).await;
    let hank = v["people"].as_array().unwrap().iter().find(|p| p["user_id"] == t.id(HANK)).unwrap().clone();
    assert_eq!(hank, json!({"user_id": t.id(HANK), "brief": true, "sms_opt_out": true}));
    // Nothing reaches Hank: no alert, no brief, no reply to anything but START.
    let c = t.f.collar(Some("031"), now()).await;
    t.f.outside(&c, t0() - mins(30), t0() - mins(1)).await;
    t.f.escape(&c, "returning", t0() - mins(30), None).await;
    t.f.eval(t0()).await;
    let texts = t.f.route(t0() + mins(2)).await;
    assert!(!texts.is_empty() && texts.iter().all(|m| m.address != HANK), "{texts:?}");
    let brief_at = next_0630(ctx).await;
    assert!(op_alerts::brief_send::run_once(ctx, brief_at).await.unwrap().is_empty());
    let r = sms(&t, HANK, "status").await;
    assert_eq!((r.message.status.as_str(), r.reply.is_none()), ("ignored", true));
    // "yes" is Twilio's opt-in word too: for an opted-out number it's START.
    let r = sms(&t, HANK, "yes").await;
    assert!(r.reply.is_none());
    let (_, out) = op_alerts::routing::prefs::get(ctx, &t.id(HANK)).await.unwrap();
    assert!(!out);
    // Back in: tomorrow's brief reaches him.
    let next = op_alerts::brief_send::run_once(ctx, brief_at + Duration::days(1)).await.unwrap();
    assert_eq!(next.iter().map(|m| m.address.as_str()).collect::<Vec<_>>(), [HANK]);
    // STOP and START by WhatsApp are confirmed there.
    let r = text(ctx, "whatsapp", &format!("whatsapp:{HANK}"), "stop").await;
    assert_eq!(r.reply.unwrap().text, "You won't get openpasture messages here. Reply START to get them again.");
    let r = text(ctx, "whatsapp", &format!("whatsapp:{HANK}"), "START").await;
    assert_eq!(r.reply.unwrap().text, "You'll get openpasture messages here again.");
}

/// The next 06:30 America/Chicago after now.
async fn next_0630(_ctx: &Ctx) -> DateTime<Utc> {
    let tz: chrono_tz::Tz = "America/Chicago".parse().unwrap();
    let day = now().with_timezone(&tz).date_naive() + Duration::days(1);
    op_alerts::brief_send::due_at(day, chrono::NaiveTime::from_hms_opt(6, 30, 0).unwrap(), tz).unwrap()
}

#[tokio::test]
async fn a_verification_code_texted_back_verifies_the_phone() {
    let t = farm().await;
    let ctx = t.ctx();
    // Pat's code goes out through the farm's Twilio.
    let sent = op_alerts::notify::verify::send_code(ctx, &t.id(PAT)).await.unwrap();
    assert_eq!(sent.via, "sms");
    let code = notify_support::last_code(&t.twilio);
    // Anything but the code from an unverified phone is ignored; so is a wrong code.
    let wrong = if code == "000000" { "111111" } else { "000000" };
    let r = sms(&t, PAT, wrong).await;
    assert_eq!((r.message.status.as_str(), r.reply.is_none()), ("ignored", true));
    assert!(op_core::users::get_user(ctx, &t.id(PAT)).await.unwrap().unwrap().phone_verified_at.is_none());
    // The code: verified, and told so.
    assert_eq!(reply(&t, PAT, &format!("{} {}", &code[..3], &code[3..])).await, "Phone verified. Reply STATUS any time.");
    assert!(op_core::users::get_user(ctx, &t.id(PAT)).await.unwrap().unwrap().phone_verified_at.is_some());
    // Now Pat's texts count.
    assert!(reply(&t, PAT, "status").await.starts_with("Cows: 250 hd"));
}

// ---- questions -------------------------------------------------------------------------------

/// An OpenAI-compatible model API that answers every question and records what it got.
async fn model_api(answer: &'static str) -> (String, Arc<Mutex<Vec<Value>>>) {
    let seen: Arc<Mutex<Vec<Value>>> = Default::default();
    let s = seen.clone();
    let app = axum::Router::new().route(
        "/chat/completions",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let s = s.clone();
            async move {
                s.lock().unwrap().push(body);
                axum::Json(json!({ "choices": [{ "finish_reason": "stop", "message": { "role": "assistant", "content": answer } }] }))
            }
        }),
    );
    (serve(app).await, seen)
}

async fn wait_reply(ctx: &Ctx, to: &str, n: usize) -> Vec<MessageLog> {
    for _ in 0..200 {
        let r = replies_to(ctx, to).await;
        if r.len() >= n {
            return r;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no reply to {to}");
}

#[tokio::test]
async fn questions_go_to_the_brain_with_read_tools_but_never_run_sql() {
    let t = farm().await;
    let ctx = t.ctx();
    // The heuristic doesn't answer questions: the command list instead.
    let r = sms(&t, VERA, "How much grass is left in P1?").await;
    assert!(r.reply.is_none(), "answered in the background");
    let got = wait_reply(ctx, VERA, 1).await;
    assert_eq!(
        got[0].text,
        "Texts I answer: Y or N to a decision (add its code after 12 h), LATER, OK to ack, STATUS, WHERE and a tag, STOP MOVE. STOP ends all texts."
    );

    // A compatible brain answers, with the farm summary and the read tools as the texter.
    let (url, seen) = model_api("P1 has about 1,500 kg DM/ha left — roughly 3 days for the Cows. “Move soon.” 🐄").await;
    ctx.secrets().set("compatible_base_url", &url).unwrap();
    ctx.update_settings(&json!({"brain": {"id": "compatible", "model": "local"}})).await.unwrap();
    sms(&t, VERA, "How much grass is left in P1?").await;
    let got = wait_reply(ctx, VERA, 2).await;
    assert_eq!(got[1].text, "P1 has about 1,500 kg DM/ha left - roughly 3 days for the Cows. \"Move soon.\"");
    let body = seen.lock().unwrap()[0].clone();
    let tools: Vec<String> = body["tools"].as_array().unwrap().iter().map(|t| t["function"]["name"].as_str().unwrap().to_owned()).collect();
    assert!(!tools.contains(&"run_sql".to_owned()), "{tools:?}");
    assert!(tools.contains(&"get_farm".to_owned()) && tools.contains(&"list_alerts".to_owned()), "{tools:?}");
    assert!(!tools.contains(&"propose_boundary".to_owned()) && !tools.contains(&"ack_alert".to_owned()), "read tools only: {tools:?}");
    let prompt = body["messages"][1]["content"].as_str().unwrap();
    assert!(prompt.contains("How much grass is left in P1?") && prompt.contains("\"asked_by\"") && prompt.contains("Vera"), "{prompt}");
    assert!(prompt.contains("at most 320 characters"), "{prompt}");
    // On WhatsApp the answer may be longer (1,000) and keeps its characters.
    text(ctx, "whatsapp", &format!("whatsapp:{VERA}"), "And P2?").await;
    let got = wait_reply(ctx, VERA, 3).await;
    assert_eq!(got[2].channel, "whatsapp");
    assert!(got[2].text.contains("—") && got[2].text.contains("🐄"), "{}", got[2].text);
    let prompt = seen.lock().unwrap()[1]["messages"][1]["content"].as_str().unwrap().to_owned();
    assert!(prompt.contains("at most 1000 characters"), "{prompt}");
}

// ---- the relay ---------------------------------------------------------------------------------

/// A farm server that texts through `host` with its own key.
async fn relay_farm(host: &notify_support::Host, label: &str) -> (tempfile::TempDir, Ctx, String) {
    let (dir, ctx) = notify_support::ctx().await;
    let (info, key) = op_brain::hosted::create_key(&host.ctx, label).await.unwrap();
    ctx.secrets().set("hosted_url", &host.url).unwrap();
    ctx.secrets().set("hosted_api_key", &key).unwrap();
    let (s, v) = call(&notify_support::app(&ctx), "PUT", "/api/notify/channels", Some(json!({"relay": {"enabled": true}}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let _ = info;
    (dir, ctx, key)
}

/// Prove `phone` on the host for `key`, reading the code off the host's Twilio.
async fn host_recipient(host: &notify_support::Host, key: &str, phone: &str, deadman: bool) {
    let auth = format!("Bearer {key}");
    let app = notify_support::app(&host.ctx);
    let (s, v) =
        call_with(&app, "POST", "/v1/notify/recipients", Some(json!({"channel": "sms", "to": phone, "deadman": deadman})), &[("authorization", &auth)]).await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    let code = notify_support::last_code(&host.twilio);
    let (s, v) =
        call_with(&app, "POST", "/v1/notify/recipients/verify", Some(json!({"channel": "sms", "to": phone, "code": code})), &[("authorization", &auth)]).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

async fn inbox(host: &notify_support::Host, key: &str, since: &str, wait: u64) -> (StatusCode, Value) {
    call_with(
        &notify_support::app(&host.ctx),
        "GET",
        &format!("/v1/notify/inbox?since={since}&wait={wait}"),
        None,
        &[("authorization", &format!("Bearer {key}"))],
    )
    .await
}

#[tokio::test]
async fn the_relay_inbox_goes_to_the_key_that_last_texted_and_delivers_once() {
    let host = notify_support::Host::start().await;
    let (_da, farm_a, key_a) = relay_farm(&host, "Farm A").await;
    let (_db, farm_b, key_b) = relay_farm(&host, "Farm B").await;
    // One phone on both farms; farm B's code was the last text to it.
    host_recipient(&host, &key_a, CODY, false).await;
    host_recipient(&host, &key_b, CODY, false).await;

    // No key, a stranger's key, hosting off: refused.
    let app = notify_support::app(&host.ctx);
    assert_eq!(call(&app, "GET", "/v1/notify/inbox", None).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(inbox(&host, "oph_nope", "", 0).await.0, StatusCode::UNAUTHORIZED);
    assert_eq!(inbox(&host, &key_b, "x", 0).await.0, StatusCode::BAD_REQUEST);

    // B is waiting when the text comes in: its long-poll returns with it.
    let waiting = {
        let host_ctx = host.ctx.clone();
        let key = key_b.clone();
        tokio::spawn(async move {
            call_with(&notify_support::app(&host_ctx), "GET", "/v1/notify/inbox?wait=10", None, &[("authorization", &format!("Bearer {key}"))]).await
        })
    };
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let started = std::time::Instant::now();
    let r = text(&host.ctx, "sms", CODY, "status").await;
    assert!(r.reply.is_none(), "the host doesn't answer a farm's text itself");
    assert_eq!(r.message.status, "received");
    let (s, v) = waiting.await.unwrap();
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(started.elapsed() < std::time::Duration::from_secs(5), "{:?}", started.elapsed());
    let msgs = v["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 1);
    assert_eq!((msgs[0]["from"].as_str(), msgs[0]["text"].as_str(), msgs[0]["channel"].as_str()), (Some(CODY), Some("status"), Some("sms")));
    let cursor = v["cursor"].as_str().unwrap().to_owned();
    // A got nothing; B's next poll with the cursor gets nothing again, and the row is gone.
    assert!(inbox(&host, &key_a, "", 0).await.1["messages"].as_array().unwrap().is_empty());
    let (_, again) = inbox(&host, &key_b, &cursor, 0).await;
    assert!(again["messages"].as_array().unwrap().is_empty());
    assert_eq!(again["cursor"], cursor);
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM relay_inbox").fetch_one(host.ctx.db()).await.unwrap();
    assert_eq!(n, 0);

    // Farm A texts Cody through the relay: now A texted last, and Cody's reply goes to A.
    let (s, v) = call_with(
        &app,
        "POST",
        "/v1/notify",
        Some(json!({"idempotency_key": "ntf_a1", "channel": "sms", "to": CODY, "text": "Cows: move to P2? Reply Y or N"})),
        &[("authorization", &format!("Bearer {key_a}"))],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    op_alerts::notify::sender::run_once(&host.ctx, now()).await.unwrap();
    text(&host.ctx, "sms", CODY, "where is 214").await;
    assert!(inbox(&host, &key_b, &cursor, 0).await.1["messages"].as_array().unwrap().is_empty());
    let (_, a) = inbox(&host, &key_a, "", 0).await;
    assert_eq!(a["messages"][0]["text"], "where is 214");

    // STOP to the shared number: every farm that has the number hears it.
    text(&host.ctx, "sms", CODY, "STOP").await;
    let (_, a2) = inbox(&host, &key_a, a["cursor"].as_str().unwrap(), 0).await;
    let (_, b2) = inbox(&host, &key_b, &cursor, 0).await;
    assert_eq!(a2["messages"][0]["text"], "STOP");
    assert_eq!(b2["messages"][0]["text"], "STOP");
    // Someone the farms don't know: the host's own business (ignored here).
    let r = text(&host.ctx, "sms", "+15005550006", "hi").await;
    assert_eq!(r.message.status, "ignored");

    // Farm side: farm B takes its inbox, once, and mirrors the STOP for its person.
    let bob = op_core::users::create_user(&farm_b, op_core::users::NewUser { name: "Cody".into(), role: Role::Owner, phone: Some(CODY.into()), email: None })
        .await
        .unwrap();
    op_core::users::set_phone_verified(&farm_b, &bob.id, now()).await.unwrap();
    assert_eq!(inbound::mode(&farm_b).await.unwrap(), inbound::Mode::Relay);
    assert_eq!(inbound::relay::run_once(&farm_b, 0).await.unwrap(), 1);
    let rows = inbound_rows(&farm_b).await;
    assert_eq!(rows.iter().map(|m| (m.channel.as_str(), m.text.as_str())).collect::<Vec<_>>(), [("relay", "STOP")]);
    assert!(op_alerts::routing::prefs::get(&farm_b, &bob.id).await.unwrap().1);
    // A lost answer: the farm asks again with its old cursor, gets the text again, and takes it once.
    sqlx::query("UPDATE texting_state SET cursor = NULL WHERE key = 'relay:inbox'").execute(farm_b.db()).await.unwrap();
    assert_eq!(inbound::relay::run_once(&farm_b, 0).await.unwrap(), 0);
    assert_eq!(inbound_rows(&farm_b).await.len(), 1);
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM relay_inbox WHERE key_id = (SELECT key_id FROM relay_polls ORDER BY polled_at DESC LIMIT 1)")
        .fetch_one(host.ctx.db())
        .await
        .unwrap();
    assert_eq!(n, 1, "held until the farm sends the cursor back");
    assert_eq!(inbound::relay::run_once(&farm_b, 0).await.unwrap(), 0);
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM relay_inbox WHERE key_id = (SELECT key_id FROM relay_polls ORDER BY polled_at DESC LIMIT 1)")
        .fetch_one(host.ctx.db())
        .await
        .unwrap();
    assert_eq!(n, 0, "delivered and gone");
    // A reply from farm B's person goes back through the relay.
    text(&host.ctx, "sms", CODY, "START").await;
    assert_eq!(inbound::relay::run_once(&farm_b, 0).await.unwrap(), 1);
    assert!(!op_alerts::routing::prefs::get(&farm_b, &bob.id).await.unwrap().1);
    // Texting in off at farm B: the inbox is still read (the relay's dead-man), texts are ignored.
    inbound::save(&farm_b, &inbound::TextingConfig { inbound: false, ..Default::default() }).await.unwrap();
    let (s, v) = call_with(
        &app,
        "POST",
        "/v1/notify",
        Some(json!({"idempotency_key": "ntf_b1", "channel": "sms", "to": CODY, "text": "Cows: 250 hd in P1."})),
        &[("authorization", &format!("Bearer {key_b}"))],
    )
    .await;
    assert_eq!(s, StatusCode::ACCEPTED, "{v}");
    op_alerts::notify::sender::run_once(&host.ctx, now()).await.unwrap();
    text(&host.ctx, "sms", CODY, "status").await;
    assert_eq!(inbound::relay::run_once(&farm_b, 0).await.unwrap(), 1);
    let last = inbound_rows(&farm_b).await.pop().unwrap();
    assert_eq!((last.text.as_str(), last.status.as_str(), last.error.as_deref()), ("status", "ignored", Some("Texts in are off.")));
    assert!(replies_to(&farm_b, CODY).await.is_empty());
    let _ = farm_a;
}

#[tokio::test]
async fn a_code_texted_to_the_relay_verifies_the_phone_on_both_ends() {
    let host = notify_support::Host::start().await;
    let (_d, farm, _key) = relay_farm(&host, "Farm").await;
    let u = op_core::users::create_user(&farm, op_core::users::NewUser { name: "Mia".into(), role: Role::Manager, phone: Some(MIA.into()), email: None })
        .await
        .unwrap();
    // The farm asks the relay to text Mia a code (it has no Twilio of its own).
    assert_eq!(op_alerts::notify::verify::send_code(&farm, &u.id).await.unwrap().via, "relay");
    let code = notify_support::last_code(&host.twilio);
    // Mia texts it back to the relay's number: the host verifies her for this key and hands the text on.
    let r = text(&host.ctx, "sms", MIA, &code).await;
    assert_eq!(r.message.status, "received");
    let (vat,): (Option<String>,) =
        sqlx::query_as("SELECT verified_at FROM notify_recipients WHERE address = ?").bind(MIA).fetch_one(host.ctx.db()).await.unwrap();
    assert!(vat.is_some());
    assert_eq!(inbound::relay::run_once(&farm, 0).await.unwrap(), 1);
    assert!(op_core::users::get_user(&farm, &u.id).await.unwrap().unwrap().phone_verified_at.is_some());
    let r = replies_to(&farm, MIA).await;
    assert_eq!((r[0].channel.as_str(), r[0].text.as_str()), ("relay", "Phone verified. Reply STATUS any time."));
}

#[tokio::test]
async fn the_dead_man_texts_once_per_outage() {
    let host = notify_support::Host::start().await;
    let (_d, _farm, key) = relay_farm(&host, "Test farm").await;
    host_recipient(&host, &key, CODY, true).await;
    host_recipient(&host, &key, MIA, false).await;
    // Never polled: nothing to miss.
    assert!(op_alerts::hosting::inbox::deadman_pass(&host.ctx, now() + mins(60)).await.unwrap().is_empty());
    assert_eq!(inbox(&host, &key, "", 0).await.0, StatusCode::OK);
    let t = now();
    assert!(op_alerts::hosting::inbox::deadman_pass(&host.ctx, t + mins(10)).await.unwrap().is_empty());
    let sent = op_alerts::hosting::inbox::deadman_pass(&host.ctx, t + mins(16)).await.unwrap();
    assert_eq!(sent.len(), 1, "only the dead-man recipient");
    assert_eq!(sent[0].address, CODY);
    assert_eq!(sent[0].text, "openpasture: Test farm hasn't checked in for 16 min. Its power or internet may be down.");
    assert!(op_alerts::text::septets(&sent[0].text) <= 160);
    // Still down: no second text.
    assert!(op_alerts::hosting::inbox::deadman_pass(&host.ctx, t + mins(30)).await.unwrap().is_empty());
    assert!(op_alerts::hosting::inbox::deadman_pass(&host.ctx, t + mins(120)).await.unwrap().is_empty());
    // Back, then down again: a new outage, a new text.
    assert_eq!(inbox(&host, &key, "", 0).await.0, StatusCode::OK);
    assert!(op_alerts::hosting::inbox::deadman_pass(&host.ctx, now() + mins(5)).await.unwrap().is_empty());
    assert_eq!(op_alerts::hosting::inbox::deadman_pass(&host.ctx, now() + mins(20)).await.unwrap().len(), 1);
    // Relaying off: no dead-man.
    let (s, _) = call(&notify_support::app(&host.ctx), "PUT", "/api/notify/hosting", Some(json!({"enabled": false}))).await;
    assert_eq!(s, StatusCode::OK);
    sqlx::query("UPDATE relay_polls SET deadman_for = NULL").execute(host.ctx.db()).await.unwrap();
    assert!(op_alerts::hosting::inbox::deadman_pass(&host.ctx, now() + mins(60)).await.unwrap().is_empty());
}

// ---- the brief -------------------------------------------------------------------------------

#[tokio::test]
async fn the_brief_goes_at_the_farm_time_once_a_day() {
    let t = farm().await;
    let ctx = t.ctx();
    let (s, _) = t.f.owner("PUT", "/api/texting", Some(json!({"brief": {"enabled": true, "time": "06:30"}}))).await;
    assert_eq!(s, StatusCode::OK);
    for p in [MIA, PAT] {
        let (s, _) = t.f.owner("PUT", &format!("/api/texting/people/{}", t.id(p)), Some(json!({"brief": true}))).await;
        assert_eq!(s, StatusCode::OK);
    }
    let at = next_0630(ctx).await;
    assert!(op_alerts::brief_send::run_once(ctx, at - mins(1)).await.unwrap().is_empty());
    let sent = op_alerts::brief_send::run_once(ctx, at).await.unwrap();
    // Mia only: Pat's phone isn't verified.
    assert_eq!(sent.len(), 1);
    let m = &sent[0];
    assert_eq!((m.address.as_str(), m.kind.as_str(), m.channel.as_str()), (MIA, "brief", "sms"));
    let herd = ctx.store().get_herd(&t.f.herd).await.unwrap().unwrap();
    assert_eq!(m.text, op_engine::brief::brief(ctx, &herd, at).await.unwrap().text);
    assert!(m.text.starts_with("Cows: "), "{}", m.text);
    assert!(op_alerts::text::septets(&m.text) <= 480);
    // Once a day.
    assert!(op_alerts::brief_send::run_once(ctx, at + mins(1)).await.unwrap().is_empty());
    // A server down at 06:30 sends it on return within two hours, not later.
    assert_eq!(op_alerts::brief_send::run_once(ctx, at + Duration::days(1) + mins(90)).await.unwrap().len(), 1);
    assert!(op_alerts::brief_send::run_once(ctx, at + Duration::days(2) + mins(150)).await.unwrap().is_empty());
    // A brief is a prompt: it opens the reply window for a bare Y.
    op_core::messages::mark(ctx, &m.id, "sent", Some("SM1"), None, None).await.unwrap();
    assert!(op_alerts::inbound::act::last_prompt_to(ctx, MIA).await.unwrap().is_some());
    // Off at the farm: nothing.
    let (s, _) = t.f.owner("PUT", "/api/texting", Some(json!({"brief": {"enabled": false}}))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(op_alerts::brief_send::run_once(ctx, at + Duration::days(3)).await.unwrap().is_empty());
}

// ---- settings --------------------------------------------------------------------------------

#[tokio::test]
async fn texting_settings_say_how_replies_come_in() {
    let t = farm().await;
    let ctx = t.ctx();
    let (s, v) = t.f.owner("GET", "/api/texting", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((v["inbound"].clone(), v["poll_s"].clone(), v["approve_window_h"].clone()), (json!(true), json!(10), json!(12)));
    assert_eq!(v["brief"], json!({"enabled": false, "time": "06:30"}));
    assert_eq!(v["inbound_mode"], "polling");
    assert!(v.get("hooks").is_none());
    assert_eq!(v["people"].as_array().unwrap().len(), 5);

    ctx.update_settings(&json!({"server": {"public_url": "https://farm.example"}})).await.unwrap();
    let (_, v) = t.f.owner("GET", "/api/texting", None).await;
    assert_eq!(v["inbound_mode"], "webhook");
    assert_eq!(v["hooks"], json!({"sms": "https://farm.example/hooks/twilio/sms"}));
    // The view sent back unchanged is fine; read-only parts are ignored.
    let (s, back) = t.f.owner("PUT", "/api/texting", Some(v.clone())).await;
    assert_eq!(s, StatusCode::OK, "{back}");
    assert_eq!(back, v);
    let (s, v) = t.f.owner("PUT", "/api/texting", Some(json!({"inbound": false}))).await;
    assert_eq!((s, v["inbound_mode"].clone()), (StatusCode::OK, json!("off")));
    for bad in [json!({"poll_s": 1}), json!({"approve_window_h": 0}), json!({"brief": {"time": "6.30"}}), json!({"brief": {"time": "25:00"}})] {
        assert_eq!(t.f.owner("PUT", "/api/texting", Some(bad.clone())).await.0, StatusCode::BAD_REQUEST, "{bad}");
    }
    // Only the owner.
    let manager = a_engine_fixture::person_identity(Role::Manager, &t.id(MIA));
    assert_eq!(t.f.api(manager.clone(), "GET", "/api/texting", None).await.0, StatusCode::FORBIDDEN);
    assert_eq!(t.f.api(manager, "PUT", &format!("/api/texting/people/{}", t.id(MIA)), Some(json!({"brief": true}))).await.0, StatusCode::FORBIDDEN);
    assert_eq!(t.f.owner("PUT", "/api/texting/people/usr_nobody", Some(json!({"brief": true}))).await.0, StatusCode::NOT_FOUND);

    // No Twilio, relay on: the relay's inbox.
    let (s, _) = call(&notify_support::app(ctx), "PUT", "/api/notify/channels", Some(json!({"secrets": {"twilio_auth_token": null}}))).await;
    assert_eq!(s, StatusCode::OK);
    let (_, v) = t.f.owner("PUT", "/api/texting", Some(json!({"inbound": true}))).await;
    assert_eq!(v["inbound_mode"], "off");
    ctx.store().set_setting_json(op_core::notify_config::CHANNELS_KEY, &json!({"relay": {"enabled": true}})).await.unwrap();
    ctx.secrets().set("hosted_api_key", "oph_test").unwrap();
    let (_, v) = t.f.owner("GET", "/api/texting", None).await;
    assert_eq!(v["inbound_mode"], "relay");
}

#[test]
fn every_reply_is_gsm7_and_fits_with_worst_case_names() {
    use op_alerts::inbound::act::{HerdFacts, status_line};
    let fmt = op_core::units::Fmt::new(op_core::Units::Imperial);
    let long = "Cows “the big herd” — 🐄 from the north side of the river bottom ground";
    let herd = op_core::Herd {
        id: "herd_1".into(),
        name: long.into(),
        species: op_core::Species::Cattle,
        count: 250,
        paddock_id: None,
        autonomy: Default::default(),
        timer_minutes: 30,
        created_at: now(),
    };
    let f = HerdFacts {
        herd,
        paddock: Some(op_alerts::inbound::act::nm(long)),
        moving: Some((op_alerts::inbound::act::nm(long), 1234.5)),
        alerts: vec![long.into(), long.into(), long.into()],
        waiting: vec![format!("move to {}", op_alerts::inbound::act::nm(long))],
    };
    let line = status_line(&f, &fmt);
    assert!(op_alerts::text::is_gsm7(&line), "{line}");
    assert!(op_alerts::text::septets(&line) <= 320, "{} {line}", op_alerts::text::septets(&line));
    assert!(line.contains("4,050 ft to go"), "{line}");
    assert!(op_alerts::text::septets(&op_alerts::hosting::inbox::deadman_text(long, 1440)) <= 160);
    assert!(op_alerts::text::is_gsm7(&op_alerts::hosting::inbox::deadman_text(long, 1440)));
    for t in [op_alerts::inbound::act::OPTED_IN, op_alerts::inbound::act::OPTED_OUT, op_alerts::inbound::act::VERIFIED] {
        assert!(op_alerts::text::is_gsm7(t) && op_alerts::text::septets(t) <= 160, "{t}");
    }
}
