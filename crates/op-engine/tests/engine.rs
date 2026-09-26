//! Engine tests against a real data directory, driven through the real routes
//! (op-core for the farm, op-ingest for collars and reports, op-engine).
//! Paddocks are kept under 0.05 ha so no land report (network) is fetched.

use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Autonomy, Ctx, Decision, DecisionAction, DecisionSource, DecisionStatus, Herd, time};
use op_engine::{context, cycle, db, knowledge, mcp};
use serde_json::{Value, json};
use tower::ServiceExt;

const LON: f64 = -92.4;
const LAT: f64 = 38.1;

struct T {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    app: Router,
}

async fn setup() -> T {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let app = Router::new().merge(op_core::router()).merge(op_ingest::router()).merge(op_engine::router()).with_state(ctx.clone());
    T { _dir: dir, ctx, app }
}

impl T {
    async fn req(&self, method: &str, path: &str, body: Option<Value>, bearer: Option<&str>) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path).header("host", "127.0.0.1");
        if let Some(k) = bearer {
            b = b.header("authorization", format!("Bearer {k}"));
        }
        let req = match body {
            Some(v) => b.header("content-type", "application/json").header("accept", "application/json, text/event-stream").body(Body::from(v.to_string())),
            None => b.body(Body::empty()),
        }
        .unwrap();
        let res = self.app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.req(method, path, body, None).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }
}

/// A square about `side_m` metres across, `east_m` east of the farm centre.
fn square(east_m: f64, side_m: f64) -> Value {
    let dlon = 1.0 / (111_320.0 * LAT.to_radians().cos());
    let dlat = 1.0 / 110_540.0;
    let r7 = |v: f64| (v * 1e7).round() / 1e7;
    let (x0, x1) = (r7(LON + east_m * dlon), r7(LON + (east_m + side_m) * dlon));
    let (y0, y1) = (LAT, r7(LAT + side_m * dlat));
    json!({ "type": "Polygon", "coordinates": [[[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]]] })
}

fn centre(g: &Value) -> [f64; 2] {
    let r = g["coordinates"][0].as_array().unwrap();
    let xs: Vec<f64> = r[..4].iter().map(|p| p[0].as_f64().unwrap()).collect();
    let ys: Vec<f64> = r[..4].iter().map(|p| p[1].as_f64().unwrap()).collect();
    [xs.iter().sum::<f64>() / 4.0, ys.iter().sum::<f64>() / 4.0]
}

struct Farm {
    herd: Herd,
    a: Value,
    b: Value,
    keys: Vec<String>,
}

/// Farm, paddocks A and B (20 m squares), a herd in A, `collars` linked collars.
async fn farm(t: &T, autonomy: &str, collars: usize) -> Farm {
    t.ok("POST", "/api/farm", Some(json!({ "name": "Test farm", "timezone": "America/Chicago", "center": [LON, LAT] }))).await;
    let a = t.ok("POST", "/api/paddocks", Some(json!({ "name": "North", "geometry": square(0.0, 20.0) }))).await;
    let b = t.ok("POST", "/api/paddocks", Some(json!({ "name": "South", "geometry": square(40.0, 20.0) }))).await;
    let herd = t
        .ok(
            "POST",
            "/api/herds",
            Some(json!({ "name": "Cows", "species": "cattle", "count": 12, "paddock_id": a["id"], "autonomy": autonomy, "timer_minutes": 30 })),
        )
        .await;
    let herd: Herd = serde_json::from_value(herd).unwrap();
    let mut keys = vec![];
    for _ in 0..collars {
        let linked = t.ok("POST", "/api/collars", Some(json!({ "herd_id": herd.id }))).await;
        keys.push(linked["key"].as_str().unwrap().to_owned());
    }
    Farm { herd, a, b, keys }
}

/// Real protocol reports: `n` fixes at `point` over the last hour, plus cues.
async fn report(t: &T, key: &str, point: [f64; 2], n: usize, cues: &[f64]) {
    let now = time::now();
    let fixes: Vec<Value> = (0..n)
        .map(|i| json!({ "at": (now - chrono::Duration::minutes((n - i) as i64)).format("%Y-%m-%dT%H:%M:%SZ").to_string(), "point": point, "accuracy_m": 2.0, "sats": 9 }))
        .collect();
    let cues: Vec<Value> = cues.iter().map(|m| json!({ "at": now.format("%Y-%m-%dT%H:%M:%SZ").to_string(), "level": 1, "margin_m": m })).collect();
    let (s, v) = t.req("POST", "/collar/v1/report", Some(json!({ "fixes": fixes, "cues": cues, "battery": 0.15 })), Some(key)).await;
    assert!(s.is_success(), "report: {s} {v}");
}

fn move_decision(f: &Farm, source: DecisionSource) -> Decision {
    Decision {
        id: op_core::id::new_id(op_core::id::DECISION),
        herd_id: f.herd.id.clone(),
        source,
        brain: None,
        model: None,
        status: DecisionStatus::Running,
        action: Some(DecisionAction::Move),
        to_paddock_id: Some(f.b["id"].as_str().unwrap().to_owned()),
        geometry: None,
        reasoning: Some("South has rested.".into()),
        confidence: Some(0.7),
        need: None,
        inputs: json!({ "from_paddock_id": f.a["id"] }),
        apply_at: None,
        boundary_id: None,
        error: None,
        created_at: time::now(),
        responded_at: None,
        outcome: None,
    }
}

async fn propose(t: &T, f: &Farm) -> Decision {
    let d = move_decision(f, DecisionSource::Brain);
    db::insert(&t.ctx, &d).await.unwrap();
    let herd = t.ctx.store().get_herd(&f.herd.id).await.unwrap().unwrap();
    cycle::record(&t.ctx, d, &herd).await.unwrap()
}

#[tokio::test]
async fn context_assembly_reads_collars_and_record() {
    let t = setup().await;
    let f = farm(&t, "propose", 2).await;
    // Collars say the herd is in South, although the record says North.
    let south = centre(&f.b["geometry"]);
    report(&t, &f.keys[0], south, 10, &[2.0, -1.5]).await;
    report(&t, &f.keys[1], south, 5, &[]).await;

    let lines = std::sync::Mutex::new(vec![]);
    let a = context::assemble(&t.ctx, &f.herd, false, &|l| lines.lock().unwrap().push(l)).await.unwrap();
    let c = &a.context;
    let (pa, pb) = (f.a["id"].as_str().unwrap(), f.b["id"].as_str().unwrap());
    assert_eq!(c["current_paddock_id"], pb);
    assert_eq!(c["position_source"], "collar");
    assert_eq!(c["candidate_paddock_ids"], json!([pa]));
    assert_eq!(c["farm"]["name"], "Test farm");
    assert_eq!(c["herd"]["animal_units"], 12.0);
    assert_eq!(c["autonomy"]["mode"], "propose");
    assert_eq!(c["paddocks"].as_array().unwrap().len(), 2);
    assert_eq!(c["collars"]["count"], 2);
    assert_eq!(c["collars"]["reporting_24h"], 2);
    assert_eq!(c["collars"]["fixes_24h"], 15);
    assert_eq!(c["collars"]["cues_24h"], 2);
    assert_eq!(c["collars"]["low_battery"].as_array().unwrap().len(), 2);
    assert_eq!(c["positions"].as_array().unwrap().len(), 2);
    assert_eq!(c["positions"][0]["paddock_id"], pb);
    // Signals from the fixes: all the pressure is on South, which has no rest.
    assert_eq!(c["signals"]["grazing_pressure"][pb]["share_of_time"], 1.0);
    assert_eq!(c["signals"]["grazing_pressure"][pb]["animal_days"], 12.0);
    assert_eq!(c["signals"]["rest_days"][pb], 0.0);
    assert!(c["signals"]["rest_days"][pa].is_null());
    assert_eq!(c["signals"]["behavior"]["cue_count"], 2);
    assert_eq!(c["signals"]["behavior"]["cues_past_line"], 1);
    // Tiny paddocks: no land report, and a note saying so.
    assert!(c["land_reports"].as_object().unwrap().is_empty());
    assert!(c["land_report_notes"][pa].is_string());
    assert!(!c["knowledge"].as_array().unwrap().is_empty());
    assert!(c["boundary"]["acks"].is_array());
    assert!(!lines.lock().unwrap().is_empty());
}

#[tokio::test]
async fn autonomy_propose_waits() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    let d = propose(&t, &f).await;
    assert_eq!(d.status, DecisionStatus::Proposed);
    assert!(d.apply_at.is_none());
    assert_eq!(d.geometry.as_ref().map(|g| serde_json::to_value(g).unwrap()), Some(f.b["geometry"].clone()), "MOVE takes the paddock's shape");
    let status = op_ingest::boundary_status(&t.ctx, &f.herd.id).await.unwrap();
    assert_eq!(status.proposed.map(|p| p.decision_id), Some(d.id.clone()));
    assert!(status.active.is_none());

    // A newer proposal supersedes it.
    let d2 = propose(&t, &f).await;
    assert_eq!(db::get(&t.ctx, &d.id).await.unwrap().unwrap().status, DecisionStatus::Superseded);

    // Approve sends it.
    let v = t.ok("POST", &format!("/api/decisions/{}/respond", d2.id), Some(json!({ "action": "approve" }))).await;
    assert_eq!(v["status"], "applied", "{v}");
    let status = op_ingest::boundary_status(&t.ctx, &f.herd.id).await.unwrap();
    assert_eq!(status.active.as_ref().map(|b| b.id.clone()), v["boundary_id"].as_str().map(str::to_owned));
    // Through a move: no tracked animals, so the target went out directly.
    let m = status.r#move.expect("the approval started a move");
    assert_eq!((m.decision_id.as_str(), m.status, m.step), (d2.id.as_str(), op_core::MoveStatus::Done, 1));
    let herd = t.ctx.store().get_herd(&f.herd.id).await.unwrap().unwrap();
    assert_eq!(herd.paddock_id.as_deref(), f.b["id"].as_str(), "the record follows the move");
    let north = t.ctx.store().get_paddock(f.a["id"].as_str().unwrap()).await.unwrap().unwrap();
    assert_eq!(north.status, op_core::PaddockStatus::Resting);
    assert!(north.grazed_until.is_some());

    // Answering twice is a conflict.
    let (s, _) = t.req("POST", &format!("/api/decisions/{}/respond", d2.id), Some(json!({ "action": "reject" })), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test]
async fn autonomy_timer_applies_when_due_unless_rejected() {
    let t = setup().await;
    let f = farm(&t, "timer", 1).await;
    let d = propose(&t, &f).await;
    assert_eq!(d.status, DecisionStatus::Proposed);
    let at = d.apply_at.expect("timer sets apply_at");
    let mins = (at - d.created_at).num_minutes();
    assert!((29..=30).contains(&mins), "apply_at is timer_minutes out: {mins}");

    // Not due yet: nothing happens.
    cycle::apply_due(&t.ctx).await.unwrap();
    assert_eq!(db::get(&t.ctx, &d.id).await.unwrap().unwrap().status, DecisionStatus::Proposed);

    // Due: the scheduler sends it.
    let mut due = db::get(&t.ctx, &d.id).await.unwrap().unwrap();
    due.apply_at = Some(time::now() - chrono::Duration::seconds(1));
    db::update(&t.ctx, &due).await.unwrap();
    cycle::apply_due(&t.ctx).await.unwrap();
    let d = db::get(&t.ctx, &d.id).await.unwrap().unwrap();
    assert_eq!(d.status, DecisionStatus::Applied);
    assert!(d.boundary_id.is_some());
    assert!(d.apply_at.is_none());

    // A rejected timer decision never goes out.
    let r = propose(&t, &f).await;
    t.ok("POST", &format!("/api/decisions/{}/respond", r.id), Some(json!({ "action": "reject", "note": "Creek is up." }))).await;
    let mut r2 = db::get(&t.ctx, &r.id).await.unwrap().unwrap();
    assert_eq!(r2.status, DecisionStatus::Rejected);
    r2.apply_at = Some(time::now() - chrono::Duration::seconds(1));
    db::update(&t.ctx, &r2).await.unwrap();
    cycle::apply_due(&t.ctx).await.unwrap();
    assert_eq!(db::get(&t.ctx, &r.id).await.unwrap().unwrap().status, DecisionStatus::Rejected);
}

#[tokio::test]
async fn autonomy_auto_applies_at_once() {
    let t = setup().await;
    let f = farm(&t, "auto", 1).await;
    let d = propose(&t, &f).await;
    assert_eq!(d.status, DecisionStatus::Applied, "{:?}", d.error);
    let b = op_ingest::boundary_status(&t.ctx, &f.herd.id).await.unwrap().active.expect("active boundary");
    assert_eq!(Some(b.id), d.boundary_id);
    assert_eq!(b.decision_id, d.id);

    // STAY is never sent, even on auto.
    let mut stay = move_decision(&f, DecisionSource::Heuristic);
    stay.action = Some(DecisionAction::Stay);
    db::insert(&t.ctx, &stay).await.unwrap();
    let herd = t.ctx.store().get_herd(&f.herd.id).await.unwrap().unwrap();
    assert_eq!(herd.autonomy, Autonomy::Auto);
    let stay = cycle::record(&t.ctx, stay, &herd).await.unwrap();
    assert_eq!(stay.status, DecisionStatus::Proposed);
    assert!(stay.geometry.is_none() && stay.to_paddock_id.is_none());
}

#[tokio::test]
async fn modify_records_the_farmers_boundary_then_applies() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    let d = propose(&t, &f).await;
    let mine = square(40.0, 15.0);
    let v = t
        .ok(
            "POST",
            &format!("/api/decisions/{}/respond", d.id),
            Some(json!({ "action": "modify", "geometry": mine, "note": "The wet spot at the south end is bigger than it looks." })),
        )
        .await;
    assert_eq!(v["status"], "applied", "{v}");
    assert_eq!(v["geometry"], mine);
    assert_eq!(v["to_paddock_id"], f.b["id"]);
    assert_eq!(v["inputs"]["proposed_geometry"], f.b["geometry"], "the brain's boundary stays on the record");
    assert_eq!(v["inputs"]["farmer_response"]["action"], "modify");
    assert_eq!(v["inputs"]["farmer_response"]["geometry"], mine);
    assert!(v["responded_at"].is_string());
    let active = op_ingest::boundary_status(&t.ctx, &f.herd.id).await.unwrap().active.unwrap();
    assert_eq!(serde_json::to_value(&active.geometry).unwrap(), mine);

    // The note is now a farm lesson, found by search.
    let hits = knowledge::search(&t.ctx, "wet spot south end", 5).await.unwrap();
    assert_eq!(hits.first().map(|h| h.kind.as_str()), Some("farmer"), "{hits:?}");

    // A modify without geometry is refused.
    let d = propose(&t, &f).await;
    let (s, _) = t.req("POST", &format!("/api/decisions/{}/respond", d.id), Some(json!({ "action": "modify" })), None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn needs_info_reply_reaches_the_next_context() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    let mut d = move_decision(&f, DecisionSource::Brain);
    d.action = Some(DecisionAction::NeedsInfo);
    d.to_paddock_id = None;
    d.need = Some("How tall is the grass in North?".into());
    db::insert(&t.ctx, &d).await.unwrap();
    let d = cycle::record(&t.ctx, d, &f.herd).await.unwrap();
    assert_eq!(d.status, DecisionStatus::Proposed);

    // The UI's reply field posts the answer as the farmer's note.
    let note = "About three inches and trampled by the water trough.";
    let v = t.ok("POST", &format!("/api/decisions/{}/respond", d.id), Some(json!({ "action": "approve", "note": note }))).await;
    assert_eq!(v["status"], "approved", "{v}");
    assert_eq!(v["inputs"]["farmer_response"]["note"], note);

    let a = context::assemble(&t.ctx, &f.herd, false, &|_| {}).await.unwrap();
    let obs = a.context["observations"].as_array().unwrap();
    assert!(obs.iter().any(|o| o["content"] == note && o["source"] == "farmer-note"), "{obs:?}");
    let history = a.context["history"].as_array().unwrap();
    assert_eq!(history[0]["farmer_response"]["note"], note, "{history:?}");
}

#[tokio::test]
async fn decide_runs_the_heuristic_brain() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    report(&t, &f.keys[0], centre(&f.a["geometry"]), 5, &[]).await;
    t.ok("PUT", "/api/settings", Some(json!({ "brain": { "id": "heuristic" } }))).await;
    let mut live = t.ctx.subscribe();
    let (s, d) = t.req("POST", &format!("/api/herds/{}/decide", f.herd.id), None, None).await;
    assert_eq!(s, StatusCode::ACCEPTED);
    assert_eq!(d["status"], "running");
    let id = d["id"].as_str().unwrap().to_owned();
    let mut done = None;
    for _ in 0..100 {
        let d = db::get(&t.ctx, &id).await.unwrap().unwrap();
        if d.status != DecisionStatus::Running {
            done = Some(d);
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let d = done.expect("decision finished");
    assert!(matches!(d.status, DecisionStatus::Proposed), "{:?} {:?}", d.status, d.error);
    assert_eq!(d.source, DecisionSource::Heuristic);
    assert!(d.reasoning.is_some());
    assert_eq!(d.inputs["from_paddock_id"], f.a["id"]);
    let mut saw_log = false;
    while let Ok(ev) = live.try_recv() {
        saw_log |= matches!(ev, op_core::Event::DecisionLog { ref decision_id, .. } if *decision_id == id);
    }
    assert!(saw_log, "progress lines are published");
}

#[tokio::test]
async fn knowledge_search_finds_seed() {
    let t = setup().await;
    let v = t.ok("GET", "/api/knowledge?q=top%20third%20residual&limit=3", None).await;
    let hits = v.as_array().unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0]["title"], "Top-Third Rule");
    for k in ["id", "title", "kind", "body", "source"] {
        assert!(hits[0][k].is_string(), "{k}");
    }
    let v = t.ok("GET", "/api/knowledge?q=rumen%20fill&limit=5", None).await;
    assert!(v.as_array().unwrap().iter().any(|h| h["body"].as_str().unwrap().to_lowercase().contains("rumen")));
    // The index lives in the data dir.
    assert!(t.ctx.data_dir().join("knowledge-index").join("fingerprint").exists());
}

#[tokio::test]
async fn signals_endpoint_lists_paddocks() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    report(&t, &f.keys[0], centre(&f.a["geometry"]), 8, &[]).await;
    let v = t.ok("GET", &format!("/api/signals?herd_id={}", f.herd.id), None).await;
    assert_eq!(v["current_paddock_id"], f.a["id"]);
    assert_eq!(v["herd_animal_units"], 12.0);
    let rows = v["paddocks"].as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let north = rows.iter().find(|r| r["paddock_id"] == f.a["id"]).unwrap();
    assert_eq!(north["current"], true);
    assert_eq!(north["rest_days"], 0.0);
    assert_eq!(north["grazing_pressure"]["share_of_time"], 1.0);
    assert!(north["forage"]["available_kg_dm_per_ha"].is_null(), "no imagery, no forage number");
}

async fn mcp_call(t: &T, path: &str, id: u64, method: &str, params: Value) -> Value {
    let (s, v) = t.req("POST", path, Some(json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })), None).await;
    assert_eq!(s, StatusCode::OK, "{method}: {v}");
    v
}

#[tokio::test]
async fn mcp_lists_all_tools_and_calls_them() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    let init = json!({ "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test-agent", "version": "1" } });
    let v = mcp_call(&t, "/mcp", 1, "initialize", init.clone()).await;
    assert_eq!(v["result"]["serverInfo"]["name"], "openpasture");

    // The stateless 2026-07-28 revision is refused, so clients such as Claude
    // Code fall back to `initialize` (with it, Claude Code never offers the tools).
    let v =
        mcp_call(&t, "/mcp", 1, "initialize", json!({ "protocolVersion": "2026-07-28", "capabilities": {}, "clientInfo": { "name": "x", "version": "1" } }))
            .await;
    assert_eq!(v["result"]["protocolVersion"], "2025-11-25", "{v}");

    let v = mcp_call(&t, "/mcp", 2, "tools/list", json!({})).await;
    let names: Vec<&str> = v["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    for n in mcp::READ_TOOLS.iter().chain(mcp::WRITE_TOOLS.iter()) {
        assert!(names.contains(n), "missing {n}");
    }
    assert_eq!(names.len(), 12);

    // The brain's scope has only the read tools.
    let v = mcp_call(&t, "/mcp?scope=brain", 3, "tools/list", json!({})).await;
    let names: Vec<&str> = v["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names.len(), 11);
    assert!(!names.contains(&"propose_boundary"));

    let v = mcp_call(&t, "/mcp", 4, "tools/call", json!({ "name": "get_farm", "arguments": {} })).await;
    assert_eq!(v["result"]["structuredContent"]["farm"]["name"], "Test farm", "{v}");

    let v = mcp_call(&t, "/mcp", 5, "tools/call", json!({ "name": "search_knowledge", "arguments": { "query": "rest recovery" } })).await;
    assert!(!v["result"]["structuredContent"]["results"].as_array().unwrap().is_empty());

    let v = mcp_call(
        &t,
        "/mcp",
        6,
        "tools/call",
        json!({ "name": "propose_boundary", "arguments": { "to_paddock_id": f.b["id"], "reasoning": "South has rested; North is short." } }),
    )
    .await;
    let d = &v["result"]["structuredContent"]["decision"];
    assert_eq!(d["status"], "proposed", "{v}");
    assert_eq!(d["source"], "brain");
    assert_eq!(d["inputs"]["via"], "mcp");
    assert_eq!(d["geometry"], f.b["geometry"]);

    // Tool errors come back as tool results, not protocol errors.
    let v = mcp_call(&t, "/mcp", 7, "tools/call", json!({ "name": "get_decision", "arguments": { "decision_id": "dec_nope" } })).await;
    assert_eq!(v["result"]["isError"], true);
}

/// The farmer's reject and the timer race for the same due decision: exactly
/// one wins, and a rejected decision never has a boundary.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reject_racing_the_timer_never_sends_a_rejected_decision() {
    let t = setup().await;
    let f = farm(&t, "timer", 1).await;
    let (mut rejected, mut applied) = (0, 0);
    for _ in 0..16 {
        let d = propose(&t, &f).await;
        sqlx::query("UPDATE decisions SET apply_at = ? WHERE id = ?")
            .bind(time::to_db(&(time::now() - chrono::Duration::seconds(1))))
            .bind(&d.id)
            .execute(t.ctx.db())
            .await
            .unwrap();
        let ctx = t.ctx.clone();
        let timer = tokio::spawn(async move { cycle::apply_due(&ctx).await.unwrap() });
        let reject = cycle::respond(&t.ctx, &d.id, cycle::Response::Reject, None, None).await;
        timer.await.unwrap();
        let now = db::get(&t.ctx, &d.id).await.unwrap().unwrap();
        match reject {
            Ok(r) => {
                assert_eq!(r.status, DecisionStatus::Rejected);
                assert_eq!(now.status, DecisionStatus::Rejected);
                assert!(now.boundary_id.is_none());
                rejected += 1;
            }
            Err(e) => {
                assert_eq!(e.status, StatusCode::CONFLICT, "{}", e.message);
                assert_eq!(now.status, DecisionStatus::Applied);
                assert!(now.inputs.get("farmer_response").is_none(), "the reject left no trace");
                applied += 1;
            }
        }
    }
    assert_eq!(rejected + applied, 16);
    let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM boundaries b JOIN decisions d ON d.id = b.decision_id WHERE d.status = 'rejected'")
        .fetch_one(t.ctx.db())
        .await
        .unwrap();
    assert_eq!(n, 0);
}

/// A claimed (approved, being sent) decision is not superseded or answered.
#[tokio::test]
async fn claimed_decisions_are_left_alone() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    let d = propose(&t, &f).await;
    // What the claim does before the boundary goes out.
    sqlx::query("UPDATE decisions SET status = 'approved' WHERE id = ?").bind(&d.id).execute(t.ctx.db()).await.unwrap();
    let (s, _) = t.req("POST", &format!("/api/decisions/{}/respond", d.id), Some(json!({ "action": "reject" })), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let _newer = propose(&t, &f).await;
    assert_eq!(db::get(&t.ctx, &d.id).await.unwrap().unwrap().status, DecisionStatus::Approved, "not superseded");
    // A farmer boundary supersedes the open proposal, straight from op-ingest.
    let open = propose(&t, &f).await;
    t.ok("POST", &format!("/api/herds/{}/boundary", f.herd.id), Some(json!({ "geometry": square(40.0, 20.0) }))).await;
    assert_eq!(db::get(&t.ctx, &open.id).await.unwrap().unwrap().status, DecisionStatus::Superseded);
    assert_eq!(db::get(&t.ctx, &d.id).await.unwrap().unwrap().status, DecisionStatus::Approved);
}

/// One running brain decision per herd, enforced by the database.
#[tokio::test]
async fn one_running_decision_per_herd() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    let mut running = move_decision(&f, DecisionSource::Brain);
    running.brain = Some(op_core::BrainId::Heuristic);
    db::insert(&t.ctx, &running).await.unwrap();
    let got = cycle::start_decision(&t.ctx, &f.herd.id).await.unwrap();
    assert_eq!(got.id, running.id);
    let mut second = move_decision(&f, DecisionSource::Brain);
    second.brain = Some(op_core::BrainId::Heuristic);
    assert!(db::insert(&t.ctx, &second).await.is_err());
    // MCP proposals (no brain) pass through running alongside it.
    let d = propose(&t, &f).await;
    assert_eq!(d.status, DecisionStatus::Proposed);
}

/// Switching autonomy with a proposal open: timer starts the countdown,
/// propose stops it, auto sends it straight away.
#[tokio::test]
async fn autonomy_change_follows_the_open_proposal() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    op_engine::start(t.ctx.clone()).await.unwrap();
    let d = propose(&t, &f).await;
    assert!(d.apply_at.is_none());
    let herd_path = format!("/api/herds/{}", f.herd.id);

    let mut rx = t.ctx.subscribe();
    t.ok("PATCH", &herd_path, Some(json!({ "autonomy": "timer" }))).await;
    let got = db::get(&t.ctx, &d.id).await.unwrap().unwrap();
    let mins = (got.apply_at.expect("countdown started") - time::now()).num_seconds();
    assert!((29 * 60..=30 * 60).contains(&mins), "{mins}");
    assert_eq!(got.status, DecisionStatus::Proposed);
    let ev = rx.try_recv().expect("a decision event for the UI");
    assert!(matches!(ev, op_core::Event::Decision { ref decision } if decision.id == d.id && decision.apply_at.is_some()));

    // A new timer length restarts the countdown.
    t.ok("PATCH", &herd_path, Some(json!({ "timer_minutes": 60 }))).await;
    let got = db::get(&t.ctx, &d.id).await.unwrap().unwrap();
    assert!((got.apply_at.unwrap() - time::now()).num_minutes() >= 59);

    t.ok("PATCH", &herd_path, Some(json!({ "autonomy": "propose" }))).await;
    assert!(db::get(&t.ctx, &d.id).await.unwrap().unwrap().apply_at.is_none());

    t.ok("PATCH", &herd_path, Some(json!({ "autonomy": "auto" }))).await;
    let mut status = DecisionStatus::Proposed;
    for _ in 0..40 {
        status = db::get(&t.ctx, &d.id).await.unwrap().unwrap().status;
        if status == DecisionStatus::Applied {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(status, DecisionStatus::Applied, "auto sends the open proposal at once");
    t.ctx.shutdown();
}
