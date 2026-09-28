//! The morning brief from real decision records, through the real routes
//! (op-core for the farm, op-ingest for collars and acks, op-engine for
//! decisions, the brief and MCP). Paddocks are the live-check squares near
//! Ames (about 16.5 ha each); nothing here fetches a land report.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::brief::BriefLine;
use op_core::units::{Fmt, Units};
use op_core::{Ctx, Decision, DecisionAction, DecisionSource, DecisionStatus, Herd, Identity, Via, time};
use op_engine::brief::{self, TEXT_MAX, gsm7_len, is_gsm7};
use op_engine::{cycle, db};
use serde_json::{Value, json};
use tower::ServiceExt;

struct T {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    app: Router,
}

async fn setup() -> T {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let app = Router::new().merge(op_core::router()).merge(op_ingest::router()).merge(op_engine::router()).with_state(ctx.clone());
    let app = op_core::with_identity(app, Identity::owner(Via::Local));
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

    async fn units(&self, units: &str) {
        self.ctx.update_settings(&json!({ "units": units })).await.unwrap();
    }

    async fn lines(&self, f: &Farm) -> Vec<String> {
        self.brief(f).await.lines
    }

    async fn brief(&self, f: &Farm) -> brief::Brief {
        let herd = self.ctx.store().get_herd(&f.herd.id).await.unwrap().unwrap();
        brief::brief(&self.ctx, &herd, time::now()).await.unwrap()
    }
}

fn square(lon: f64, lat: f64) -> Value {
    json!({ "type": "Polygon", "coordinates": [[[lon, lat], [lon + 0.005, lat], [lon + 0.005, lat + 0.0036], [lon, lat + 0.0036], [lon, lat]]] })
}

struct Farm {
    herd: Herd,
    p1: Value,
    p2: Value,
    keys: Vec<String>,
}

/// Farm, P1 and P2 side by side, Cows in P1 with `autonomy`, `collars` linked collars.
async fn farm(t: &T, autonomy: &str, collars: usize) -> Farm {
    t.ok("POST", "/api/farm", Some(json!({ "name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03] }))).await;
    let p1 = t.ok("POST", "/api/paddocks", Some(json!({ "name": "P1", "geometry": square(-93.625, 42.03) }))).await;
    let p2 = t.ok("POST", "/api/paddocks", Some(json!({ "name": "P2", "geometry": square(-93.62, 42.03) }))).await;
    let herd = t
        .ok(
            "POST",
            "/api/herds",
            Some(json!({ "name": "Cows", "species": "cattle", "count": 12, "paddock_id": p1["id"], "autonomy": autonomy, "timer_minutes": 30 })),
        )
        .await;
    let herd: Herd = serde_json::from_value(herd).unwrap();
    let mut keys = vec![];
    for _ in 0..collars {
        let linked = t.ok("POST", "/api/collars", Some(json!({ "herd_id": herd.id }))).await;
        keys.push(linked["key"].as_str().unwrap().to_owned());
    }
    Farm { herd, p1, p2, keys }
}

fn id(v: &Value) -> String {
    v["id"].as_str().unwrap().to_owned()
}

fn area(v: &Value) -> f64 {
    v["area_ha"].as_f64().unwrap()
}

/// Real protocol reports: `n` fixes at `point` over the last `n` minutes.
async fn report(t: &T, key: &str, point: [f64; 2], n: usize) {
    let now = time::now();
    let fixes: Vec<Value> = (0..n)
        .map(|i| json!({ "at": (now - chrono::Duration::minutes((n - i) as i64)).format("%Y-%m-%dT%H:%M:%SZ").to_string(), "point": point, "accuracy_m": 2.0, "sats": 9 }))
        .collect();
    let (s, v) = t.req("POST", "/collar/v1/report", Some(json!({ "fixes": fixes, "cues": [], "battery": 0.8 })), Some(key)).await;
    assert!(s.is_success(), "report: {s} {v}");
}

fn decision(f: &Farm, action: DecisionAction, reasoning: &str) -> Decision {
    Decision {
        id: op_core::id::new_id(op_core::id::DECISION),
        herd_id: f.herd.id.clone(),
        source: DecisionSource::Brain,
        brain: None,
        model: None,
        status: DecisionStatus::Running,
        action: Some(action),
        to_paddock_id: (action == DecisionAction::Move).then(|| id(&f.p2)),
        geometry: None,
        reasoning: Some(reasoning.into()),
        confidence: Some(0.6),
        need: None,
        inputs: json!({ "from_paddock_id": id(&f.p1), "position_source": "collar" }),
        apply_at: None,
        boundary_id: None,
        error: None,
        created_at: time::now(),
        responded_at: None,
        outcome: None,
    }
}

/// Record `d` the way the cycle does (validate, autonomy, supersede).
async fn record(t: &T, f: &Farm, d: Decision) -> Decision {
    db::insert(&t.ctx, &d).await.unwrap();
    let herd = t.ctx.store().get_herd(&f.herd.id).await.unwrap().unwrap();
    cycle::record(&t.ctx, d, &herd).await.unwrap()
}

const FIVE: &str = "Grass in P1 is short and trampled near the water. P2 has rested 34 days. Rain is due Friday. \
The herd held its boundary yesterday. Collars place the herd in P1.";

#[tokio::test]
async fn a_waiting_move_asks_for_y_or_n_in_both_unit_systems() {
    let t = setup().await;
    let f = farm(&t, "propose", 2).await;
    let d = record(&t, &f, decision(&f, DecisionAction::Move, FIVE)).await;
    assert_eq!(d.status, DecisionStatus::Proposed);

    t.units("imperial").await;
    let b = t.brief(&f).await;
    let imperial = Fmt::new(Units::Imperial).area(area(&f.p2));
    assert!(imperial.ends_with(" ac"), "{imperial}");
    assert_eq!(
        b.lines,
        [
            format!("Cows: MOVE to P2 ({imperial})."),
            "Reply Y or N.".into(),
            "Grass in P1 is short and trampled near the water.".into(),
            "P2 has rested 34 days.".into(),
            "Rain is due Friday.".into(),
            "The herd held its boundary yesterday.".into(),
            "2 of 2 collars not reported yet.".into(),
            "No field note in 7 days.".into(),
        ],
        "two to four reasons: the fifth is left out"
    );
    assert_eq!(b.herd_id, f.herd.id);
    assert_eq!(b.text, b.lines.join("\n"), "it all fits one text");

    t.units("metric").await;
    let metric = Fmt::new(Units::Metric).area(area(&f.p2));
    assert!(metric.ends_with(" ha"), "{metric}");
    assert_eq!(t.lines(&f).await[0], format!("Cows: MOVE to P2 ({metric})."));
}

#[tokio::test]
async fn a_move_onto_part_of_a_paddock_gives_the_boundarys_area_as_its_text_does() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    // The west half of P2: a strip, not the whole paddock.
    let half = json!({ "type": "Polygon", "coordinates": [[[-93.62, 42.03], [-93.6175, 42.03], [-93.6175, 42.0336], [-93.62, 42.0336], [-93.62, 42.03]]] });
    let mut d = decision(&f, DecisionAction::Move, "P2 has rested 34 days.");
    d.geometry = Some(serde_json::from_value(half).unwrap());
    let d = record(&t, &f, d).await;
    let strip = d.geometry.as_ref().unwrap().area_ha();
    assert!((strip - area(&f.p2) / 2.0).abs() < 0.05, "{strip}");
    t.units("metric").await;
    assert_eq!(t.lines(&f).await[0], format!("Cows: MOVE to P2 ({}).", Fmt::new(Units::Metric).area(strip)));
}

#[tokio::test]
async fn a_timer_move_says_when_it_sends() {
    let t = setup().await;
    let f = farm(&t, "timer", 1).await;
    let d = record(&t, &f, decision(&f, DecisionAction::Move, "P2 has rested 34 days.")).await;
    let at = d.apply_at.expect("timer sets apply_at");
    let tz: chrono_tz::Tz = "America/Chicago".parse().unwrap();
    let local = at.with_timezone(&tz);
    let clock = if local.date_naive() == time::now().with_timezone(&tz).date_naive() {
        local.format("%H:%M").to_string()
    } else {
        local.format("%a %H:%M").to_string()
    };
    let lines = t.lines(&f).await;
    assert_eq!(lines[1], format!("Sends {clock} unless you reply N."));
    assert_eq!(lines[2], "P2 has rested 34 days.");
}

#[tokio::test]
async fn a_sent_move_counts_confirmed_collars_and_the_distance_left() {
    let t = setup().await;
    let f = farm(&t, "propose", 2).await;
    // The herd is in P1 by its collars, so the move to P2 sweeps.
    let inside = [-93.624, 42.0318];
    for k in &f.keys {
        report(&t, k, inside, 5).await;
    }
    let d = record(&t, &f, decision(&f, DecisionAction::Move, "P2 has rested 34 days.")).await;
    let v = t.ok("POST", &format!("/api/decisions/{}/respond", d.id), Some(json!({ "action": "approve" }))).await;
    assert_eq!(v["status"], "applied", "{v}");
    let status = op_ingest::boundary_status(&t.ctx, &f.herd.id).await.unwrap();
    let m = status.r#move.clone().unwrap();
    assert_eq!(m.status, op_core::MoveStatus::Sweeping, "{m:?}");
    let active = status.active.clone().unwrap();

    // One collar applied the step.
    let ack = json!({ "command_id": active.id, "version": active.version, "status": "applied", "at": time::now().format("%Y-%m-%dT%H:%M:%SZ").to_string() });
    let (s, v) = t.req("POST", "/collar/v1/ack", Some(ack), Some(&f.keys[0])).await;
    assert!(s.is_success(), "ack: {s} {v}");

    t.units("imperial").await;
    let feet = Fmt::new(Units::Imperial).len(m.remaining_m);
    assert!(feet.ends_with(" ft"), "{feet}");
    let lines = t.lines(&f).await;
    assert_eq!(lines[1], format!("Sent, 1/2 collars confirmed, {feet} to go."));
    t.units("metric").await;
    let metres = Fmt::new(Units::Metric).len(m.remaining_m);
    assert!(metres.ends_with(" m"), "{metres}");
    assert_eq!(t.lines(&f).await[1], format!("Sent, 1/2 collars confirmed, {metres} to go."));
    // Both collars reported just now: none silent.
    assert!(!t.lines(&f).await.iter().any(|l| l.contains("silent")));
}

#[tokio::test]
async fn a_farmer_boundary_with_no_sweep_is_sent_and_confirmed() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    // No fixes: the target goes out at once.
    let started = t.ok("POST", &format!("/api/herds/{}/boundary", f.herd.id), Some(json!({ "geometry": f.p2["geometry"] }))).await;
    let status = op_ingest::boundary_status(&t.ctx, &f.herd.id).await.unwrap();
    let active = status.active.unwrap();
    assert_eq!(active.decision_id, started["decision_id"].as_str().unwrap());
    let ack = json!({ "command_id": active.id, "version": active.version, "status": "applied", "at": time::now().format("%Y-%m-%dT%H:%M:%SZ").to_string() });
    assert!(t.req("POST", "/collar/v1/ack", Some(ack), Some(&f.keys[0])).await.0.is_success());
    // A farm at Ames starts imperial; the call is checked in metric.
    t.units("metric").await;
    let lines = t.lines(&f).await;
    assert_eq!(lines[0], format!("Cows: MOVE to P2 ({}).", Fmt::new(Units::Metric).area(area(&f.p2))));
    assert_eq!(lines[1], "Sent, 1/1 collars confirmed.");
    assert_eq!(lines[2], "Boundary drawn by the farmer.");
}

#[tokio::test]
async fn stay_and_needs_info() {
    let t = setup().await;
    let f = farm(&t, "propose", 0).await;
    record(&t, &f, decision(&f, DecisionAction::Stay, "The current paddock still appears workable.")).await;
    assert_eq!(
        t.lines(&f).await,
        ["Cows: STAY in P1.", "The current paddock still appears workable.", "No field note in 7 days."],
        "a STAY waits on nobody; no collars, nothing silent"
    );

    let mut d = decision(&f, DecisionAction::NeedsInfo, "There is no recent field observation from the current paddock.");
    d.need = Some("How is P1 looking? Residual grass height and ground condition".into());
    record(&t, &f, d).await;
    assert_eq!(
        t.lines(&f).await,
        [
            "Cows: NEEDS_INFO.",
            "How is P1 looking? Residual grass height and ground condition.",
            "There is no recent field observation from the current paddock.",
        ],
        "it asks for the one thing; no separate field-note line"
    );

    // A farmer's note in the last week: no field-note line after a STAY.
    op_engine::knowledge::add_lesson(&t.ctx, "Farmer's note on P1", "Plenty of grass left.", "farmer", "test", None, Some(&id(&f.p1))).await.unwrap();
    record(&t, &f, decision(&f, DecisionAction::Stay, "Plenty left.")).await;
    assert_eq!(t.lines(&f).await, ["Cows: STAY in P1.", "Plenty left."]);
}

#[tokio::test]
async fn running_failed_or_missing_say_so_in_one_line_and_give_the_rest() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    t.ctx.brief_lines().register(BriefLine {
        name: "q_test_attention",
        order: 50,
        run: BriefLine::run_fn(|_c, _h, _n| async { Ok(vec!["Battery low: 031 14%.".to_owned()]) }),
    });
    let rest = ["1 of 1 collars not reported yet.", "No field note in 7 days.", "Battery low: 031 14%."];

    let expect = |head: &str| std::iter::once(head.to_owned()).chain(rest.iter().map(|s| s.to_string())).collect::<Vec<_>>();
    assert_eq!(t.lines(&f).await, expect("Cows: no decision yet today."));

    // Yesterday's decision is not today's.
    let mut old = decision(&f, DecisionAction::Stay, "Old.");
    old.created_at = time::now() - chrono::Duration::hours(26);
    record(&t, &f, old).await;
    assert_eq!(t.lines(&f).await, expect("Cows: no decision yet today."));

    let running = decision(&f, DecisionAction::Stay, "x");
    let running = Decision { action: None, reasoning: None, ..running };
    db::insert(&t.ctx, &running).await.unwrap();
    assert_eq!(t.lines(&f).await, expect("Cows: today's decision is still running."));

    let mut failed = db::get(&t.ctx, &running.id).await.unwrap().unwrap();
    failed.status = DecisionStatus::Failed;
    failed.error = Some("The brain took longer than 20 minutes".into());
    db::update(&t.ctx, &failed).await.unwrap();
    assert_eq!(t.lines(&f).await, expect("Cows: today's decision failed: The brain took longer than 20 minutes."));
}

#[tokio::test]
async fn registry_lines_come_last_in_their_order() {
    let t = setup().await;
    let f = farm(&t, "propose", 0).await;
    let reg = t.ctx.brief_lines();
    reg.register(BriefLine { name: "q_test_late", order: 60, run: BriefLine::run_fn(|_c, _h, _n| async { Ok(vec!["Late line.".to_owned()]) }) });
    reg.register(BriefLine { name: "q_test_broken", order: 30, run: BriefLine::run_fn(|_c, _h, _n| async { anyhow::bail!("down") }) });
    reg.register(BriefLine {
        name: "q_test_early",
        order: 10,
        run: BriefLine::run_fn(|_c, herd_id, _n| async move { Ok(vec![format!("Early line for {}.", herd_id.len()), String::new()]) }),
    });
    record(&t, &f, decision(&f, DecisionAction::Stay, "Workable.")).await;
    let lines = t.lines(&f).await;
    assert_eq!(lines[..2], ["Cows: STAY in P1.", "Workable."]);
    assert_eq!(lines[lines.len() - 2..], [format!("Early line for {}.", f.herd.id.len()), "Late line.".into()]);
}

#[tokio::test]
async fn stale_data_is_named() {
    let t = setup().await;
    let f = farm(&t, "propose", 3).await;
    report(&t, &f.keys[0], [-93.624, 42.0318], 3).await;
    // The second last reported two days ago; the third, linked just now, never has.
    report(&t, &f.keys[1], [-93.624, 42.0318], 3).await;
    let two_days = time::to_db(&(time::now() - chrono::Duration::days(2)));
    let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM collars WHERE herd_id = ? AND last_seen IS NOT NULL ORDER BY id")
        .bind(&f.herd.id)
        .fetch_all(t.ctx.db())
        .await
        .unwrap();
    sqlx::query("UPDATE collars SET last_seen = ? WHERE id = ?").bind(&two_days).bind(&ids[1]).execute(t.ctx.db()).await.unwrap();
    // Imagery for P1, taken 20 days ago.
    let taken = (time::now() - chrono::Duration::days(20)).format("%Y-%m-%d").to_string();
    let report_json = json!({ "report_id": "lr_1", "paddock_id": id(&f.p1), "source": "alexandria", "as_of": time::to_db(&time::now()),
        "sections": { "imagery": { "status": "ok", "latest": { "captured_at": taken }, "ndvi_stats": { "mean": 0.5 } } } });
    sqlx::query("INSERT INTO land_reports (id, paddock_id, cache_key, source, as_of, report, created_at) VALUES ('lr_1', ?, 'k', 'alexandria', ?, ?, ?)")
        .bind(id(&f.p1))
        .bind(time::to_db(&time::now()))
        .bind(report_json.to_string())
        .bind(time::to_db(&time::now()))
        .execute(t.ctx.db())
        .await
        .unwrap();
    let mut d = decision(&f, DecisionAction::Stay, "Workable.");
    d.inputs["position_source"] = json!("farm_record");
    record(&t, &f, d).await;
    assert_eq!(
        t.lines(&f).await,
        [
            "Cows: STAY in P1.",
            "Workable.",
            "1 of 3 collars silent for a day.",
            "1 of 3 collars not reported yet.",
            "Herd position from the farm record, not collars.",
            "Imagery for P1 is 20 days old.",
            "No field note in 7 days.",
        ]
    );
}

#[tokio::test]
async fn a_call_the_farmer_changed_drops_the_proposals_reasons() {
    let t = setup().await;
    let f = farm(&t, "propose", 0).await;
    // S's schedule STAY that the farmer answered N to: now a HOLD, applied.
    let mut d = decision(&f, DecisionAction::Stay, "The schedule opens strip 3 of 3 at Mon 08:37.");
    d.action = Some(DecisionAction::Hold);
    d.status = DecisionStatus::Applied;
    d.need = Some("Check the water in strip 3".into());
    d.inputs["proposed_action"] = json!("STAY");
    d.inputs["farmer_response"] = json!({ "action": "reject", "by": { "via": "text", "name": "Mia" } });
    db::insert(&t.ctx, &d).await.unwrap();
    let lines = t.lines(&f).await;
    assert_eq!(lines[0], "Cows: HOLD today's strip.");
    assert!(!lines.iter().any(|l| l.contains("Mon 08:37") || l.contains("water")), "the STAY's reasons and check are gone: {lines:?}");

    // A brain MOVE the farmer redrew: its reasons were for its own boundary.
    let mut m = decision(&f, DecisionAction::Move, "P2 has rested 34 days.");
    m.status = DecisionStatus::Applied;
    m.inputs["proposed_geometry"] = f.p2["geometry"].clone();
    m.created_at = time::now() + chrono::Duration::seconds(1);
    db::insert(&t.ctx, &m).await.unwrap();
    assert!(!t.lines(&f).await.iter().any(|l| l.contains("rested 34 days")));
    // As proposed, the reasons stay.
    let mut p = decision(&f, DecisionAction::Move, "P2 has rested 34 days.");
    p.created_at = time::now() + chrono::Duration::seconds(2);
    record(&t, &f, p).await;
    assert!(t.lines(&f).await.iter().any(|l| l == "P2 has rested 34 days."));
}

#[tokio::test]
async fn the_text_is_gsm7_and_fits_one_brief() {
    let t = setup().await;
    let f = farm(&t, "propose", 1).await;
    t.ok("PATCH", &format!("/api/herds/{}", f.herd.id), Some(json!({ "name": "Cows 🐄 “north” — the big herd that grazes the far side" }))).await;
    let long = "Grass in P1 is short — about 3″ — and trampled near the water trough, where the cows stand all afternoon in the heat. \
P2 has rested 34 days and its regrowth looks strong on the imagery, well past the 30-day target for this time of year. \
Rain is due Friday, so moving before the ground softens keeps the lane from pugging. \
The herd held its boundary yesterday with only a few warning tones at the east side.";
    t.ctx.brief_lines().register(BriefLine {
        name: "q_test_attention",
        order: 50,
        run: BriefLine::run_fn(|_c, _h, _n| async { Ok(vec!["Battery low: 031 14%, 118 16%. GPS weak: 207.".to_owned()]) }),
    });
    record(&t, &f, decision(&f, DecisionAction::Move, long)).await;
    let b = t.brief(&f).await;
    assert!(is_gsm7(&b.text), "{}", b.text);
    assert!(gsm7_len(&b.text) <= TEXT_MAX, "{} > {TEXT_MAX}", gsm7_len(&b.text));
    let text: Vec<&str> = b.text.lines().collect();
    assert!(text[0].starts_with("Cows \"north\" - the big herd that graze: MOVE to P2 ("), "names are cut: {}", text[0]);
    assert_eq!(text[1], "Reply Y or N.");
    assert!(text.contains(&"Grass in P1 is short - about 3\" - and trampled near the water trough, where the cows stand all afternoon in the heat."));
    assert!(!text.iter().any(|l| l.starts_with("The herd held")), "the fourth reason gave way");
    assert!(text.contains(&"Battery low: 031 14%, 118 16%. GPS weak: 207."), "{}", b.text);
    // The lines keep every reason as written.
    assert!(b.lines.iter().any(|l| l.starts_with("The herd held")));
}

#[tokio::test]
async fn brief_over_rest_and_mcp() {
    let t = setup().await;
    let f = farm(&t, "propose", 0).await;
    record(&t, &f, decision(&f, DecisionAction::Stay, "Workable.")).await;

    let v = t.ok("GET", &format!("/api/brief?herd_id={}", f.herd.id), None).await;
    assert_eq!(v["herd_id"], f.herd.id.as_str());
    assert_eq!(v["lines"], json!(["Cows: STAY in P1.", "Workable.", "No field note in 7 days."]));
    assert_eq!(v["text"], "Cows: STAY in P1.\nWorkable.\nNo field note in 7 days.");
    // One herd: herd_id may be left out.
    assert_eq!(t.ok("GET", "/api/brief", None).await, v);
    assert_eq!(t.req("GET", "/api/brief?herd_id=herd_nope", None, None).await.0, StatusCode::NOT_FOUND);

    // MCP: a read tool anyone may call, not offered to the decision brain.
    op_engine::register_tools(&t.ctx);
    let spec = t.ctx.tools().get("get_morning_brief").expect("registered");
    assert!(spec.read && !spec.brain && spec.min_role == op_core::Role::Viewer);
    assert!(!t.ctx.tools().brain_tools().contains(&"get_morning_brief".to_owned()));
    let runner = t.ctx.tool_runner(Identity::brain(), &["run_sql"]);
    assert!(runner.tools().iter().any(|t| t.name == "get_morning_brief"), "text questions may use it");
    let out = runner.call("get_morning_brief", json!({})).await.unwrap();
    assert_eq!(out, v);
    let body = json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": { "name": "get_morning_brief", "arguments": { "herd_id": f.herd.id } } });
    let (s, r) = t.req("POST", "/mcp", Some(body), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(r["result"]["structuredContent"], v, "{r}");

    // Two herds: say which.
    t.ok("POST", "/api/herds", Some(json!({ "name": "Heifers", "species": "cattle", "count": 20 }))).await;
    let (s, e) = t.req("GET", "/api/brief", None, None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(e["error"], "The farm has 2 herds; pass herd_id.");
    assert!(runner.call("get_morning_brief", json!({})).await.unwrap_err().contains("pass herd_id"));
}

/// A text question through `Brain::ask` with the real engine tools: the
/// Anthropic-shaped model (a server in this test) reads the brief, then answers.
#[tokio::test]
async fn a_question_reads_the_brief_through_the_real_tools() {
    use std::sync::{Arc, Mutex};
    let t = setup().await;
    let f = farm(&t, "propose", 0).await;
    record(&t, &f, decision(&f, DecisionAction::Stay, "Workable.")).await;
    op_engine::register_tools(&t.ctx);

    let seen: Arc<Mutex<Vec<Value>>> = Arc::default();
    let log = seen.clone();
    let model = axum::Router::new().route(
        "/v1/messages",
        axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let log = log.clone();
            async move {
                let n = {
                    let mut l = log.lock().unwrap();
                    l.push(body);
                    l.len()
                };
                axum::Json(if n == 1 {
                    json!({ "stop_reason": "tool_use", "content": [{ "type": "tool_use", "id": "tu_1", "name": "get_morning_brief", "input": {} }] })
                } else {
                    json!({ "stop_reason": "end_turn", "content": [{ "type": "text", "text": "Cows stay in P1 today. Nothing to answer." }] })
                })
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, model).await.unwrap() });

    let brain = op_brain::api::AnthropicBrain::new("k".into(), None).with_base(&url);
    let req = op_brain::AskRequest {
        question: "What's the plan today?".into(),
        context: json!({ "farm": "Test farm" }),
        tools: t.ctx.tool_runner(Identity { role: op_core::Role::Hand, user_id: None, name: Some("Luis".into()), via: Via::Text }, &["run_sql"]),
        max_chars: 320,
        log: None,
    };
    use op_brain::Brain;
    assert_eq!(brain.ask(req).await.unwrap(), "Cows stay in P1 today. Nothing to answer.");

    let seen = seen.lock().unwrap();
    let offered: Vec<&str> = seen[0]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
    let mut expected: Vec<&str> = op_engine::mcp::READ_TOOLS.iter().copied().filter(|n| *n != "run_sql").collect();
    expected.push("get_morning_brief");
    // @F: the pre-send check is a read tool too.
    expected.push("check_boundary");
    // @S: the schedule is a read tool too; schedule_strips (a write) is never offered.
    expected.push("get_schedule");
    assert_eq!(offered, expected, "the read tools, never run_sql or propose_boundary");
    let result = seen[1]["messages"][2]["content"][0]["content"].as_str().unwrap();
    let brief: Value = serde_json::from_str(result).unwrap();
    assert_eq!(brief["lines"][0], "Cows: STAY in P1.");
}
