//! Web Push end to end: a browser (a P-256 key and an auth secret, as
//! `PushManager.subscribe` makes them) subscribes through the API, an alert
//! routes to it, the sender POSTs to an in-test push service, and that
//! service checks the VAPID JWT (RFC 8292) and decrypts the payload the way
//! the browser does (RFC 8291). Product code runs unchanged.

mod a_engine_fixture;

use std::sync::{Arc, Mutex};

use a_engine_fixture::*;
use axum::Router;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hmac::{Hmac, Mac};
use op_alerts::notify::sender;
use op_core::{Identity, Role, Via};
use ring::agreement::{self, EphemeralPrivateKey};
use ring::rand::{SecureRandom, SystemRandom};
use ring::{aead, signature};
use serde_json::{Value, json};
use sha2::Sha256;

// ---- the browser ------------------------------------------------------------------------

/// A browser's push keys. Its private key decrypts one message (ring's ECDH
/// keys are single-use), which is all each test needs per subscription.
struct Browser {
    key: Mutex<Option<EphemeralPrivateKey>>,
    public: Vec<u8>,
    auth: [u8; 16],
}

fn hmac256(key: &[u8], parts: &[&[u8]]) -> Vec<u8> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).unwrap();
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().to_vec()
}

impl Browser {
    fn new() -> Self {
        let rng = SystemRandom::new();
        let key = EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng).unwrap();
        let public = key.compute_public_key().unwrap().as_ref().to_vec();
        let mut auth = [0u8; 16];
        rng.fill(&mut auth).unwrap();
        Self { key: Mutex::new(Some(key)), public, auth }
    }

    /// `PushSubscription.toJSON()`.
    fn subscription(&self, endpoint: &str) -> Value {
        json!({ "endpoint": endpoint, "expirationTime": null, "keys": { "p256dh": B64.encode(&self.public), "auth": B64.encode(self.auth) } })
    }

    /// Decrypt an aes128gcm body sent to this browser (RFC 8291 §3.4, RFC 8188 §2).
    fn decrypt(&self, body: &[u8]) -> Value {
        let salt = &body[..16];
        let rs = u32::from_be_bytes(body[16..20].try_into().unwrap());
        assert!(rs >= 18, "record size");
        let idlen = body[20] as usize;
        assert_eq!(idlen, 65, "the key id is the server's P-256 key");
        let as_public = &body[21..21 + idlen];
        let ct = &body[21 + idlen..];
        assert!(ct.len() <= rs as usize, "one record");
        let key = self.key.lock().unwrap().take().expect("this browser already decrypted a message");
        let peer = agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, as_public);
        let ecdh = agreement::agree_ephemeral(key, &peer, |s| s.to_vec()).unwrap();
        let prk_key = hmac256(&self.auth, &[&ecdh]);
        let ikm = hmac256(&prk_key, &[b"WebPush: info\0", &self.public, as_public, &[1]]);
        let prk = hmac256(salt, &[&ikm]);
        let cek = hmac256(&prk, &[b"Content-Encoding: aes128gcm\0", &[1]]);
        let nonce = hmac256(&prk, &[b"Content-Encoding: nonce\0", &[1]]);
        let k = aead::LessSafeKey::new(aead::UnboundKey::new(&aead::AES_128_GCM, &cek[..16]).unwrap());
        let mut buf = ct.to_vec();
        let plain = k.open_in_place(aead::Nonce::try_assume_unique_for_key(&nonce[..12]).unwrap(), aead::Aad::empty(), &mut buf).expect("decrypts");
        // The last record ends with 0x02, then optional zero padding.
        let end = plain.iter().rposition(|b| *b != 0).unwrap();
        assert_eq!(plain[end], 2, "last-record delimiter");
        serde_json::from_slice(&plain[..end]).unwrap()
    }
}

// ---- the push service ---------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Push {
    path: String,
    headers: HeaderMap,
    body: Bytes,
}

impl Push {
    fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
    }
}

#[derive(Clone, Default)]
struct Service {
    url: String,
    got: Arc<Mutex<Vec<Push>>>,
    answer: Arc<Mutex<u16>>,
}

async fn take(State(s): State<Service>, Path(n): Path<String>, headers: HeaderMap, body: Bytes) -> impl IntoResponse {
    s.got.lock().unwrap().push(Push { path: format!("/push/{n}"), headers, body });
    let code = *s.answer.lock().unwrap();
    (StatusCode::from_u16(code).unwrap(), [("location", format!("/m/{n}"))], "")
}

impl Service {
    async fn start() -> Self {
        let mut s = Service { answer: Arc::new(Mutex::new(201)), ..Default::default() };
        let app = Router::new().route("/push/{n}", axum::routing::post(take)).with_state(s.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        s.url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        s
    }
    fn endpoint(&self, n: u32) -> String {
        format!("{}/push/{n}", self.url)
    }
    fn got(&self) -> Vec<Push> {
        self.got.lock().unwrap().clone()
    }
    fn answer(&self, code: u16) {
        *self.answer.lock().unwrap() = code;
    }
}

/// What a push service checks before it takes a message (RFC 8292 §2–3):
/// `Authorization: vapid t=<JWT>, k=<key>`, an ES256 JWT whose signature
/// verifies with `k`, `aud` its own origin, `exp` within 24 h, and a `sub`.
/// Returns the claims.
fn vapid(p: &Push, aud: &str, key: &str) -> Value {
    let auth = p.header("authorization").expect("Authorization");
    let rest = auth.strip_prefix("vapid ").expect("vapid scheme");
    let mut t = None;
    let mut k = None;
    for part in rest.split(',').map(str::trim) {
        if let Some(v) = part.strip_prefix("t=") {
            t = Some(v.to_owned());
        } else if let Some(v) = part.strip_prefix("k=") {
            k = Some(v.to_owned());
        }
    }
    let (t, k) = (t.expect("t="), k.expect("k="));
    assert_eq!(k, key, "the farm's public key");
    let parts: Vec<&str> = t.split('.').collect();
    assert_eq!(parts.len(), 3, "a JWS");
    let head: Value = serde_json::from_slice(&B64.decode(parts[0]).unwrap()).unwrap();
    assert_eq!(head["alg"], "ES256");
    let claims: Value = serde_json::from_slice(&B64.decode(parts[1]).unwrap()).unwrap();
    let sig = B64.decode(parts[2]).unwrap();
    signature::UnparsedPublicKey::new(&signature::ECDSA_P256_SHA256_FIXED, B64.decode(&k).unwrap())
        .verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
        .expect("the JWT is signed with k");
    assert_eq!(claims["aud"], aud);
    let exp = claims["exp"].as_i64().unwrap();
    let now = chrono::Utc::now().timestamp();
    assert!(exp > now && exp <= now + 24 * 3600, "exp within 24 h: {exp} vs {now}");
    let sub = claims["sub"].as_str().unwrap();
    assert!(sub.starts_with("https://") || sub.starts_with("mailto:"), "{sub}");
    claims
}

// ---- helpers ---------------------------------------------------------------------------------

const PUBLIC_URL: &str = "https://farm.example.com";

fn me(role: Role, user: &str) -> Identity {
    // A browser on this machine: plain-http push services (this test's) are allowed only then.
    Identity { role, user_id: Some(user.to_owned()), name: None, via: Via::Local }
}

async fn subscribe(f: &Farm, user: &str, role: Role, b: &Browser, endpoint: &str) -> String {
    let (s, v) = f.api(me(role, user), "POST", "/api/push/subscriptions", Some(b.subscription(endpoint))).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    v["id"].as_str().unwrap().to_owned()
}

async fn key(f: &Farm) -> String {
    let (_, v) = f.owner("GET", "/api/push", None).await;
    v["vapid_public_key"].as_str().expect("a public key").to_owned()
}

/// An escaped animal (critical), opened and routed at `t0`.
async fn escaped(f: &Farm) -> String {
    let c = f.collar(Some("214"), t0()).await;
    f.outside(&c, t0() - mins(2), t0()).await;
    f.escape(&c, "returning", t0() - mins(1), None).await;
    f.eval(t0()).await;
    f.open_kind("escaped").await[0].id.clone()
}

async fn send_all(f: &Farm) {
    for _ in 0..3 {
        sender::run_once(&f.ctx, chrono::Utc::now()).await.unwrap();
    }
}

// ---- tests -------------------------------------------------------------------------------------

#[tokio::test]
async fn an_alert_reaches_a_subscribed_phone_signed_and_encrypted() {
    let f = Farm::new().await;
    f.ctx.set_public_url(Some(PUBLIC_URL.into()));
    let svc = Service::start().await;
    let mia = f.person("Mia", Role::Manager, None, false, None).await;
    let phone = Browser::new();
    let sub = subscribe(&f, &mia, Role::Manager, &phone, &svc.endpoint(1)).await;
    let k = key(&f).await;
    // Turning alerts on in a browser adds push to the person's channels.
    let (prefs, _) = op_alerts::routing::prefs::get(&f.ctx, &mia).await.unwrap();
    assert_eq!(prefs.channels, ["sms", "push"]);
    assert_eq!(op_core::notify_config::configured_channels(&f.ctx).await.unwrap(), ["push"]);

    let alert = escaped(&f).await;
    f.route(t0() + secs(10)).await;
    let m = f.messages_to(&mia).await;
    assert_eq!(m.iter().map(|x| (x.channel.as_str(), x.address.as_str())).collect::<Vec<_>>(), [("push", sub.as_str())], "no phone, only push");
    send_all(&f).await;

    let got = svc.got();
    assert_eq!(got.len(), 1, "{got:#?}");
    let p = &got[0];
    assert_eq!(p.path, "/push/1");
    let claims = vapid(p, &svc.url, &k);
    assert_eq!(claims["sub"], PUBLIC_URL);
    assert_eq!(p.header("content-encoding").as_deref(), Some("aes128gcm"));
    assert_eq!(p.header("ttl").as_deref(), Some("14400"));
    assert_eq!(p.header("urgency").as_deref(), Some("high"));
    assert_eq!(p.header("topic").as_deref(), Some(alert.as_str()), "a later message for this alert replaces an unfetched one");
    assert!(p.body.len() <= 4096);

    let n = phone.decrypt(&p.body);
    assert_eq!(n["title"], "Test farm");
    assert_eq!(n["tag"], alert.as_str());
    assert_eq!(n["alert_id"], alert.as_str());
    assert_eq!(n["herd_id"], f.herd.as_str());
    assert_eq!(n["url"], format!("/#/map?alert={alert}"));
    let body = n["body"].as_str().unwrap();
    assert!(body.starts_with("214 outside P1"), "{body}");
    assert!(!body.contains("Reply"), "a notification is answered in the app: {body}");

    // Sent, and the subscription was last good now.
    let m = f.messages_to(&mia).await;
    assert_eq!((m[0].status.as_str(), m[0].provider_id.as_deref()), ("sent", Some("/m/1")));
    let (last_ok,): (Option<String>,) = sqlx::query_as("SELECT last_ok FROM push_subscriptions WHERE id = ?").bind(&sub).fetch_one(f.ctx.db()).await.unwrap();
    assert!(last_ok.is_some());
}

#[tokio::test]
async fn push_is_offered_only_over_https_and_once_a_browser_subscribed() {
    let f = Farm::new().await;
    let svc = Service::start().await;
    let mia = f.person("Mia", Role::Manager, None, false, None).await;
    // Plain http (a LAN address): browsers won't subscribe, so nothing is offered.
    let (s, v) = f.api(me(Role::Manager, &mia), "GET", "/api/push", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((v["available"].clone(), v["key_set"].clone()), (json!(false), json!(false)), "{v}");
    assert!(v["vapid_public_key"].is_null());
    assert!(v["reason"].as_str().unwrap().contains("https"));
    let (s, v) = f.api(me(Role::Manager, &mia), "POST", "/api/push/subscriptions", Some(Browser::new().subscription(&svc.endpoint(1)))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");

    // Over https the key is made on first ask, but push isn't a channel until someone subscribes.
    f.ctx.set_public_url(Some(PUBLIC_URL.into()));
    let (_, v) = f.api(me(Role::Manager, &mia), "GET", "/api/push", None).await;
    assert_eq!((v["available"].clone(), v["key_set"].clone()), (json!(true), json!(true)), "{v}");
    let k = v["vapid_public_key"].as_str().unwrap().to_owned();
    assert_eq!(B64.decode(&k).unwrap().len(), 65);
    let (_, again) = f.owner("GET", "/api/push", None).await;
    assert_eq!(again["vapid_public_key"], k.as_str(), "one key per farm");
    assert!(f.ctx.secrets().get("vapid_private_key").unwrap().is_some());
    assert!(!op_core::notify_config::configured_channels(&f.ctx).await.unwrap().contains(&"push"));
    let (_, rules) = f.owner("GET", "/api/alerts/rules", None).await;
    assert_eq!(rules["person_channels"], json!([]));

    let sub = subscribe(&f, &mia, Role::Manager, &Browser::new(), &svc.endpoint(1)).await;
    assert!(op_core::notify_config::configured_channels(&f.ctx).await.unwrap().contains(&"push"));
    let (_, rules) = f.owner("GET", "/api/alerts/rules", None).await;
    assert_eq!(rules["person_channels"], json!(["push"]));
    let (_, v) = f.api(me(Role::Manager, &mia), "GET", "/api/push", None).await;
    assert_eq!(v["mine"].as_array().unwrap().len(), 1);
    assert_eq!(v["mine"][0]["id"], sub.as_str());
    assert_eq!(v["mine"][0]["endpoint"], svc.endpoint(1));

    // The same browser again (keys renewed) is the same subscription.
    let again = subscribe(&f, &mia, Role::Manager, &Browser::new(), &svc.endpoint(1)).await;
    assert_eq!(again, sub);
    // Back on plain http, push stops being a channel.
    f.ctx.set_public_url(None);
    assert!(!op_core::notify_config::configured_channels(&f.ctx).await.unwrap().contains(&"push"));
}

#[tokio::test]
async fn a_subscription_the_browser_dropped_is_removed_and_its_message_fails() {
    let f = Farm::new().await;
    f.ctx.set_public_url(Some(PUBLIC_URL.into()));
    let svc = Service::start().await;
    let mia = f.person("Mia", Role::Manager, None, false, None).await;
    let sub = subscribe(&f, &mia, Role::Manager, &Browser::new(), &svc.endpoint(7)).await;
    svc.answer(410);
    escaped(&f).await;
    f.route(t0() + secs(10)).await;
    send_all(&f).await;
    assert_eq!(svc.got().len(), 1);
    let m = f.messages_to(&mia).await;
    assert_eq!((m[0].status.as_str(), m[0].error.as_deref()), ("failed", Some("That phone stopped taking notifications.")));
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM push_subscriptions WHERE id = ?").bind(&sub).fetch_one(f.ctx.db()).await.unwrap();
    assert_eq!(left, 0);
    // A push service that is down is tried again later instead.
    let f = Farm::new().await;
    f.ctx.set_public_url(Some(PUBLIC_URL.into()));
    let mia = f.person("Mia", Role::Manager, None, false, None).await;
    subscribe(&f, &mia, Role::Manager, &Browser::new(), &svc.endpoint(8)).await;
    svc.answer(503);
    escaped(&f).await;
    f.route(t0() + secs(10)).await;
    sender::run_once(&f.ctx, chrono::Utc::now()).await.unwrap();
    let m = f.messages_to(&mia).await;
    assert_eq!((m[0].status.as_str(), m[0].error.as_deref()), ("queued", Some("The push service said 503.")));
}

#[tokio::test]
async fn plain_http_push_services_are_refused_unless_asked_from_this_machine() {
    let f = Farm::new().await;
    f.ctx.set_public_url(Some(PUBLIC_URL.into()));
    let hank = f.person("Hank", Role::Hand, None, false, None).await;
    let b = Browser::new();
    let remote = Identity { role: Role::Hand, user_id: Some(hank.clone()), name: None, via: Via::UserToken };
    // Aimed at this server's own API, a push would be a local (owner) request: never from a phone.
    let (s, v) = f.api(remote.clone(), "POST", "/api/push/subscriptions", Some(b.subscription("http://127.0.0.1:7878/api/herds/h/move/stop"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    assert_eq!(v["error"], "Push services are https.");
    let (s, _) = f.api(remote.clone(), "POST", "/api/push/subscriptions", Some(b.subscription("https://fcm.googleapis.com/fcm/send/abc:def"))).await;
    assert_eq!(s, StatusCode::CREATED);
    // Keys that aren't a browser's are refused when they arrive.
    let mut bad = b.subscription("https://fcm.googleapis.com/fcm/send/other");
    bad["keys"]["auth"] = json!("c2hvcnQ");
    let (s, v) = f.api(remote.clone(), "POST", "/api/push/subscriptions", Some(bad)).await;
    assert_eq!((s, v["error"].as_str()), (StatusCode::BAD_REQUEST, Some("keys.auth must be 16 bytes.")));
    // Nobody to send to: a local owner without a person row.
    let (s, v) = f.owner("POST", "/api/push/subscriptions", Some(b.subscription("https://fcm.googleapis.com/fcm/send/x"))).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");
    // The owner through the app token isn't on this machine either.
    let cody = f.person("Cody", Role::Owner, None, false, None).await;
    let token = Identity { role: Role::Owner, user_id: Some(cody), name: None, via: Via::AppToken };
    let (s, v) = f.api(token, "POST", "/api/push/subscriptions", Some(b.subscription("http://127.0.0.1:9/push/1"))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
}

#[tokio::test]
async fn people_remove_only_their_own_subscriptions_and_the_owner_any() {
    let f = Farm::new().await;
    f.ctx.set_public_url(Some(PUBLIC_URL.into()));
    let svc = Service::start().await;
    let mia = f.person("Mia", Role::Manager, None, false, None).await;
    let hank = f.person("Hank", Role::Hand, None, false, None).await;
    let vera = f.person("Vera", Role::Viewer, None, false, None).await;
    let cody = f.person("Cody", Role::Owner, None, false, None).await;
    let m1 = subscribe(&f, &mia, Role::Manager, &Browser::new(), &svc.endpoint(1)).await;
    let m2 = subscribe(&f, &mia, Role::Manager, &Browser::new(), &svc.endpoint(2)).await;
    // A viewer may turn alerts on in their own browser too.
    let v1 = subscribe(&f, &vera, Role::Viewer, &Browser::new(), &svc.endpoint(3)).await;
    let (_, v) = f.api(me(Role::Hand, &hank), "GET", "/api/push", None).await;
    assert_eq!(v["mine"], json!([]), "only your own");
    let (s, _) = f.api(me(Role::Hand, &hank), "DELETE", &format!("/api/push/subscriptions/{m1}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = f.api(me(Role::Hand, &hank), "POST", &format!("/api/push/subscriptions/{m1}/test"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = f.api(me(Role::Manager, &mia), "DELETE", &format!("/api/push/subscriptions/{m1}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = f.api(me(Role::Owner, &cody), "DELETE", &format!("/api/push/subscriptions/{v1}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, v) = f.api(me(Role::Manager, &mia), "GET", "/api/push", None).await;
    assert_eq!(v["mine"].as_array().unwrap().iter().map(|x| x["id"].as_str().unwrap()).collect::<Vec<_>>(), [m2.as_str()]);
    // Removing a person removes their subscriptions.
    sqlx::query("DELETE FROM users WHERE id = ?").bind(&mia).execute(f.ctx.db()).await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM push_subscriptions").fetch_one(f.ctx.db()).await.unwrap();
    assert_eq!(left, 0);
}

#[tokio::test]
async fn a_test_notification_goes_to_that_browser_now() {
    let f = Farm::new().await;
    f.ctx.set_public_url(Some(PUBLIC_URL.into()));
    let svc = Service::start().await;
    let hank = f.person("Hank", Role::Hand, None, false, None).await;
    let b = Browser::new();
    let sub = subscribe(&f, &hank, Role::Hand, &b, &svc.endpoint(4)).await;
    let k = key(&f).await;
    let (s, v) = f.api(me(Role::Hand, &hank), "POST", &format!("/api/push/subscriptions/{sub}/test"), None).await;
    assert_eq!((s, v["ok"].clone()), (StatusCode::OK, json!(true)), "{v}");
    let got = svc.got();
    assert_eq!(got.len(), 1);
    vapid(&got[0], &svc.url, &k);
    assert_eq!(got[0].header("urgency").as_deref(), Some("normal"));
    assert_eq!(got[0].header("topic"), None);
    let n = b.decrypt(&got[0].body);
    assert_eq!((n["title"].as_str(), n["body"].as_str(), n["url"].as_str()), (Some("Test farm"), Some("openpasture test."), Some("/#/map")));
    let m = f.messages_to(&hank).await;
    assert_eq!((m[0].channel.as_str(), m[0].kind.as_str(), m[0].status.as_str()), ("push", "test", "sent"));
}

#[tokio::test]
async fn the_morning_brief_goes_by_push_without_its_texting_line() {
    let f = Farm::new().await;
    f.ctx.set_public_url(Some(PUBLIC_URL.into()));
    let svc = Service::start().await;
    let mia = f.person("Mia", Role::Manager, None, false, None).await;
    let b = Browser::new();
    subscribe(&f, &mia, Role::Manager, &b, &svc.endpoint(5)).await;
    f.ctx.store().set_setting_json("texting", &json!({"brief": {"enabled": true, "time": "07:00"}})).await.unwrap();
    op_alerts::brief_send::set_brief(&f.ctx, &mia, true).await.unwrap();
    f.decision("MOVE", "proposed", t0() - mins(5), None).await;
    let queued = op_alerts::brief_send::run_once(&f.ctx, t0() + mins(1)).await.unwrap();
    assert_eq!(queued.iter().map(|m| (m.channel.as_str(), m.kind.as_str())).collect::<Vec<_>>(), [("push", "brief")]);
    assert!(queued[0].text.contains("Reply Y or N."), "{}", queued[0].text);
    send_all(&f).await;
    let got = svc.got();
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].header("ttl").as_deref(), Some("43200"));
    let n = b.decrypt(&got[0].body);
    let body = n["body"].as_str().unwrap();
    assert!(body.starts_with("Cows: "), "{body}");
    assert!(!body.contains("Reply"), "{body}");
}

#[tokio::test]
async fn push_settings_are_the_owners_and_new_keys_drop_every_browser() {
    let f = Farm::new().await;
    f.ctx.set_public_url(Some(PUBLIC_URL.into()));
    let svc = Service::start().await;
    let mia = f.person("Mia", Role::Manager, None, false, None).await;
    subscribe(&f, &mia, Role::Manager, &Browser::new(), &svc.endpoint(6)).await;
    let k = key(&f).await;
    let (s, _) = f.api(me(Role::Manager, &mia), "PUT", "/api/push/settings", Some(json!({"enabled": false}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = f.owner("PUT", "/api/push/settings", Some(json!({"enabled": false}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["available"].clone(), v["enabled"].clone()), (json!(false), json!(false)));
    assert!(!op_core::notify_config::configured_channels(&f.ctx).await.unwrap().contains(&"push"));
    let (_, v) = f.owner("PUT", "/api/push/settings", Some(json!({"enabled": true, "new_keys": true}))).await;
    assert_eq!(v["available"], true);
    assert_ne!(v["vapid_public_key"].as_str().unwrap(), k, "a new key");
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM push_subscriptions").fetch_one(f.ctx.db()).await.unwrap();
    assert_eq!(left, 0, "browsers subscribed with the old key must subscribe again");
    let (s, _) = f.owner("PUT", "/api/push/settings", Some(json!({"colour": "red"}))).await;
    assert_ne!(s, StatusCode::OK);
}
