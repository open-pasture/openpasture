//! The pre-send check through the real routes (field-ready §2.13): `sent` is
//! what the send stores (exclusions, a staged temporary exclusion, the end
//! of a sweep), the facts and the engine's findings (forage, area per head,
//! rest, weak GPS), the legacy ring, the sweep preview and its pace, the MCP
//! tool, and the time a check takes with 250 animals and 30 features.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Polygon, Role, Via, time};
use op_geo::Projection;
use op_ingest::NewLinked;
use serde_json::{Value, json};
use tower::ServiceExt;

/// The live check's farm centre near Ames (and the coverage grid's origin).
const CENTER: [f64; 2] = [-93.62, 42.03];

/// Metres east and north of the farm centre. P1 lies west of it: x -413..0, y 0..400.
fn at(x: f64, y: f64) -> [f64; 2] {
    Projection::new(CENTER).offset(x, y)
}

fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Value {
    json!({ "type": "Polygon", "coordinates": [[at(x0, y0), at(x1, y0), at(x1, y1), at(x0, y1), at(x0, y0)]] })
}

fn p1() -> Value {
    json!({ "type": "Polygon", "coordinates": [[[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]]] })
}

fn p2() -> Value {
    json!({ "type": "Polygon", "coordinates": [[[-93.62, 42.03], [-93.615, 42.03], [-93.615, 42.0336], [-93.62, 42.0336], [-93.62, 42.03]]] })
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn codes(v: &Value) -> Vec<&str> {
    v["findings"].as_array().unwrap().iter().map(|f| f["code"].as_str().unwrap()).collect()
}

fn finding<'a>(v: &'a Value, code: &str) -> &'a Value {
    v["findings"].as_array().unwrap().iter().find(|f| f["code"] == code).unwrap_or_else(|| panic!("no {code} in {:?}", codes(v)))
}

fn polygon(v: &Value) -> Polygon {
    serde_json::from_value(v.clone()).unwrap()
}

struct T {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    app: Router,
    herd: String,
    p1: String,
    p2: String,
}

impl T {
    /// The §6 farm: P1 and P2 around Ames, Cows (250) in P1, units metric.
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let app = Router::new().merge(op_core::router()).merge(op_ingest::router()).merge(op_engine::router()).with_state(ctx.clone());
        let app = op_core::with_identity(app, Identity::owner(Via::Local));
        let mut t = T { _dir: dir, ctx, app, herd: String::new(), p1: String::new(), p2: String::new() };
        t.ok("POST", "/api/farm", json!({ "name": "Test farm", "timezone": "America/Chicago", "center": CENTER })).await;
        t.ctx.update_settings(&json!({ "units": "metric" })).await.unwrap();
        let a = t.ok("POST", "/api/paddocks", json!({ "name": "P1", "geometry": p1() })).await;
        let b = t.ok("POST", "/api/paddocks", json!({ "name": "P2", "geometry": p2() })).await;
        let h = t.ok("POST", "/api/herds", json!({ "name": "Cows", "species": "cattle", "count": 250, "paddock_id": a["id"] })).await;
        t.p1 = a["id"].as_str().unwrap().into();
        t.p2 = b["id"].as_str().unwrap().into();
        t.herd = h["id"].as_str().unwrap().into();
        t
    }

    async fn req(&self, method: &str, path: &str, key: Option<&str>, body: Option<Value>) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path).header("host", "127.0.0.1");
        if let Some(k) = key {
            b = b.header("authorization", format!("Bearer {k}"));
        }
        let req = match body {
            Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())),
            None => b.body(Body::empty()),
        }
        .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn ok(&self, method: &str, path: &str, body: Value) -> Value {
        let (s, v) = self.req(method, path, None, Some(body)).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    async fn check(&self, body: Value) -> Value {
        self.ok("POST", &format!("/api/herds/{}/check", self.herd), body).await
    }

    async fn send(&self, body: Value) -> Value {
        self.ok("POST", &format!("/api/herds/{}/boundary", self.herd), body).await
    }

    async fn status(&self) -> Value {
        self.req("GET", &format!("/api/herds/{}/boundary", self.herd), None, None).await.1
    }

    async fn feature(&self, body: Value) {
        self.ok("POST", "/api/features", body).await;
    }

    /// `n` collars on the herd; `v0` ones say they are firmware 0.2.
    async fn collars(&self, n: usize, v0: bool) -> Vec<String> {
        let items: Vec<NewLinked> = (0..n).map(|i| NewLinked { name: format!("{}", 100 + i), animal_id: None }).collect();
        let made = op_ingest::create_linked_collars(&self.ctx, &self.herd, &items).await.unwrap();
        let keys: Vec<String> = made.into_iter().map(|(_, k)| k).collect();
        if v0 {
            let device =
                json!({ "fw": "0.2.0", "caps": ["holes", "slots", "collar_id", "cue_mode", "episodes", "config"], "limits": op_geo::CollarLimits::V0 });
            for k in &keys {
                self.report(k, json!({ "device": device })).await;
            }
        }
        keys
    }

    async fn report(&self, key: &str, body: Value) {
        let (s, v) = self.req("POST", "/collar/v1/report", Some(key), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    async fn fix(&self, key: &str, p: [f64; 2]) {
        self.report(key, json!({ "fixes": [{ "at": ts(Utc::now()), "point": p, "accuracy_m": 2.0, "sats": 9 }] })).await;
    }
}

/// A cached land report for the paddock with this mean NDVI.
async fn ndvi_report(ctx: &Ctx, paddock_id: &str, ndvi: f64) {
    let now = time::to_db(&time::now());
    let report = json!({
        "report_id": format!("lr_{paddock_id}"), "paddock_id": paddock_id, "source": "alexandria", "as_of": now, "geometry": p1(),
        "sections": { "imagery": { "status": "ok", "ndvi_stats": { "mean": ndvi }, "latest": { "captured_at": "2026-09-20" }, "sources": [] } },
    });
    sqlx::query("INSERT INTO land_reports (id, paddock_id, cache_key, source, as_of, report, created_at) VALUES (?, ?, 'test', 'alexandria', ?, ?, ?)")
        .bind(format!("lr_{paddock_id}"))
        .bind(paddock_id)
        .bind(&now)
        .bind(report.to_string())
        .bind(&now)
        .execute(ctx.db())
        .await
        .unwrap();
}

#[tokio::test]
async fn the_check_sends_what_the_send_stores() {
    let t = T::new().await;
    t.feature(json!({ "kind": "exclusion", "name": "wet spot", "paddock_id": t.p1, "geometry": rect(-300.0, 150.0, -250.0, 200.0) })).await;
    let now = Utc::now();
    t.feature(json!({
        "kind": "exclusion", "name": "calving pen", "geometry": rect(-150.0, 250.0, -100.0, 300.0),
        "active_from": ts(now + Duration::hours(1)), "active_until": ts(now + Duration::hours(3)),
    }))
    .await;

    let c = t.check(json!({ "geometry": p1() })).await;
    let sent = polygon(&c["sent"]);
    assert_eq!(sent.coordinates.len(), 2, "the wet spot is a hole");
    assert_eq!(c["facts"]["holes"], 1);
    assert_eq!(finding(&c, "overlaps_exclusion")["text"], "Wet spot kept out");
    assert!(c.get("legacy").is_none() && c.get("sweep").is_none());
    // No collar reports, so the send stores the target itself.
    t.send(json!({ "geometry": p1() })).await;
    assert_eq!(polygon(&t.status().await["active"]["geometry"]), sent, "byte for byte what the check said");

    // Staged into the calving pen's window: two holes, and the staged boundary holds both.
    let staged = ts(now + Duration::hours(2));
    let c = t.check(json!({ "geometry": p1(), "effective_at": staged })).await;
    let sent = polygon(&c["sent"]);
    assert_eq!(sent.coordinates.len(), 3);
    t.send(json!({ "geometry": p1(), "effective_at": staged })).await;
    let s = t.status().await;
    assert_eq!(polygon(&s["pending"]["geometry"]), sent);
    assert_eq!(serde_json::to_string(&s["pending"]["geometry"]).unwrap(), serde_json::to_string(&c["sent"]).unwrap());
}

#[tokio::test]
async fn a_sweep_is_previewed_and_ends_on_what_the_check_sent() {
    let t = T::new().await;
    t.send(json!({ "geometry": p1() })).await;
    let keys = t.collars(12, true).await;
    for (i, k) in keys.iter().enumerate() {
        t.fix(k, at(-380.0 + 10.0 * (i % 4) as f64, 150.0 + 30.0 * (i / 4) as f64)).await;
    }
    t.feature(json!({ "kind": "exclusion", "name": "pond", "geometry": rect(150.0, 150.0, 200.0, 200.0) })).await;

    let plain = t.check(json!({ "geometry": p2() })).await;
    let out = finding(&plain, "animals_outside");
    assert_eq!((out["severity"].as_str(), out["text"].as_str()), (Some("warning"), Some("12 outside it")));
    let c = t.check(json!({ "geometry": p2(), "sweep": true })).await;
    assert_eq!(finding(&c, "animals_outside")["severity"], "info", "the sweep walks them in");
    let sweep = &c["sweep"];
    let minutes = sweep["minutes"].as_f64().unwrap();
    assert_eq!(c["facts"]["sweep_minutes"], sweep["minutes"]);
    let lines = sweep["back_lines"].as_array().unwrap();
    // About 380 m to the rear of P2 at 3 m a step, 40 s a step: an hour and a half, lines every 10 m.
    eprintln!("{} back lines, {minutes} min", lines.len());
    assert!(lines.len() >= 25, "{}", lines.len());
    assert!((60.0..=120.0).contains(&minutes), "{minutes}");
    let x = |l: &Value| Projection::new(CENTER).forward([l[0][0].as_f64().unwrap(), l[0][1].as_f64().unwrap()])[0];
    assert!(x(&lines[0]) < -370.0 && x(lines.last().unwrap()) > -15.0, "from behind the herd to P2's west edge");

    let m = t.send(json!({ "geometry": p2() })).await;
    assert_eq!(m["status"], "sweeping");
    assert_eq!(serde_json::to_string(&m["target"]).unwrap(), serde_json::to_string(&c["sent"]).unwrap());
    // Everyone arrives; the driver's next pass sends the target: what the check sent.
    for (i, k) in keys.iter().enumerate() {
        t.fix(k, at(100.0 + 10.0 * i as f64, 60.0 + 5.0 * i as f64)).await;
    }
    op_ingest::moves::drive(&t.ctx, &t.herd, Utc::now() + Duration::seconds(31)).await.unwrap();
    let s = t.status().await;
    assert_eq!(s["move"]["status"], "done");
    assert_eq!(serde_json::to_string(&s["active"]["geometry"]).unwrap(), serde_json::to_string(&c["sent"]).unwrap());
}

#[tokio::test]
async fn the_pace_comes_from_the_herds_own_sweeps() {
    let t = T::new().await;
    assert_eq!(op_engine::presend::seconds_per_step(&t.ctx, &t.herd).await.unwrap(), 40.0, "30 s, plus half the fast poll and report");
    t.ok("PUT", "/api/collars/config", json!({ "report_s": 60, "poll_s": 60, "fast_report_s": 30, "fast_poll_s": 30 })).await;
    assert_eq!(op_engine::presend::seconds_per_step(&t.ctx, &t.herd).await.unwrap(), 60.0);
    // A finished sweep of 21 steps over 25 minutes: 75 s a step.
    let start = Utc::now() - Duration::hours(3);
    sqlx::query(
        "INSERT INTO moves (id, herd_id, decision_id, target, status, step, remaining_m, stragglers, warn_m, hysteresis_m, sweep, started_at, updated_at)
         VALUES ('mov_1', ?, 'dec_1', ?, 'done', 21, 0, '[]', 5, 1, '{}', ?, ?)",
    )
    .bind(&t.herd)
    .bind(p2().to_string())
    .bind(time::to_db(&start))
    .bind(time::to_db(&(start + Duration::minutes(25))))
    .execute(t.ctx.db())
    .await
    .unwrap();
    assert_eq!(op_engine::presend::seconds_per_step(&t.ctx, &t.herd).await.unwrap(), 75.0);
}

#[tokio::test]
async fn facts_forage_and_area_per_head() {
    let t = T::new().await;
    let c = t.check(json!({ "geometry": p1() })).await;
    let f = &c["facts"];
    assert!((f["area_ha"].as_f64().unwrap() - 16.556).abs() < 0.01, "{f}");
    assert_eq!(f["head"], 250);
    assert!((f["m2_per_head"].as_f64().unwrap() - 662.2).abs() < 0.5, "{f}");
    assert_eq!(f["vertices"], 4);
    assert!(f.get("grazing_days").is_none() && f.get("forage_kg_dm").is_none() && f.get("forage_source").is_none(), "no forage estimate, nothing shown");
    assert!(!codes(&c).contains(&"forage_short"));

    // NDVI 0.62: 7 in of grass, 4 in above the residual, 1,344 kg DM/ha.
    ndvi_report(&t.ctx, &t.p1, 0.62).await;
    let c = t.check(json!({ "geometry": p1() })).await;
    let f = &c["facts"];
    assert_eq!(f["forage_source"], "ndvi");
    assert!((f["forage_kg_dm"].as_f64().unwrap() - 1344.0 * 16.556).abs() < 20.0, "{f}");
    // 60 % of it at 11.8 kg DM per AU a day for 250 AU.
    let days = f["grazing_days"].as_f64().unwrap();
    assert!((days - 4.5).abs() < 0.05, "{days}");

    // A measured height wins: 6 in, 3 above the residual.
    t.ok("POST", &format!("/api/paddocks/{}/heights", t.p1), json!({ "height_cm": 15.24 })).await;
    let c = t.check(json!({ "geometry": p1() })).await;
    assert_eq!(c["facts"]["forage_source"], "measured");
    assert!((c["facts"]["forage_kg_dm"].as_f64().unwrap() - 1008.0 * 16.556).abs() < 20.0);

    // A half-hectare pen for 250 head: tight, and grass for a fraction of a day.
    let c = t.check(json!({ "geometry": rect(-200.0, 100.0, -150.0, 200.0) })).await;
    assert_eq!(finding(&c, "area_per_head_low")["text"], "Only 20 m²/hd");
    assert_eq!(finding(&c, "forage_short")["text"], "Grass for 0.1 d");
    t.ctx.update_settings(&json!({ "units": "imperial" })).await.unwrap();
    let c = t.check(json!({ "geometry": rect(-200.0, 100.0, -150.0, 200.0) })).await;
    assert_eq!(finding(&c, "area_per_head_low")["text"], "Only 215 ft²/hd");
    // Roomy enough: nothing.
    assert!(!codes(&t.check(json!({ "geometry": p1() })).await).contains(&"area_per_head_low"));
}

#[tokio::test]
async fn a_strip_and_its_check_say_the_same_days() {
    let t = T::new().await;
    // 6 in measured: 1,008 kg DM/ha above the residual.
    t.ok("POST", &format!("/api/paddocks/{}/heights", t.p1), json!({ "height_cm": 15.24 })).await;
    for days in [0.5, 1.0] {
        let v = t.ok("POST", "/api/strips/preview", json!({ "paddock_id": t.p1, "herd_id": t.herd, "orientation_deg": 0, "days": days })).await;
        let strip = &v["strips"][0];
        assert_eq!(strip["days"], days, "{strip}");
        let c = t.check(json!({ "geometry": strip["geometry"] })).await;
        // 60 % of 1,008 kg × the strip's area at 2,950 kg DM a day: the strip's own days.
        let kg = c["facts"]["forage_kg_dm"].as_f64().unwrap();
        assert_eq!(c["facts"]["grazing_days"], json!(days), "{}", c["facts"]);
        assert_eq!(op_engine::calc::round(kg * 0.6 / 2950.0, 1), days);
        assert!(!codes(&c).contains(&"forage_short"), "{days} d isn't short");
    }
    // Twice a day (0.4 d strips) is short, and both say 0.4.
    let v = t.ok("POST", "/api/strips/preview", json!({ "paddock_id": t.p1, "herd_id": t.herd, "orientation_deg": 0, "days": 0.4 })).await;
    let c = t.check(json!({ "geometry": v["strips"][0]["geometry"] })).await;
    assert_eq!((v["strips"][0]["days"].clone(), c["facts"]["grazing_days"].clone()), (json!(0.4), json!(0.4)));
    assert_eq!(finding(&c, "forage_short")["text"], "Grass for 0.4 d");
}

#[tokio::test]
async fn short_rest_is_found_for_ground_the_herd_isnt_on() {
    let t = T::new().await;
    let c = t.check(json!({ "geometry": p2() })).await;
    assert!(c["facts"].get("rest_days").is_none() && !codes(&c).contains(&"rested_short"), "no record of P2");
    let mut p = t.req("GET", &format!("/api/paddocks/{}", t.p2), None, None).await.1;
    p["grazed_until"] = json!(ts(Utc::now() - Duration::days(5)));
    t.ok("PATCH", &format!("/api/paddocks/{}", t.p2), json!({ "grazed_until": p["grazed_until"] })).await;
    let c = t.check(json!({ "geometry": p2() })).await;
    assert_eq!(c["facts"]["rest_days"], 5.0);
    let r = finding(&c, "rested_short");
    assert_eq!((r["severity"].as_str(), r["text"].as_str()), (Some("warning"), Some("P2 rested 5 d")));
    assert_eq!(r["targets"], json!([["paddock", t.p2]]));
    // Rested long enough: nothing.
    t.ok("PATCH", &format!("/api/paddocks/{}", t.p2), json!({ "grazed_until": ts(Utc::now() - Duration::days(40)) })).await;
    assert!(!codes(&t.check(json!({ "geometry": p2() })).await).contains(&"rested_short"));
    // A strip of the paddock the herd is on: grazing now, not a rest question.
    let c = t.check(json!({ "geometry": rect(-400.0, 0.0, -300.0, 400.0) })).await;
    assert_eq!(c["facts"]["rest_days"], 0.0);
    assert!(!codes(&c).contains(&"rested_short"));
    // A collar in P2 a moment ago: grazed there, whatever the record says.
    let keys = t.collars(1, true).await;
    t.fix(&keys[0], at(200.0, 200.0)).await;
    let c = t.check(json!({ "geometry": p2() })).await;
    assert_eq!(c["facts"]["rest_days"], 0.0);
    assert_eq!(finding(&c, "rested_short")["text"], "P2 grazed in the last day");
}

#[tokio::test]
async fn an_empty_herd_left_in_a_paddock_is_not_grazing_it() {
    // The training flow ends with every animal moved back to Cows and the
    // Training herd at 0 head, still placed in P2. P2 rested 40 days; it read
    // grazed now (rest 0, "P2 grazed in the last day" under a boundary drawn
    // there, the Rest layer at 0) and a height measured there stood as the
    // grass ahead of a herd.
    let t = T::new().await;
    t.ok("PATCH", &format!("/api/paddocks/{}", t.p2), json!({ "grazed_until": ts(Utc::now() - Duration::days(40)) })).await;
    let training = t.ok("POST", "/api/herds", json!({ "name": "Training", "species": "cattle", "count": 0, "paddock_id": t.p2 })).await;
    let c = t.check(json!({ "geometry": p2() })).await;
    assert_eq!(c["facts"]["rest_days"], 40.0, "{}", c["facts"]);
    assert!(!codes(&c).contains(&"rested_short"), "{:?}", codes(&c));
    // The Rest layer: rested 40 days, not drawn as grazed now.
    let layer = || async {
        let l = t.ok("GET", "/api/layers/paddocks", json!(null)).await;
        let p = l["paddocks"].as_array().unwrap().iter().find(|p| p["paddock_id"] == t.p2.as_str()).unwrap().clone();
        (p["rest_days"].clone(), p.get("grazing").cloned().unwrap_or(json!(false)))
    };
    assert_eq!(layer().await, (json!(40.0), json!(false)));
    // A height taken 10 days ago, grazed down since (grazed_until 2 days ago):
    // no herd with head is there now, so it no longer counts.
    t.ok("POST", &format!("/api/paddocks/{}/heights", t.p2), json!({ "height_cm": 25.4, "at": ts(Utc::now() - Duration::days(10)) })).await;
    t.ok("PATCH", &format!("/api/paddocks/{}", t.p2), json!({ "grazed_until": ts(Utc::now() - Duration::days(2)) })).await;
    let p2_forage = || async {
        let s = t.ok("GET", &format!("/api/signals?herd_id={}", t.herd), json!(null)).await;
        s["paddocks"].as_array().unwrap().iter().find(|p| p["paddock_id"] == t.p2.as_str()).unwrap()["forage"].clone()
    };
    let f = p2_forage().await;
    assert!(f.is_object() && f["source"] != "measured", "{f}");
    // Three head back in Training: P2 is being grazed now.
    t.ok("PATCH", &format!("/api/herds/{}", training["id"].as_str().unwrap()), json!({ "count": 3 })).await;
    assert_eq!(t.check(json!({ "geometry": p2() })).await["facts"]["rest_days"], 0.0);
    assert_eq!(layer().await, (json!(0.0), json!(true)));
    let f = p2_forage().await;
    assert_eq!((f["source"].as_str(), f["height_cm"].as_f64()), (Some("measured"), Some(25.4)), "{f}");
}

#[tokio::test]
async fn a_herd_emptied_where_it_grazed_has_grazed_the_height_down() {
    // Training grazed P2 with 5 head from 20 days ago and was emptied back into
    // Cows 2 days ago, left placed in P2. A height taken 10 days ago, in the
    // middle of that, is grass since eaten: it stops counting.
    let t = T::new().await;
    let training = t.ok("POST", "/api/herds", json!({ "name": "Training", "species": "cattle", "count": 5, "paddock_id": t.p2 })).await;
    let id = training["id"].as_str().unwrap().to_owned();
    t.ok("PATCH", &format!("/api/herds/{id}"), json!({ "count": 0 })).await;
    let rows: Vec<i64> = sqlx::query_scalar("SELECT id FROM herd_history WHERE herd_id = ? ORDER BY id").bind(&id).fetch_all(t.ctx.db()).await.unwrap();
    assert_eq!(rows.len(), 2);
    for (row, ago) in rows.iter().zip([20, 2]) {
        sqlx::query("UPDATE herd_history SET at = ? WHERE id = ?")
            .bind(time::to_db(&(Utc::now() - Duration::days(ago))))
            .bind(row)
            .execute(t.ctx.db())
            .await
            .unwrap();
    }
    t.ok("POST", &format!("/api/paddocks/{}/heights", t.p2), json!({ "height_cm": 25.4, "at": ts(Utc::now() - Duration::days(10)) })).await;
    let s = t.ok("GET", &format!("/api/signals?herd_id={}", t.herd), json!(null)).await;
    let f = s["paddocks"].as_array().unwrap().iter().find(|p| p["paddock_id"] == t.p2.as_str()).unwrap()["forage"].clone();
    assert!(f.is_object() && f["source"] != "measured", "{f}");
    // Measured after they were emptied out: it stands.
    t.ok("POST", &format!("/api/paddocks/{}/heights", t.p2), json!({ "height_cm": 9.0, "at": ts(Utc::now() - Duration::days(1)) })).await;
    let s = t.ok("GET", &format!("/api/signals?herd_id={}", t.herd), json!(null)).await;
    let f = s["paddocks"].as_array().unwrap().iter().find(|p| p["paddock_id"] == t.p2.as_str()).unwrap()["forage"].clone();
    assert_eq!((f["source"].as_str(), f["height_cm"].as_f64()), (Some("measured"), Some(9.0)), "{f}");
}

#[tokio::test]
async fn fixes_across_the_fence_are_not_grazing() {
    let t = T::new().await;
    let rested = Utc::now() - Duration::days(40);
    t.ok("PATCH", &format!("/api/paddocks/{}", t.p2), json!({ "grazed_until": ts(rested) })).await;
    let keys = t.collars(10, true).await;
    // Yesterday 10:00 to 15:55 UTC, a fix every 5 minutes: 72 a collar, the
    // herd in P1. Two cows lie along the P1/P2 fence and 3 of their fixes each
    // land 5 m into P2: 6 of 720, under 1 % of the herd's day.
    let day = (Utc::now() - Duration::days(1)).date_naive().and_hms_opt(10, 0, 0).unwrap().and_utc();
    for (i, k) in keys.iter().enumerate() {
        let fixes: Vec<Value> = (0..72)
            .map(|j| {
                let p = if i < 2 && j % 24 == 23 { at(5.0, 200.0) } else { at(-200.0, 200.0) };
                json!({ "at": ts(day + Duration::minutes(5 * j)), "point": p, "accuracy_m": 2.0, "sats": 9 })
            })
            .collect();
        t.report(k, json!({ "fixes": fixes })).await;
    }
    let c = t.check(json!({ "geometry": p2() })).await;
    assert_eq!(c["facts"]["rest_days"], 40.0, "{}", c["facts"]);
    assert!(!codes(&c).contains(&"rested_short"), "{:?}", codes(&c));
    let p2_signals = || async {
        let s = t.ok("GET", &format!("/api/signals?herd_id={}", t.herd), json!(null)).await;
        s["paddocks"].as_array().unwrap().iter().find(|p| p["paddock_id"] == t.p2.as_str()).cloned().unwrap()
    };
    assert_eq!(p2_signals().await["rest_days"], 40.0);

    // Half the herd walks into P2 for the next hour (16:00 to 16:55): 60 of
    // 780 fixes, 7.7 % of the day. That is grazing, last at 16:55.
    for k in &keys[..5] {
        let fixes: Vec<Value> =
            (0..12).map(|j| json!({ "at": ts(day + Duration::minutes(360 + 5 * j)), "point": at(200.0, 200.0), "accuracy_m": 2.0, "sats": 9 })).collect();
        t.report(k, json!({ "fixes": fixes })).await;
    }
    let last = day + Duration::minutes(415);
    let want = (Utc::now() - last).num_milliseconds() as f64 / 86_400_000.0;
    let c = t.check(json!({ "geometry": p2() })).await;
    let got = c["facts"]["rest_days"].as_f64().unwrap();
    assert!((got - want).abs() <= 0.11, "{got} vs {want}");
    assert!(codes(&c).contains(&"rested_short"));
    assert_eq!(p2_signals().await["last_grazed"], json!(time::to_db(&last)));
}

#[tokio::test]
async fn weak_gps_inside_the_shape_is_found_from_the_coverage_days() {
    let t = T::new().await;
    let keys = t.collars(3, true).await;
    let now = Utc::now();
    for (i, k) in keys.iter().enumerate() {
        // Thirty fixes 10 s apart in one 10 m cell each, at 7 m accuracy.
        let p = at(-195.0 + 20.0 * i as f64, 105.0);
        let fixes: Vec<Value> = (0..30).map(|s| json!({ "at": ts(now - Duration::seconds(300 - 10 * s)), "point": p, "accuracy_m": 7.0, "sats": 6 })).collect();
        t.report(k, json!({ "fixes": fixes })).await;
    }
    op_analytics::days::aggregate(&t.ctx, Utc::now()).await.unwrap();
    let c = t.check(json!({ "geometry": p1() })).await;
    let w = finding(&c, "weak_coverage");
    assert_eq!((w["severity"].as_str(), w["text"].as_str()), (Some("warning"), Some("Weak GPS on 0.03 ha")));
    assert_eq!(w["geometry"]["type"], "MultiPolygon");
    // Cells outside the shape don't count.
    assert!(!codes(&t.check(json!({ "geometry": rect(-400.0, 250.0, -300.0, 390.0) })).await).contains(&"weak_coverage"));
}

#[tokio::test]
async fn legacy_collars_get_one_ring_and_are_told_about() {
    let t = T::new().await;
    t.feature(json!({ "kind": "exclusion", "name": "pond", "geometry": rect(-250.0, 150.0, -200.0, 200.0) })).await;
    t.collars(2, true).await;
    let c = t.check(json!({ "geometry": p1() })).await;
    assert!(c.get("legacy").is_none(), "every collar holds holes");
    t.collars(1, false).await;
    let c = t.check(json!({ "geometry": p1() })).await;
    let legacy = polygon(&c["legacy"]);
    assert_eq!(legacy.coordinates.len(), 1);
    assert!(legacy.contains(at(-225.0, 175.0)), "no hole for firmware 0.1");
    assert_eq!(finding(&c, "collars_no_holes")["text"], "1 collar can't hold holes");
    // The same ring the legacy collar will enforce once it is sent.
    t.send(json!({ "geometry": p1() })).await;
    let stored: op_core::Boundary = serde_json::from_value(t.status().await["active"].clone()).unwrap();
    let caps = op_ingest::CollarCaps { fw: None, caps: vec![], limits: op_geo::CollarLimits::LEGACY };
    assert_eq!(op_ingest::fence_geometry(&stored, &caps), legacy);
}

#[tokio::test]
async fn check_boundary_is_a_read_tool() {
    let t = T::new().await;
    op_engine::register_tools(&t.ctx);
    let spec = t.ctx.tools().get("check_boundary").expect("registered");
    assert!(spec.read && !spec.brain && spec.min_role == Role::Viewer);
    assert!(!t.ctx.tools().brain_tools().contains(&"check_boundary".to_owned()));
    let viewer = Identity { role: Role::Viewer, user_id: None, name: None, via: Via::UserToken };
    let full = op_core::tools::ToolScope::Full;
    assert!(t.ctx.tools().listed_for(&viewer, &full).iter().any(|s| s.name == "check_boundary"));
    let out = t.ctx.tools().call(&t.ctx, "check_boundary", json!({ "geometry": p1() }), None, viewer.clone(), &full).await.unwrap();
    let route = t.check(json!({ "geometry": p1() })).await;
    assert_eq!(out["sent"], route["sent"]);
    assert_eq!(out["facts"], route["facts"]);
    let bad = t.ctx.tools().call(&t.ctx, "check_boundary", json!({ "geometry": { "type": "Point", "coordinates": [0, 0] } }), None, viewer, &full).await;
    assert_eq!(bad.unwrap_err().status, StatusCode::BAD_REQUEST);
    // Checking stores nothing.
    assert!(t.status().await["active"].is_null());
}

#[tokio::test]
async fn shapes_that_cant_be_sent_are_400_and_unknown_herds_404() {
    let t = T::new().await;
    let bowtie = json!({ "type": "Polygon", "coordinates": [[at(-300.0, 0.0), at(-100.0, 200.0), at(-100.0, 0.0), at(-300.0, 200.0), at(-300.0, 0.0)]] });
    let (s, v) = t.req("POST", &format!("/api/herds/{}/check", t.herd), None, Some(json!({ "geometry": bowtie }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    let (s, _) = t.req("POST", &format!("/api/herds/{}/check", t.herd), None, Some(json!({ "geometry": p1(), "warn_m": -1 }))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = t.req("POST", "/api/herds/herd_nope/check", None, Some(json!({ "geometry": p1() }))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_check_with_250_animals_and_30_features_is_quick() {
    let t = T::new().await;
    t.send(json!({ "geometry": p1() })).await;
    let keys = t.collars(250, true).await;
    for (i, k) in keys.iter().enumerate() {
        t.fix(k, at(-400.0 + 15.0 * (i % 25) as f64, 20.0 + 15.0 * (i / 25) as f64)).await;
    }
    for i in 0..10 {
        let x = -400.0 + 40.0 * i as f64;
        t.feature(json!({ "kind": "exclusion", "geometry": rect(x, 250.0, x + 20.0, 270.0) })).await;
        t.feature(json!({ "kind": "water", "name": format!("trough {i}"), "geometry": { "type": "Point", "coordinates": at(x + 10.0, 320.0) } })).await;
    }
    for i in 0..5 {
        let x = -380.0 + 70.0 * i as f64;
        t.feature(json!({ "kind": "hazard", "geometry": { "type": "Point", "coordinates": at(x, 360.0) }, "props": { "radius_m": 5.0 } })).await;
        t.feature(json!({ "kind": "road", "geometry": { "type": "LineString", "coordinates": [at(x, -50.0), at(x + 30.0, 450.0)] } })).await;
    }
    let target = rect(-420.0, 0.0, 10.0, 410.0);
    t.check(json!({ "geometry": target })).await;
    let started = std::time::Instant::now();
    let c = t.check(json!({ "geometry": target })).await;
    let took = started.elapsed();
    assert_eq!(c["facts"]["holes"], 10);
    assert!(codes(&c).contains(&"crosses_road") && codes(&c).contains(&"water_inside"));
    eprintln!("check of 250 animals and 30 features took {took:?}");
    assert!(took < std::time::Duration::from_millis(300), "check took {took:?}");
    // With the sweep preview (to P2, 250 animals):
    let started = std::time::Instant::now();
    let c = t.check(json!({ "geometry": p2(), "sweep": true })).await;
    eprintln!("check with a sweep of 250 took {:?} ({} back lines)", started.elapsed(), c["sweep"]["back_lines"].as_array().map_or(0, |l| l.len()));
}
