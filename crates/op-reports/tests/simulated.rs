//! Official reports never take in the simulator silently: a herd on
//! simulated collars only is left out (and the notes say so), animals on
//! simulated collars are left out of per-animal tables, and a herd with
//! some simulated collars is named in the notes.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Via, with_identity};
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
        op_reports::register_tools(&ctx);
        let router = with_identity(op_core::router().merge(op_reports::router()), Identity::owner(Via::Local)).with_state(ctx.clone());
        Self { _dir: dir, ctx, router }
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
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

    async fn ok(&self, method: &str, path: &str, body: Value) -> Value {
        let (s, v) = self.call(method, path, Some(body)).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    async fn report(&self, id: &str, q: &str) -> Value {
        let (s, v) = self.call("GET", &format!("/api/reports/{id}?{q}"), None).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }

    async fn herd(&self, name: &str, count: u32, paddock: &str) -> String {
        let h = self.ok("POST", "/api/herds", json!({"name": name, "species": "cattle", "count": count, "paddock_id": paddock})).await;
        let id = h["id"].as_str().unwrap().to_owned();
        sqlx::query("UPDATE herd_history SET at = '2025-09-01T12:00:00.000Z' WHERE herd_id = ?").bind(&id).execute(self.ctx.db()).await.unwrap();
        id
    }

    /// A collar on the herd, as the simulator links it or a real one.
    async fn collar(&self, herd: &str, simulated: bool) -> String {
        let id = op_core::id::new_id(op_core::id::COLLAR);
        sqlx::query("INSERT INTO collars (id, name, herd_id, key_hash, state, created_at, simulated) VALUES (?, ?, ?, ?, 'unknown', '2025-09-01T12:00:00.000Z', ?)")
            .bind(&id)
            .bind(&id)
            .bind(herd)
            .bind(format!("hash-{id}"))
            .bind(simulated)
            .execute(self.ctx.db())
            .await
            .unwrap();
        id
    }

    async fn animal(&self, herd: &str, tag: &str, collar: &str) {
        let a = op_core::Animal {
            id: op_core::id::new_id(op_core::id::ANIMAL),
            tag: tag.into(),
            herd_id: herd.into(),
            collar_id: Some(collar.into()),
            ..Default::default()
        };
        self.ctx.store().insert_animal(&a).await.unwrap();
    }
}

fn square(lon: f64, lat: f64) -> Value {
    json!({"type": "Polygon", "coordinates": [[[lon, lat], [lon + 0.005, lat], [lon + 0.005, lat + 0.0036], [lon, lat + 0.0036], [lon, lat]]]})
}

fn column(section: &Value, key: &str) -> Vec<Value> {
    let i = section["columns"].as_array().unwrap().iter().position(|c| c["key"] == key).unwrap();
    section["rows"].as_array().unwrap().iter().map(|r| r[i].clone()).collect()
}

fn notes(doc: &Value) -> Vec<String> {
    doc["notes"].as_array().unwrap().iter().map(|n| n.as_str().unwrap().to_owned()).collect()
}

const SEPT: &str = "from=2025-09-01&to=2025-09-30";

#[tokio::test]
async fn official_reports_leave_simulated_herds_out_and_say_so() {
    let app = App::new().await;
    app.ok("POST", "/api/farm", json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]})).await;
    let p1 = app.ok("POST", "/api/paddocks", json!({"name": "P1", "geometry": square(-93.625, 42.03)})).await["id"].as_str().unwrap().to_owned();
    let p2 = app.ok("POST", "/api/paddocks", json!({"name": "P2", "geometry": square(-93.62, 42.03)})).await["id"].as_str().unwrap().to_owned();
    let cows = app.herd("Cows", 100, &p1).await;
    let sim = app.herd("Sim herd", 12, &p2).await;
    let real = app.collar(&cows, false).await;
    for _ in 0..3 {
        app.collar(&sim, true).await;
    }

    // NRCS 528: only the real herd's field, and a note naming what was left out.
    let doc = app.report("nrcs_528", SEPT).await;
    let rec = &doc["sections"][0];
    assert_eq!(column(rec, "field"), [json!("P1")]);
    assert_eq!(column(rec, "number"), [json!(100)]);
    assert!(notes(&doc).contains(&"Left out: Sim herd (simulated collars only).".to_owned()), "{:?}", notes(&doc));

    // Every herd-history report the same way.
    for id in ["paddock_record", "organic_season"] {
        let doc = app.report(id, SEPT).await;
        let text = doc["sections"].to_string();
        assert!(!text.contains("P2") && !text.contains("Sim herd"), "{id}: {text}");
        assert!(notes(&doc).iter().any(|n| n.starts_with("Left out: Sim herd")), "{id}");
    }

    // A lease on the simulated herd's paddock has no grazing to bill.
    app.ok("PUT", &format!("/api/leases/{p2}"), json!({"landowner": "Jane Doe", "rate_per": "head_day", "rate_amount": 1.0})).await;
    let doc = app.report("lease_head_days", SEPT).await;
    let s = &doc["sections"][0];
    assert_eq!(column(s, "paddock"), [json!("P2")], "the lease is listed");
    assert_eq!((column(s, "head_days"), column(s, "amount")), (vec![json!(0.0)], vec![json!(0.0)]), "with nothing grazed: {s}");

    // Asked for by name: empty, and it says why.
    let doc = app.report("nrcs_528", &format!("{SEPT}&herd_id={sim}")).await;
    assert!(doc["sections"][0]["rows"].as_array().unwrap().is_empty());
    assert!(notes(&doc).iter().any(|n| n.starts_with("Left out: Sim herd")));
    // The real herd alone: nothing to say.
    let doc = app.report("nrcs_528", &format!("{SEPT}&herd_id={cows}")).await;
    assert!(!notes(&doc).iter().any(|n| n.contains("imulated")), "{:?}", notes(&doc));

    // A simulated collar on the real herd: it stays in, named in the notes, and
    // the animal wearing it is left out of the welfare record.
    let stray = app.collar(&cows, true).await;
    app.animal(&cows, "101", &real).await;
    app.animal(&cows, "901", &stray).await;
    let doc = app.report("nrcs_528", SEPT).await;
    assert_eq!(column(&doc["sections"][0], "field"), [json!("P1")]);
    assert!(notes(&doc).contains(&"Cows has 1 simulated collar: head counts include their animals; per-animal rows leave them out.".to_owned()), "{:?}", notes(&doc));
    let doc = app.report("welfare", SEPT).await;
    let tags: Vec<Value> = column(&doc["sections"][0], "tag");
    assert_eq!(tags, [json!("101")], "the animal on a simulated collar is no animal");
    assert!(notes(&doc).iter().any(|n| n.starts_with("Cows has 1 simulated collar")));
}
