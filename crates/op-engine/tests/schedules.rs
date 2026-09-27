//! Strip schedules in farm time (field-ready §2.14, S), through the real
//! routes: making one from previewed strips, occurrences across a DST change,
//! the queue routes and roles, the decision about a schedule (STAY keeps,
//! HOLD repeats today's strip, N on a STAY holds, MOVE elsewhere ends it,
//! HOLD without a schedule fails), the heuristic, the MCP tools and the
//! brief's `schedule` line.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use chrono_tz::Tz;
use http_body_util::BodyExt;
use op_core::tools::ToolScope;
use op_core::{Ctx, Decision, DecisionAction, DecisionSource, DecisionStatus, Identity, Role, Via, time};
use op_engine::{brief, context, cycle, db};
use serde_json::{Value, json};
use tower::ServiceExt;

struct T {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    app: Router,
    base: Router,
}

async fn setup() -> T {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let base = Router::new().merge(op_core::router()).merge(op_ingest::router()).merge(op_engine::router()).with_state(ctx.clone());
    let app = op_core::with_identity(base.clone(), Identity::owner(Via::Local));
    op_engine::register_tools(&ctx);
    op_engine::schedules::register_brief(&ctx);
    T { _dir: dir, ctx, app, base }
}

impl T {
    async fn on(&self, app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let b = Request::builder().method(method).uri(path).header("host", "127.0.0.1");
        let req = match body {
            Some(v) => b.header("content-type", "application/json").body(Body::from(v.to_string())),
            None => b.body(Body::empty()),
        }
        .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn req(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.on(&self.app, method, path, body).await
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, v) = self.req(method, path, body).await;
        assert!(s.is_success(), "{method} {path}: {s} {v}");
        v
    }

    fn as_role(&self, role: Role) -> Router {
        op_core::with_identity(self.base.clone(), Identity { role, user_id: None, name: None, via: Via::UserToken })
    }
}

fn square(lon: f64, lat: f64) -> Value {
    json!({ "type": "Polygon", "coordinates": [[[lon, lat], [lon + 0.005, lat], [lon + 0.005, lat + 0.0036], [lon, lat + 0.0036], [lon, lat]]] })
}

fn utc(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
}

fn chicago() -> Tz {
    "America/Chicago".parse().unwrap()
}

struct Farm {
    herd: String,
    p1: String,
    p2: String,
    strips: Vec<Value>,
}

/// Farm near Ames, P1 and P2, Cows in P1 cut into six strips advancing east,
/// the herd sent strip 1 (no collars: it takes it at once).
async fn farm(t: &T) -> Farm {
    t.ok("POST", "/api/farm", Some(json!({ "name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03] }))).await;
    let p1 = t.ok("POST", "/api/paddocks", Some(json!({ "name": "P1", "geometry": square(-93.625, 42.03) }))).await;
    let p2 = t.ok("POST", "/api/paddocks", Some(json!({ "name": "P2", "geometry": square(-93.62, 42.03) }))).await;
    let herd = t.ok("POST", "/api/herds", Some(json!({ "name": "Cows", "species": "cattle", "count": 12, "paddock_id": p1["id"] }))).await;
    let herd = herd["id"].as_str().unwrap().to_owned();
    let preview = t.ok("POST", "/api/strips/preview", Some(json!({ "paddock_id": p1["id"], "orientation_deg": 90, "count": 6 }))).await;
    let strips: Vec<Value> = preview["strips"].as_array().unwrap().iter().map(|s| s["geometry"].clone()).collect();
    assert_eq!(strips.len(), 6);
    t.ok("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({ "geometry": strips[0] }))).await;
    Farm { herd, p1: p1["id"].as_str().unwrap().to_owned(), p2: p2["id"].as_str().unwrap().to_owned(), strips }
}

async fn schedule(t: &T, f: &Farm, extra: Value) -> Value {
    let mut body = json!({ "herd_id": f.herd, "strips": f.strips, "cadence": { "every_days": 1, "at": "07:00" } });
    body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
    let (s, v) = t.req("POST", "/api/schedules", Some(body)).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    v
}

async fn moves(t: &T, id: &str) -> Vec<Value> {
    t.ok("GET", &format!("/api/schedules/{id}/moves"), None).await.as_array().unwrap().clone()
}

fn opens(ms: &[Value]) -> Vec<(u64, DateTime<Utc>)> {
    ms.iter().filter(|m| m["step"] == 0 && m["skipped"] != "held").map(|m| (m["index"].as_u64().unwrap(), utc(m["at"].as_str().unwrap()))).collect()
}

fn local(at: DateTime<Utc>) -> String {
    at.with_timezone(&chicago()).format("%Y-%m-%d %H:%M").to_string()
}

/// A decision the brain made, recorded as the cycle records one.
async fn decided(t: &T, herd: &str, action: DecisionAction, to: Option<&str>) -> Decision {
    let herd = t.ctx.store().get_herd(herd).await.unwrap().unwrap();
    let paddocks = t.ctx.store().list_paddocks().await.unwrap();
    let a = context::assemble(&t.ctx, &herd, false, &|_| {}).await.unwrap();
    let _ = paddocks;
    let mut inputs = json!({});
    if !a.context["schedule"].is_null() {
        inputs["schedule"] = a.context["schedule"].clone();
    }
    let d = Decision {
        id: op_core::id::new_id(op_core::id::DECISION),
        herd_id: herd.id.clone(),
        source: DecisionSource::Heuristic,
        brain: None,
        model: None,
        status: DecisionStatus::Running,
        action: Some(action),
        to_paddock_id: to.map(str::to_owned),
        geometry: None,
        reasoning: Some("Test.".into()),
        confidence: Some(0.6),
        need: None,
        inputs,
        apply_at: None,
        boundary_id: None,
        error: None,
        created_at: time::now(),
        responded_at: None,
        outcome: None,
    };
    db::insert(&t.ctx, &d).await.unwrap();
    cycle::record(&t.ctx, d, &herd).await.unwrap()
}

#[tokio::test]
async fn a_daily_seven_oclock_schedule_opens_at_seven_local_on_both_sides_of_the_dst_change() {
    let t = setup().await;
    let f = farm(&t).await;
    // Saturday 2026-10-31 07:00 CDT; clocks fall back early on Sunday 2026-11-01.
    let s = schedule(&t, &f, json!({ "starts_at": "2026-10-31T12:00:00Z" })).await;
    assert_eq!((s["next_index"].as_u64(), s["status"].as_str()), (Some(1), Some("active")));
    let ms = moves(&t, s["id"].as_str().unwrap()).await;
    let o = opens(&ms);
    assert_eq!(o.len(), 5);
    assert_eq!(o[0].1, utc("2026-10-31T12:00:00Z"));
    assert_eq!(o[1].1, utc("2026-11-01T13:00:00Z"), "07:00 CST is 13:00 UTC");
    assert_eq!(o[2].1, utc("2026-11-02T13:00:00Z"));
    let days: Vec<String> = o.iter().map(|(_, at)| local(*at)).collect();
    assert_eq!(days, ["2026-10-31 07:00", "2026-11-01 07:00", "2026-11-02 07:00", "2026-11-03 07:00", "2026-11-04 07:00"]);
    // The back fence closes 4 h after each open, three steps 10 minutes apart (defaults).
    let closes: Vec<String> = ms.iter().filter(|m| m["index"] == 2 && m["step"].as_u64() > Some(0)).map(|m| local(utc(m["at"].as_str().unwrap()))).collect();
    assert_eq!(closes, ["2026-11-01 11:00", "2026-11-01 11:10", "2026-11-01 11:20"]);
    // With no start given it is the next time the farm's clock reads 07:00.
    let (st, p) = t.req("POST", "/api/schedules/preview", Some(json!({ "herd_id": f.herd, "strips": f.strips }))).await;
    assert_eq!(st, StatusCode::CONFLICT, "one schedule per herd: {p}");
}

#[tokio::test]
async fn the_routes_make_list_and_edit_a_schedule_for_managers_only() {
    let t = setup().await;
    let f = farm(&t).await;
    // Preview first: nothing stored.
    let p = t.ok("POST", "/api/schedules/preview", Some(json!({ "herd_id": f.herd, "strips": f.strips }))).await;
    let first = utc(p["moves"][0]["at"].as_str().unwrap());
    assert_eq!(first.with_timezone(&chicago()).format("%H:%M").to_string(), "07:00");
    assert!(first > Utc::now());
    assert!(t.ok("GET", "/api/schedules", None).await.as_array().unwrap().is_empty());
    // Viewers and hands read; only managers and up make and change.
    for role in [Role::Viewer, Role::Hand] {
        let (st, _) = t.on(&t.as_role(role), "POST", "/api/schedules", Some(json!({ "herd_id": f.herd, "strips": f.strips }))).await;
        assert_eq!(st, StatusCode::FORBIDDEN);
    }
    let s = schedule(&t, &f, json!({ "back_fence": { "enabled": false } })).await;
    let id = s["id"].as_str().unwrap().to_owned();
    assert_eq!(s["created_by"], json!({ "via": "local" }));
    let (st, v) = t.on(&t.as_role(Role::Viewer), "GET", &format!("/api/schedules/{id}"), None).await;
    assert_eq!((st, v["id"].clone()), (StatusCode::OK, json!(id)));
    for path in ["pause", "hold", "move-now", "end"] {
        let (st, _) = t.on(&t.as_role(Role::Hand), "POST", &format!("/api/schedules/{id}/{path}"), None).await;
        assert_eq!(st, StatusCode::FORBIDDEN, "{path}");
    }
    let listed = t.ok("GET", &format!("/api/schedules?herd_id={}&status=running", f.herd), None).await;
    assert_eq!(listed.as_array().unwrap().len(), 1);
    // Without a back fence each open keeps every strip so far.
    let ms = moves(&t, &id).await;
    assert_eq!(ms.len(), 5);
    // Skip strip 3 by index; strip 4 takes its day.
    let before = opens(&ms);
    t.ok("POST", &format!("/api/schedules/{id}/skip"), Some(json!({ "index": 2 }))).await;
    let after = opens(&moves(&t, &id).await);
    assert_eq!(after.iter().find(|(k, _)| *k == 3).unwrap().1, before[1].1);
    // Edit a time, hold, pause, resume, end.
    let at = before[3].1 + Duration::hours(1);
    t.ok("POST", &format!("/api/schedules/{id}/time"), Some(json!({ "index": 5, "at": at }))).await;
    assert_eq!(opens(&moves(&t, &id).await).iter().find(|(k, _)| *k == 5).unwrap().1, at);
    t.ok("POST", &format!("/api/schedules/{id}/hold"), None).await;
    let held = opens(&moves(&t, &id).await);
    assert_eq!(held[0].1, before[1].1, "strip 2 now opens a day later");
    assert_eq!(held[0].1.with_timezone(&chicago()).format("%H:%M").to_string(), "07:00");
    let s = t.ok("POST", &format!("/api/schedules/{id}/pause"), None).await;
    assert_eq!(s["status"], "paused");
    let (st, _) = t.req("POST", &format!("/api/schedules/{id}/pause"), None).await;
    assert_eq!(st, StatusCode::CONFLICT);
    let s = t.ok("POST", &format!("/api/schedules/{id}/resume"), None).await;
    assert_eq!(s["status"], "active");
    let s = t.ok("POST", &format!("/api/schedules/{id}/move-now"), None).await;
    assert_eq!(s["next_index"], 3, "strip 2 opened now; strip 3 is skipped, so strip 4 is next");
    let s = t.ok("POST", &format!("/api/schedules/{id}/end"), None).await;
    assert_eq!(s["status"], "done");
    // Bad bodies.
    let (st, _) = t.req("POST", "/api/schedules", Some(json!({ "herd_id": f.herd, "strips": f.strips, "cadence": { "every_days": 0 } }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST);
    let (st, _) = t.req("POST", "/api/schedules", Some(json!({ "herd_id": f.herd }))).await;
    assert_eq!(st, StatusCode::BAD_REQUEST, "no strips and no layout");
    let (st, _) = t.req("GET", "/api/schedules/sch_nope", None).await;
    assert_eq!(st, StatusCode::NOT_FOUND);
    let (st, _) = t.req("POST", "/api/schedules", Some(json!({ "herd_id": f.herd, "strips": f.strips, "extra": 1 }))).await;
    assert_eq!(st, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test]
async fn a_saved_layout_is_a_schedule_s_strips() {
    let t = setup().await;
    let f = farm(&t).await;
    let layout = t.ok("POST", "/api/layouts", Some(json!({ "paddock_id": f.p1, "orientation_deg": 90, "count": 6 }))).await;
    let (st, s) = t.req("POST", "/api/schedules", Some(json!({ "herd_id": f.herd, "layout_id": layout["id"] }))).await;
    assert_eq!(st, StatusCode::CREATED, "{s}");
    assert_eq!((s["layout_id"].clone(), s["paddock_id"].clone()), (layout["id"].clone(), json!(f.p1)));
    assert_eq!(s["strips"], layout["strips"]);
}

#[tokio::test]
async fn n_on_a_stay_holds_today_s_strip_and_y_keeps_the_schedule() {
    let t = setup().await;
    let f = farm(&t).await;
    let s = schedule(&t, &f, json!({})).await;
    let id = s["id"].as_str().unwrap().to_owned();
    let before = opens(&moves(&t, &id).await);

    // The decision is about the schedule: the context names the next strip.
    let d = decided(&t, &f.herd, DecisionAction::Stay, None).await;
    assert_eq!(d.status, DecisionStatus::Proposed);
    assert_eq!(d.inputs["schedule"]["next"]["strip"], 2);
    assert_eq!(d.inputs["schedule"]["next"]["of"], 6);
    // Y keeps it: nothing moves.
    let ok = t.ok("POST", &format!("/api/decisions/{}/respond", d.id), Some(json!({ "action": "approve" }))).await;
    assert_eq!(ok["status"], "approved");
    assert_eq!(opens(&moves(&t, &id).await), before);

    // N on the next STAY holds: every open one day later, the current strip reissued.
    let d = decided(&t, &f.herd, DecisionAction::Stay, None).await;
    let top = t.ok("GET", &format!("/api/herds/{}/boundary", f.herd), None).await["staged"].as_array().unwrap().last().unwrap()["version"].as_u64().unwrap();
    let held = t.ok("POST", &format!("/api/decisions/{}/respond", d.id), Some(json!({ "action": "reject", "note": "Plenty left in strip 1." }))).await;
    assert_eq!((held["action"].as_str(), held["status"].as_str()), (Some("HOLD"), Some("applied")), "{held}");
    assert_eq!(held["inputs"]["proposed_action"], "STAY");
    assert_eq!(held["inputs"]["farmer_response"]["action"], "reject");
    let after = moves(&t, &id).await;
    let shifted = opens(&after);
    for (a, b) in before.iter().zip(&shifted) {
        assert_eq!(a.0, b.0);
        assert_eq!(local(a.1 + Duration::days(1)), local(b.1), "one cadence later, same local time");
    }
    assert_eq!(after.iter().filter(|m| m["skipped"] == "held").count(), 1);
    let status = t.ok("GET", &format!("/api/herds/{}/boundary", f.herd), None).await;
    assert_eq!(status["active"]["decision_id"], json!(id), "today's strip went again");
    assert!(status["active"]["version"].as_u64().unwrap() > top);

    // HOLD proposed by the brain applies the same way.
    let d = decided(&t, &f.herd, DecisionAction::Hold, None).await;
    assert_eq!(d.status, DecisionStatus::Proposed);
    let applied = t.ok("POST", &format!("/api/decisions/{}/respond", d.id), Some(json!({ "action": "approve" }))).await;
    assert_eq!(applied["status"], "applied");
    assert_eq!(opens(&moves(&t, &id).await)[0].1, shifted[0].1 + Duration::days(1));
}

#[tokio::test]
async fn hold_without_a_schedule_fails_and_a_move_elsewhere_ends_one() {
    let t = setup().await;
    let f = farm(&t).await;
    let d = decided(&t, &f.herd, DecisionAction::Hold, None).await;
    assert_eq!(d.status, DecisionStatus::Failed);
    assert!(d.error.as_deref().unwrap_or_default().contains("HOLD needs an active strip schedule"), "{:?}", d.error);

    let s = schedule(&t, &f, json!({})).await;
    let d = decided(&t, &f.herd, DecisionAction::Move, Some(&f.p2)).await;
    assert_eq!(d.status, DecisionStatus::Proposed);
    let moved = t.ok("POST", &format!("/api/decisions/{}/respond", d.id), Some(json!({ "action": "approve" }))).await;
    assert_eq!(moved["status"], "applied");
    let s = t.ok("GET", &format!("/api/schedules/{}", s["id"].as_str().unwrap()), None).await;
    assert_eq!(s["status"], "done", "a move to P2 ends the P1 schedule");
    assert!(op_ingest::schedule::running(&t.ctx, &f.herd).await.unwrap().is_none());
    // A rejected STAY with no schedule is just a rejection.
    let d = decided(&t, &f.herd, DecisionAction::Stay, None).await;
    let r = t.ok("POST", &format!("/api/decisions/{}/respond", d.id), Some(json!({ "action": "reject" }))).await;
    assert_eq!((r["status"].as_str(), r["action"].as_str()), (Some("rejected"), Some("STAY")));
}

#[tokio::test]
async fn the_heuristic_keeps_or_holds_the_schedule() {
    let t = setup().await;
    let f = farm(&t).await;
    schedule(&t, &f, json!({})).await;
    let herd = t.ctx.store().get_herd(&f.herd).await.unwrap().unwrap();
    let a = context::assemble(&t.ctx, &herd, false, &|_| {}).await.unwrap();
    let sc = &a.context["schedule"];
    assert_eq!((sc["status"].as_str(), sc["strips"].as_u64(), sc["next"]["strip"].as_u64()), (Some("active"), Some(6), Some(2)));
    let out = op_brain::heuristic::decide(&a.context);
    assert_eq!(out.action, DecisionAction::Stay);
    assert!(out.reasoning.contains("strip 2 of 6"), "{}", out.reasoning);
    // A fresh field note saying there's plenty holds.
    let mut ctx = a.context.clone();
    ctx["observations"] = json!([{ "content": "Plenty of grass left, good residual.", "paddock_id": f.p1, "source": "farmer-note" }]);
    let out = op_brain::heuristic::decide(&ctx);
    assert_eq!(out.action, DecisionAction::Hold);
    // So does a strip holding two days or more; one that is grazed down keeps moving.
    let mut ctx = a.context.clone();
    ctx["schedule"]["today"] = json!({ "area_ha": 2.7, "days": 2.5 });
    assert_eq!(op_brain::heuristic::decide(&ctx).action, DecisionAction::Hold);
    ctx["observations"] = json!([{ "content": "Strip is short and bare.", "paddock_id": f.p1, "source": "farmer-note" }]);
    assert_eq!(op_brain::heuristic::decide(&ctx).action, DecisionAction::Stay);
}

#[tokio::test]
async fn mcp_tools_and_the_brief_line() {
    let t = setup().await;
    let f = farm(&t).await;
    let owner = Identity::owner(Via::Local);
    let none = t.ctx.tools().call(&t.ctx, "get_schedule", json!({}), None, owner.clone(), &ToolScope::Full).await.unwrap();
    assert!(none["schedule"].is_null());
    // schedule_strips cuts the herd's paddock and schedules it.
    let made = t
        .ctx
        .tools()
        .call(&t.ctx, "schedule_strips", json!({ "orientation_deg": 90, "count": 6, "every_days": 2, "at": "06:30" }), None, owner.clone(), &ToolScope::Full)
        .await
        .unwrap();
    assert_eq!(made["schedule"]["cadence"], json!({ "every_days": 2, "at": "06:30" }));
    assert!(made["message"].as_str().unwrap().starts_with("Scheduled: strip 2 of 6 opens"), "{made}");
    let got = t.ctx.tools().call(&t.ctx, "get_schedule", json!({ "herd_id": f.herd }), None, Identity::brain(), &ToolScope::Full).await.unwrap();
    assert_eq!(got["schedule"]["id"], made["schedule"]["id"]);
    assert_eq!(got["next"]["strip"], 2);
    assert!(!got["moves"].as_array().unwrap().is_empty());
    // A viewer reads; only managers schedule. Neither is a decision-brain tool.
    let spec = t.ctx.tools().get("schedule_strips").unwrap();
    assert!(!spec.read && spec.min_role == Role::Manager && !spec.brain);
    let viewer = Identity { role: Role::Viewer, user_id: None, name: None, via: Via::UserToken };
    let e = t.ctx.tools().call(&t.ctx, "schedule_strips", json!({}), None, viewer, &ToolScope::Full).await.unwrap_err();
    assert_eq!(e.status, StatusCode::FORBIDDEN);
    assert!(t.ctx.tools().get("get_schedule").unwrap().read);
    assert!(!t.ctx.tools().brain_tools().iter().any(|n| n.contains("schedule")));

    // The brief's line: the next strip, when it opens, how many collars store it (none here).
    let herd = t.ctx.store().get_herd(&f.herd).await.unwrap().unwrap();
    let b = brief::brief(&t.ctx, &herd, time::now()).await.unwrap();
    let line = b.lines.iter().find(|l| l.starts_with("Strip ")).cloned().unwrap_or_default();
    assert!(line.starts_with("Strip 2 of 6 opens ") && line.ends_with("06:30"), "{:?}", b.lines);
    let id = made["schedule"]["id"].as_str().unwrap();
    t.ok("POST", &format!("/api/schedules/{id}/pause"), None).await;
    let b = brief::brief(&t.ctx, &herd, time::now()).await.unwrap();
    assert!(b.lines.iter().any(|l| l == "Schedule paused before strip 2 of 6."), "{:?}", b.lines);
}
