//! Simulated collars (`op_core::sim`): the server decides at link time. A
//! simulated herd (made only on a server with no real data) takes only
//! simulated collars, a real herd only real ones; they carry
//! `simulated: true` in REST and live events and stay so. A report never
//! changes it: a real collar listing the `sim` cap stays real.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Ctx, Event};
use serde_json::{Value, json};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let router = op_core::router().merge(op_ingest::router()).with_state(ctx.clone());
        Self { _dir: dir, ctx, router }
    }

    async fn req(&self, method: &str, path: &str, key: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        if let Some(k) = key {
            req = req.header("authorization", format!("Bearer {k}"));
        }
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = self.router.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn farm(&self) {
        let (s, _) = self.req("POST", "/api/farm", None, Some(json!({"name": "Home", "timezone": "UTC", "center": [-92.405, 38.125]}))).await;
        assert_eq!(s, StatusCode::CREATED);
    }

    async fn new_herd(&self, name: &str, simulated: bool) -> (StatusCode, Value) {
        self.req("POST", "/api/herds", None, Some(json!({"name": name, "species": "cattle", "count": 1, "simulated": simulated}))).await
    }

    /// A farm and one herd, real or simulated.
    async fn herd(&self, simulated: bool) -> String {
        self.farm().await;
        let (s, herd) = self.new_herd("Cows", simulated).await;
        assert_eq!(s, StatusCode::CREATED, "{herd}");
        herd["id"].as_str().unwrap().to_owned()
    }

    async fn try_link(&self, herd: &str, simulated: Option<bool>) -> (StatusCode, Value) {
        let mut body = json!({ "herd_id": herd });
        if let Some(s) = simulated {
            body["simulated"] = json!(s);
        }
        self.req("POST", "/api/collars", None, Some(body)).await
    }

    /// (collar id, key, the collar as linked).
    async fn link(&self, herd: &str, simulated: Option<bool>) -> (String, String, Value) {
        let (s, c) = self.try_link(herd, simulated).await;
        assert_eq!(s, StatusCode::CREATED, "{c}");
        (c["collar"]["id"].as_str().unwrap().to_owned(), c["key"].as_str().unwrap().to_owned(), c["collar"].clone())
    }

    async fn get(&self, id: &str) -> Value {
        let (s, v) = self.req("GET", &format!("/api/collars/{id}"), None, None).await;
        assert_eq!(s, StatusCode::OK);
        v
    }

    async fn report(&self, key: &str, caps: &[&str]) {
        let body = json!({ "fixes": [], "battery": 0.7, "device": { "fw": "0.2.0", "caps": caps } });
        let (s, v) = self.req("POST", "/collar/v1/report", Some(key), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
}

const FW02: [&str; 6] = ["holes", "slots", "collar_id", "cue_mode", "episodes", "config"];

#[tokio::test]
async fn a_collar_linked_as_simulated_says_so_everywhere() {
    let app = App::new().await;
    let herd = app.herd(true).await;
    let mut bus = app.ctx.subscribe();
    let (id, _, linked) = app.link(&herd, Some(true)).await;
    assert_eq!(linked["simulated"], json!(true));
    assert_eq!(app.get(&id).await["simulated"], json!(true));
    let (_, list) = app.req("GET", &format!("/api/collars?herd_id={herd}"), None, None).await;
    assert_eq!(list[0]["simulated"], json!(true));
    // The live event carries it too.
    let ev = loop {
        match bus.recv().await.unwrap() {
            Event::Collar { collar } if collar.id == id => break collar,
            _ => {}
        }
    };
    assert!(ev.simulated);
    assert_eq!(serde_json::to_value(&ev).unwrap()["simulated"], json!(true));

    // A simulated herd takes no real collar.
    let (s, v) = app.try_link(&herd, None).await;
    assert_eq!(s, StatusCode::CONFLICT, "{v}");

    // A real herd beside it: real collars only, and they have no such field at all.
    let (s, cows) = app.new_herd("Real cows", false).await;
    assert_eq!(s, StatusCode::CREATED);
    let cows = cows["id"].as_str().unwrap().to_owned();
    let (s, v) = app.try_link(&cows, Some(true)).await;
    assert_eq!(s, StatusCode::CONFLICT, "a real herd takes no simulated collar: {v}");
    let (real, _, linked) = app.link(&cows, None).await;
    assert!(linked.get("simulated").is_none(), "{linked}");
    assert!(app.get(&real).await.get("simulated").is_none());

    // Nobody can PATCH it away, or move a collar across.
    let (s, _) = app.req("PATCH", &format!("/api/collars/{id}"), None, Some(json!({"simulated": false, "name": "Sim 1"}))).await;
    assert_eq!(s, StatusCode::OK);
    let c = app.get(&id).await;
    assert_eq!((c["simulated"].clone(), c["name"].clone()), (json!(true), json!("Sim 1")));
    let (_, c) = app.req("PATCH", &format!("/api/collars/{real}"), None, Some(json!({"simulated": true}))).await;
    assert!(c.get("simulated").is_none(), "a real collar can't be marked simulated by hand");
    for (collar, to) in [(&id, &cows), (&real, &herd)] {
        let (s, v) = app.req("PATCH", &format!("/api/collars/{collar}"), None, Some(json!({"herd_id": to}))).await;
        assert_eq!(s, StatusCode::CONFLICT, "{v}");
    }

    let sims = op_core::sim::collars(app.ctx.db()).await.unwrap();
    assert!(sims.contains(&id) && !sims.contains(&real));
    assert!(!op_core::sim::all_simulated(app.ctx.db()).await.unwrap());
}

#[tokio::test]
async fn simulated_herds_only_go_on_a_server_with_no_real_data() {
    let app = App::new().await;
    let real = app.herd(false).await;
    let (s, v) = app.new_herd("Sim", true).await;
    assert_eq!(s, StatusCode::CONFLICT, "a real herd is here: {v}");
    // Nor can a real herd be turned into one.
    let (_, h) = app.req("PATCH", &format!("/api/herds/{real}"), None, Some(json!({"simulated": true}))).await;
    assert!(h.get("simulated").is_none_or(|v| v == &json!(false)), "{h}");
    assert!(!op_core::sim::herd_is_simulated(app.ctx.db(), &real).await.unwrap());
    assert!(sqlx::query("UPDATE herds SET simulated = 1 WHERE id = ?").bind(&real).execute(app.ctx.db()).await.is_err(), "not even in the database");
}

/// Astra 6, 03 P1-03 / 02 F09: a real collar reported `sim` in its caps, was
/// classified simulated for good, and its alerts reached no one.
#[tokio::test]
async fn a_real_collar_reporting_the_sim_cap_stays_real() {
    let app = App::new().await;
    let herd = app.herd(false).await;
    let (id, key, _) = app.link(&herd, None).await;

    // Firmware 0.2 without the cap: real.
    app.report(&key, &FW02).await;
    assert!(app.get(&id).await.get("simulated").is_none());

    // The simulator's caps from a real collar's key: still real. The cap is kept as reported.
    let mut caps = FW02.to_vec();
    caps.extend(["sim", "lora"]);
    app.report(&key, &caps).await;
    let c = app.get(&id).await;
    assert!(c.get("simulated").is_none(), "{c}");
    assert!(c["caps"].as_array().unwrap().contains(&json!("sim")));

    // real -> sim cap -> real: still real, nothing muted or left out.
    app.report(&key, &FW02).await;
    assert!(app.get(&id).await.get("simulated").is_none());
    assert!(op_core::sim::collars(app.ctx.db()).await.unwrap().is_empty());
    assert!(op_core::sim::herds(app.ctx.db()).await.unwrap().is_empty());
    assert!(!op_core::sim::all_simulated(app.ctx.db()).await.unwrap());

    // And no write anywhere can make it simulated now.
    assert!(sqlx::query("UPDATE collars SET simulated = 1 WHERE id = ?").bind(&id).execute(app.ctx.db()).await.is_err());
}

#[tokio::test]
async fn a_reported_device_class_places_the_collar_in_the_herd_network() {
    let app = App::new().await;
    let herd = app.herd(true).await;
    let (anchor, akey, _) = app.link(&herd, Some(true)).await;
    let (node, nkey, _) = app.link(&herd, Some(true)).await;
    let (plain, pkey, _) = app.link(&herd, Some(true)).await;
    for (key, class) in [(&akey, Some("anchor")), (&nkey, Some("node")), (&pkey, None)] {
        let mut device = json!({ "fw": "0.2.0", "caps": FW02 });
        if let Some(c) = class {
            device["device_class"] = json!(c);
        }
        let (s, v) = app.req("POST", "/collar/v1/report", Some(key), Some(json!({ "fixes": [], "device": device }))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    let role = |id: String| {
        let db = app.ctx.db().clone();
        async move { sqlx::query_scalar::<_, String>("SELECT role FROM fleet_links WHERE collar_id = ?").bind(id).fetch_optional(&db).await.unwrap() }
    };
    assert_eq!(role(anchor).await.as_deref(), Some("anchor"));
    assert_eq!(role(node).await.as_deref(), Some("node"));
    assert_eq!(role(plain).await, None, "firmware 0.2 sends no class: the network view works it out");
}
