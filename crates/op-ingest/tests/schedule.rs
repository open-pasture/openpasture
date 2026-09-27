//! Strip schedules on the collars (field-ready §2.14, S): staging sized by
//! the tightest reporting collar, the collars' own clocks opening strips with
//! the server away, restaging above sweeps and lone boundaries, the late rule,
//! escapes, and the queue edits (skip, hold, move now, edit time, pause,
//! resume, end).
//!
//! Times: these tests hand the schedule a plain occurrence function (every
//! ten minutes from the start); farm-time occurrences are op-engine's.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use http_body_util::BodyExt;
use op_core::schedule::{BackFence, Cadence, MoveState, Schedule, ScheduleStatus, ScheduledMove};
use op_core::{Actor, Ctx, Via};
use op_geo::{CollarLimits, Polygon, Projection};
use op_ingest::schedule::{self as sched, NewSchedule};
use op_protocol::{BoundaryCommand, SlotStore};
use serde_json::{Value, json};
use tower::ServiceExt;

const MID: [f64; 2] = [-92.405, 38.125];
/// Minutes between opens in these tests.
const EVERY: i64 = 10;

fn m_at(x: f64, y: f64) -> [f64; 2] {
    Projection::new(MID).inverse([x, y])
}

fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Polygon {
    Polygon::from_ring(vec![m_at(x0, y0), m_at(x1, y0), m_at(x1, y1), m_at(x0, y1)])
}

/// Six 50 m strips across the 300 x 200 m paddock, advancing east.
fn strips() -> Vec<Polygon> {
    (0..6).map(|i| rect(50.0 * i as f64, 0.0, 50.0 * (i + 1) as f64, 200.0)).collect()
}

fn ts(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn device(limits: CollarLimits) -> Value {
    json!({"fw": "0.2.0", "caps": ["holes", "slots", "collar_id", "cue_mode", "episodes", "config"], "limits": limits})
}

/// Close two steps, 2 and 3 minutes after each open.
fn quick_fence() -> BackFence {
    BackFence { enabled: true, lag_strips: 0, close_after_min: 2, close_steps: 2, close_every_min: 1 }
}

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

    async fn raw(&self, method: &str, path: &str, key: Option<&str>, body: Option<Value>) -> (StatusCode, Vec<u8>) {
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
        (status, res.into_body().collect().await.unwrap().to_bytes().to_vec())
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let (s, bytes) = self.raw(method, path, None, body).await;
        (s, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() })
    }

    /// Farm, a 300 x 200 m paddock and a herd in it; returns (herd, paddock).
    async fn herd(&self, name: &str) -> (String, String) {
        if self.call("GET", "/api/farm", None).await.0 != StatusCode::OK {
            let (s, v) = self.call("POST", "/api/farm", Some(json!({"name": "Home", "timezone": "America/Chicago", "center": MID}))).await;
            assert_eq!(s, StatusCode::CREATED, "{v}");
        }
        let (s, pad) = self.call("POST", "/api/paddocks", Some(json!({"name": format!("{name} paddock"), "geometry": rect(0.0, 0.0, 300.0, 200.0)}))).await;
        assert_eq!(s, StatusCode::CREATED, "{pad}");
        let (s, h) = self.call("POST", "/api/herds", Some(json!({"name": name, "species": "cattle", "count": 0, "paddock_id": pad["id"]}))).await;
        assert_eq!(s, StatusCode::CREATED, "{h}");
        (h["id"].as_str().unwrap().to_owned(), pad["id"].as_str().unwrap().to_owned())
    }

    /// A firmware 0.2 collar with `limits`, standing in strip 1.
    async fn collar(&self, herd: &str, limits: CollarLimits) -> Dev {
        let (s, v) = self.call("POST", "/api/collars", Some(json!({"herd_id": herd}))).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        let dev = Dev {
            id: v["collar"]["id"].as_str().unwrap().to_owned(),
            key: v["key"].as_str().unwrap().to_owned(),
            store: SlotStore::new(limits, Some(herd.to_owned()), None),
        };
        self.report(&dev, json!({"device": device(limits), "fixes": [fix(m_at(25.0, 100.0), Utc::now())]})).await;
        dev
    }

    async fn report(&self, dev: &Dev, body: Value) {
        let (s, v) = self.raw("POST", "/collar/v1/report", Some(&dev.key), Some(body)).await;
        assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&v));
    }

    /// Download until the server has nothing more for this collar, storing
    /// each command and acking it; then report what it holds. `now` is its clock.
    async fn sync(&self, dev: &mut Dev, now: DateTime<Utc>) -> Vec<u32> {
        let mut got = Vec::new();
        for _ in 0..40 {
            let mut q = format!("have={}&free={}", dev.store.have(), dev.store.free());
            if let Some(b) = dev.store.free_bytes() {
                q.push_str(&format!("&free_bytes={b}"));
            }
            let (s, bytes) = self.raw("GET", &format!("/collar/v1/boundary?{q}"), Some(&dev.key), None).await;
            if s == StatusCode::NO_CONTENT {
                break;
            }
            assert_eq!(s, StatusCode::OK, "{}", String::from_utf8_lossy(&bytes));
            let cmd: BoundaryCommand = op_protocol::verify_wire(&bytes, &self.ctx.public_key()).expect("signed");
            got.push(cmd.version);
            let ack = dev.store.insert(cmd, Some(now));
            self.ack(dev, &ack.to_ack(now)).await;
        }
        let slots = dev.store.report();
        self.report(dev, json!({"slots": slots})).await;
        got
    }

    async fn ack(&self, dev: &Dev, ack: &op_protocol::Ack) {
        let (s, v) = self.raw("POST", "/collar/v1/ack", Some(&dev.key), Some(serde_json::to_value(ack).unwrap())).await;
        assert_eq!(s, StatusCode::NO_CONTENT, "{}", String::from_utf8_lossy(&v));
    }

    /// The herd's boundary: strip 1 (the herd is standing in it).
    async fn on_strip_one(&self, herd: &str) -> u64 {
        let (s, m) = self.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": strips()[0]}))).await;
        assert_eq!(s, StatusCode::CREATED, "{m}");
        assert_eq!(m["status"], "done", "{m}");
        self.status(herd).await["active"]["version"].as_u64().unwrap()
    }

    async fn status(&self, herd: &str) -> Value {
        self.call("GET", &format!("/api/herds/{herd}/boundary"), None).await.1
    }

    async fn schedule(&self, herd: &str, paddock: &str, start: DateTime<Utc>, bf: BackFence) -> Schedule {
        sched::create(&self.ctx, new_schedule(herd, paddock, start, bf), &every(start)).await.unwrap()
    }

    async fn moves(&self, s: &Schedule) -> Vec<ScheduledMove> {
        sched::moves(&self.ctx, &s.id).await.unwrap()
    }
}

struct Dev {
    id: String,
    key: String,
    store: SlotStore,
}

fn fix(point: [f64; 2], at: DateTime<Utc>) -> Value {
    json!({"at": ts(at), "point": point, "accuracy_m": 2.0, "sats": 9})
}

fn every(start: DateTime<Utc>) -> impl Fn(u32) -> DateTime<Utc> + Send + Sync {
    move |n| start + Duration::minutes(EVERY * i64::from(n))
}

fn new_schedule(herd: &str, paddock: &str, start: DateTime<Utc>, bf: BackFence) -> NewSchedule {
    NewSchedule {
        herd_id: herd.into(),
        paddock_id: paddock.into(),
        layout_id: None,
        strips: strips(),
        next_index: None,
        cadence: Cadence { every_days: 1, at: "07:00".into() },
        starts_at: start,
        back_fence: bf,
        created_by: Actor { via: Via::Local, user_id: None, name: None },
    }
}

fn whole(t: DateTime<Utc>) -> DateTime<Utc> {
    op_protocol::wire_time::trunc_secs(t)
}

fn staged(ms: &[ScheduledMove]) -> Vec<&ScheduledMove> {
    ms.iter().filter(|m| m.state == MoveState::Staged).collect()
}

fn limits(slots: usize, slot_bytes: usize) -> CollarLimits {
    CollarLimits { slots, slot_bytes, ..CollarLimits::V0 }
}

#[tokio::test]
async fn a_schedule_starts_after_the_strip_the_herd_is_on_and_plans_each_back_fence_step() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    assert_eq!((s.next_index, s.status), (1, ScheduleStatus::Active));
    assert_eq!(s.planned_end, Some(start + Duration::minutes(5 * EVERY)), "five strips, one occurrence each");
    let ms = app.moves(&s).await;
    // Strips 2..6 open ten minutes apart; each closes strip behind in two steps.
    assert_eq!(ms.len(), 15);
    let opens: Vec<(u32, DateTime<Utc>)> = ms.iter().filter(|m| m.step == 0).map(|m| (m.index, m.at)).collect();
    assert_eq!(opens, (1..6).map(|k| (k, start + Duration::minutes(EVERY * (k as i64 - 1)))).collect::<Vec<_>>());
    let area = |p: &Polygon| p.area_ha() * 10_000.0;
    let open2 = &ms[0];
    assert!((area(&open2.geometry) - 100.0 * 200.0).abs() < 100.0, "strips 1 and 2");
    let closes: Vec<&ScheduledMove> = ms.iter().filter(|m| m.index == 1 && m.step > 0).collect();
    assert_eq!(closes.iter().map(|m| m.at).collect::<Vec<_>>(), vec![start + Duration::minutes(2), start + Duration::minutes(3)]);
    assert!((area(&closes[0].geometry) - 75.0 * 200.0).abs() < 100.0, "half of strip 1 left");
    assert!((area(&closes[1].geometry) - 50.0 * 200.0).abs() < 100.0, "strip 2 alone");
    // Everything fits on a V0 collar: staged as boundaries at their times, versions rising.
    let st = staged(&ms);
    assert_eq!(st.len(), 15);
    let versions: Vec<u32> = st.iter().map(|m| m.boundary_version.unwrap()).collect();
    assert!(versions.windows(2).all(|w| w[0] < w[1]), "{versions:?}");
    let status = app.status(&herd).await;
    let pending: Vec<&Value> = status["staged"].as_array().unwrap().iter().collect();
    assert_eq!(pending.len(), 15);
    assert_eq!(pending[0]["effective_at"], json!(op_protocol::wire_time::format(&start)));
    assert_eq!(pending[0]["decision_id"], json!(s.id), "a schedule's boundaries carry its id");
}

#[tokio::test]
async fn staging_fills_the_room_the_tightest_reporting_collar_has() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    let small = app.collar(&herd, limits(5, 0)).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    // Five slots less the one in effect.
    let ms = app.moves(&s).await;
    assert_eq!(staged(&ms).len(), 4);
    assert_eq!(ms.iter().filter(|m| m.state == MoveState::Planned).count(), 11);
    // Parked, the small collar doesn't count: the V0 one has room for the rest.
    let (st, v) = app.call("POST", &format!("/api/collars/{}/park", small.id), Some(json!({"reason": "charging"}))).await;
    assert_eq!(st, StatusCode::OK, "{v}");
    sched::drive(&app.ctx, &s.id, Utc::now()).await.unwrap();
    let ms2 = app.moves(&s).await;
    assert_eq!(staged(&ms2).len(), 15);
    // The four already staged kept their versions.
    assert_eq!(
        staged(&ms)[..4].iter().map(|m| m.boundary_version).collect::<Vec<_>>(),
        staged(&ms2)[..4].iter().map(|m| m.boundary_version).collect::<Vec<_>>()
    );

    // Bytes: room for the boundary in effect and two more 4-corner records.
    let (herd2, pad2) = app.herd("Heifers").await;
    app.collar(&herd2, CollarLimits::V0).await;
    let tight = app.collar(&herd2, limits(16, 3 * CollarLimits::record_bytes(4))).await;
    app.on_strip_one(&herd2).await;
    let s2 = app.schedule(&herd2, &pad2, start, quick_fence()).await;
    assert_eq!(staged(&app.moves(&s2).await).len(), 2);
    // Silent for an hour: it no longer sizes the staging.
    sqlx::query("UPDATE collars SET last_seen = ? WHERE id = ?").bind(ts(Utc::now() - Duration::hours(1))).bind(&tight.id).execute(app.ctx.db()).await.unwrap();
    sched::drive(&app.ctx, &s2.id, Utc::now()).await.unwrap();
    assert_eq!(staged(&app.moves(&s2).await).len(), 15);

    // Never more than the slots less one, on a V0 collar that holds nothing yet.
    let room = sched::budget(&app.ctx, &herd, &op_ingest::herd_boundaries(app.ctx.db(), &herd, Utc::now()).await.unwrap(), &Default::default(), Utc::now())
        .await
        .unwrap();
    assert!(room.count <= CollarLimits::V0.slots - 1);
}

#[tokio::test]
async fn every_staged_move_opens_on_the_collars_own_clock_with_the_server_away() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    let mut a = app.collar(&herd, CollarLimits::V0).await;
    let mut b = app.collar(&herd, CollarLimits::V0).await;
    let v1 = app.on_strip_one(&herd).await as u32;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    let now = Utc::now();
    let got_a = app.sync(&mut a, now).await;
    let got_b = app.sync(&mut b, now).await;
    let ms = app.moves(&s).await;
    let versions: Vec<u32> = ms.iter().map(|m| m.boundary_version.unwrap()).collect();
    assert_eq!(got_a, [&[v1][..], &versions[..]].concat(), "the boundary in effect, then every staged move in order");
    assert_eq!(got_b, got_a);
    assert_eq!(a.store.staged().len(), 15);
    // The ack line: every collar stores the next open.
    let (next, count) = sched::next_open(&app.ctx, &s).await.unwrap().unwrap();
    let count = count.unwrap();
    assert_eq!((next.index, count.stored, count.collars), (1, 2, 2));

    // The server is away. Each collar opens every strip and closes every back
    // fence on its own clock, a fix a second after each time.
    let mut acks = Vec::new();
    for m in &ms {
        let t = m.at + Duration::seconds(1);
        for dev in [&mut a, &mut b] {
            let ack = dev.store.tick(t).expect("applies at its time");
            assert_eq!(ack.version, m.boundary_version.unwrap());
            assert_eq!(dev.store.active().unwrap().cmd.version, m.boundary_version.unwrap());
            acks.push((ack, t));
        }
    }
    // The last one it holds is strip 6 alone.
    let last = a.store.active().unwrap().cmd.polygon();
    assert!((last.area_ha() * 1e4 - 50.0 * 200.0).abs() < 100.0);
    assert!(last.contains(m_at(275.0, 100.0)) && !last.contains(m_at(240.0, 100.0)));

    // Back: the acks arrive late, with the collars' own times; the server
    // marks every move done and the schedule over.
    for (i, (ack, t)) in acks.iter().enumerate() {
        let dev = if i % 2 == 0 { &a } else { &b };
        app.ack(dev, &ack.to_ack(*t)).await;
    }
    let after = ms.last().unwrap().at + Duration::minutes(1);
    sched::drive(&app.ctx, &s.id, after).await.unwrap();
    let done = app.moves(&s).await;
    assert!(done.iter().all(|m| m.state == MoveState::Done), "{done:?}");
    for m in &done {
        assert_eq!(m.applied_at, Some(m.at + Duration::seconds(1)), "the first collar's own apply time");
    }
    let s = sched::get(&app.ctx, &s.id).await.unwrap().unwrap();
    assert_eq!((s.status, s.next_index), (ScheduleStatus::Done, 6));
    assert!(sched::running(&app.ctx, &herd).await.unwrap().is_none());
}

#[tokio::test]
async fn a_sweep_drops_the_staged_moves_and_they_are_staged_again_once_it_ends() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    let before: Vec<u32> = app.moves(&s).await.iter().filter_map(|m| m.boundary_version).collect();
    assert_eq!(before.len(), 15);

    // The farmer sends the herd to the far strip: a sweep, one immediate step at a time.
    let (st, m) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": strips()[5]}))).await;
    assert_eq!(st, StatusCode::CREATED, "{m}");
    assert_eq!(m["status"], "sweeping", "{m}");
    let stepped = app.status(&herd).await["active"]["version"].as_u64().unwrap() as u32;
    assert!(stepped > *before.last().unwrap());
    // While it sweeps nothing is staged again, and nothing counts as staged.
    sched::drive(&app.ctx, &s.id, Utc::now()).await.unwrap();
    let ms = app.moves(&s).await;
    assert!(staged(&ms).is_empty(), "the sweep's step dropped them");
    assert!(app.status(&herd).await["staged"].as_array().is_none_or(|a| a.is_empty()));

    // The move ends part way: staged again above the sweep's last step, the opens at the same times.
    let (st, _) = app.call("POST", &format!("/api/herds/{herd}/move/stop"), None).await;
    assert_eq!(st, StatusCode::OK);
    sched::drive(&app.ctx, &s.id, Utc::now()).await.unwrap();
    let ms2 = app.moves(&s).await;
    assert_eq!(staged(&ms2).len(), 15, "as many as the collar has room for");
    assert!(staged(&ms2).iter().all(|m| m.boundary_version.unwrap() > stepped));
    let opens = |ms: &[ScheduledMove]| ms.iter().filter(|m| m.step == 0).map(|m| (m.index, m.at)).collect::<Vec<_>>();
    assert_eq!(opens(&ms), opens(&ms2));
    // The herd is spread over the sweep's ground: strip 2's open keeps all of it,
    // and its back fence closes it from both ends onto strip 2.
    let swept: Polygon = serde_json::from_value(app.status(&herd).await["active"]["geometry"].clone()).unwrap();
    let open2 = ms2.iter().find(|m| m.index == 1 && m.step == 0).unwrap();
    for x in [25.0, 75.0, 275.0] {
        assert!(swept.contains(m_at(x, 100.0)) && open2.geometry.contains(m_at(x, 100.0)), "x {x}");
    }
    let closes: Vec<&ScheduledMove> = ms2.iter().filter(|m| m.index == 1 && m.step > 0).collect();
    let last = closes.iter().max_by_key(|m| m.step).unwrap();
    assert!((last.geometry.area_ha() * 1e4 - 50.0 * 200.0).abs() < 100.0, "strip 2 alone");
    assert!(closes.len() > 2 && closes.iter().all(|m| m.at < opens(&ms2)[1].1));
    assert!(!closes[0].geometry.contains(m_at(298.0, 100.0)) && !closes[0].geometry.contains(m_at(23.0, 100.0)), "from both ends");
}

#[tokio::test]
async fn a_sweep_isnt_held_up_by_a_schedule_s_staged_boundaries() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    let dev = app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    let (st, m) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": strips()[5]}))).await;
    assert_eq!((st, m["status"].as_str(), m["step"].as_u64()), (StatusCode::CREATED, Some("sweeping"), Some(1)));
    // Something of the schedule's is staged above the sweep's step (as after a restage).
    let later = Utc::now() + Duration::hours(2);
    let opts = op_ingest::SendOpts { effective_at: Some(later), ..Default::default() };
    op_ingest::send_boundary(&app.ctx, &herd, strips()[1].clone(), opts, &s.id).await.unwrap();
    assert_eq!(app.status(&herd).await["staged"].as_array().map_or(0, |a| a.len()), 1);
    // The herd walks east; the sweep steps on regardless.
    let t = Utc::now() + Duration::seconds(35);
    app.report(&dev, json!({"fixes": [fix(m_at(70.0, 100.0), t)]})).await;
    let moved = op_ingest::moves::drive(&app.ctx, &herd, t + Duration::seconds(5)).await.unwrap();
    assert_eq!(moved.map(|m| m.step), Some(2), "a staged boundary that isn't the move's own doesn't freeze it");
}

#[tokio::test]
async fn a_lone_boundary_settles_after_a_minute() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    // Something else sends the herd a boundary on its own (no move).
    let b = op_ingest::send_boundary(&app.ctx, &herd, strips()[0].clone(), Default::default(), "dec_other").await.unwrap();
    sched::drive(&app.ctx, &s.id, Utc::now()).await.unwrap();
    assert!(staged(&app.moves(&s).await).is_empty());
    sched::drive(&app.ctx, &s.id, Utc::now() + Duration::seconds(61)).await.unwrap();
    let ms = app.moves(&s).await;
    assert_eq!(staged(&ms).len(), 15);
    assert!(ms.iter().all(|m| m.boundary_version.unwrap() > b.version));
}

#[tokio::test]
async fn an_open_held_up_is_applied_when_at_most_30_minutes_late_and_marked_late_after() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(5));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    let other = op_ingest::send_boundary(&app.ctx, &herd, strips()[0].clone(), Default::default(), "dec_other").await.unwrap();
    // Ten minutes after strip 2 should have opened (and its back fence closed):
    // late, but within 30 minutes, so each goes now.
    let at = start + Duration::minutes(4);
    sched::drive(&app.ctx, &s.id, at).await.unwrap();
    let ms = app.moves(&s).await;
    let first: Vec<&ScheduledMove> = ms.iter().filter(|m| m.index == 1).collect();
    assert!(first.iter().all(|m| m.state == MoveState::Done && m.boundary_version.unwrap() > other.version), "{first:?}");
    let active = app.status(&herd).await["active"].clone();
    assert!(active["effective_at"].is_null(), "sent as immediate");
    assert_eq!(active["version"].as_u64(), first.last().unwrap().boundary_version.map(u64::from));
    // The later opens go ahead as planned.
    assert!(ms.iter().filter(|m| m.index > 1).all(|m| m.state == MoveState::Staged));

    let (herd2, pad2) = app.herd("Heifers").await;
    app.collar(&herd2, CollarLimits::V0).await;
    app.on_strip_one(&herd2).await;
    // Hourly opens this time.
    let hourly = move |n: u32| start + Duration::minutes(60 * i64::from(n));
    let s2 = sched::create(&app.ctx, new_schedule(&herd2, &pad2, start, quick_fence()), &hourly).await.unwrap();
    op_ingest::send_boundary(&app.ctx, &herd2, strips()[0].clone(), Default::default(), "dec_other").await.unwrap();
    // Forty minutes after strip 2's time: too late, never applied; its back
    // fence steps with it. Strip 3 opens on time.
    let at = start + Duration::minutes(40);
    sched::drive(&app.ctx, &s2.id, at).await.unwrap();
    let ms = app.moves(&s2).await;
    let word = |m: &ScheduledMove| (m.index, m.step, m.state, m.skipped.clone());
    assert!(
        ms.iter().filter(|m| m.index == 1).all(|m| m.state == MoveState::Skipped && m.skipped.as_deref() == Some("late")),
        "{:?}",
        ms.iter().map(word).collect::<Vec<_>>()
    );
    let three: Vec<&ScheduledMove> = ms.iter().filter(|m| m.index == 2).collect();
    assert!(three.iter().all(|m| m.state == MoveState::Staged), "{:?}", ms.iter().map(word).collect::<Vec<_>>());
    assert_eq!(sched::get(&app.ctx, &s2.id).await.unwrap().unwrap().next_index, 2);
}

#[tokio::test]
async fn an_escape_leaves_the_schedule_staged_and_hands_its_copies_back() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    let mut a = app.collar(&herd, CollarLimits::V0).await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    let versions: Vec<u32> = app.moves(&s).await.iter().filter_map(|m| m.boundary_version).collect();
    app.sync(&mut a, Utc::now()).await;
    assert_eq!(a.store.staged().len(), 15);

    // A walks out of strip 1 and stays out: its own pen drops its staged slots.
    let t = Utc::now() + Duration::seconds(5);
    app.report(&a, json!({"fixes": [fix(m_at(25.0, 230.0), t)]})).await;
    op_ingest::escapes::scan(&app.ctx, t + Duration::seconds(60)).await.unwrap();
    let got = app.sync(&mut a, t + Duration::seconds(61)).await;
    assert_eq!(got.len(), 1, "the pen");
    assert!(a.store.staged().is_empty(), "an immediate drops what is staged below it");
    // The herd's schedule is untouched: a pen is that collar's alone.
    sched::drive(&app.ctx, &s.id, Utc::now()).await.unwrap();
    assert_eq!(app.moves(&s).await.iter().filter_map(|m| m.boundary_version).collect::<Vec<_>>(), versions);

    // Back in: copies of the herd's boundary and of every staged move, at their times.
    app.report(&a, json!({"fixes": [fix(m_at(25.0, 100.0), t + Duration::seconds(90))]})).await;
    op_ingest::escapes::scan(&app.ctx, t + Duration::seconds(95)).await.unwrap();
    app.sync(&mut a, t + Duration::seconds(96)).await;
    let times: Vec<Option<DateTime<Utc>>> = a.store.staged().iter().map(|x| x.cmd.effective_at).collect();
    let want: Vec<Option<DateTime<Utc>>> = app.moves(&s).await.iter().map(|m| Some(m.at)).collect();
    assert_eq!(times, want);
    assert!(a.store.staged().iter().all(|x| x.cmd.collar_id.as_deref() == Some(a.id.as_str())));
    // And the copies count as holding the herd's versions.
    let (_, count) = sched::next_open(&app.ctx, &s).await.unwrap().unwrap();
    assert_eq!(count.unwrap().stored, 1);
}

#[tokio::test]
async fn skip_moves_the_later_strips_up_one_occurrence() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    let before = app.moves(&s).await;
    let top = before.iter().filter_map(|m| m.boundary_version).max().unwrap();
    // Skip strip 3: strip 4 opens when strip 3 would have, strip 5 in strip 4's place.
    let s = sched::skip(&app.ctx, &s.id, 2).await.unwrap();
    let ms = app.moves(&s).await;
    let open = |k: u32| ms.iter().find(|m| m.index == k && m.step == 0).unwrap().clone();
    assert_eq!((open(2).state, open(2).skipped.as_deref()), (MoveState::Skipped, Some("skipped")));
    assert!(ms.iter().filter(|m| m.index == 2 && m.step > 0).all(|m| m.state == MoveState::Skipped));
    assert_eq!(open(3).at, start + Duration::minutes(EVERY));
    assert_eq!(open(4).at, start + Duration::minutes(2 * EVERY));
    assert_eq!(open(5).at, start + Duration::minutes(3 * EVERY));
    // Strip 4 opens over the skipped ground: strips 2, 3 and 4, closing to strip 4.
    let area = |p: &Polygon| p.area_ha() * 10_000.0;
    assert!((area(&open(3).geometry) - 150.0 * 200.0).abs() < 150.0);
    // Two strips of old ground: two back-fence steps each, a minute apart.
    let last_close = ms.iter().filter(|m| m.index == 3 && m.step > 0).max_by_key(|m| m.step).unwrap();
    assert!((area(&last_close.geometry) - 50.0 * 200.0).abs() < 100.0);
    assert_eq!((last_close.step, last_close.at), (4, open(3).at + Duration::minutes(5)));
    // The collars dropped what was staged (the current strip went again) and got the new plan.
    let status = app.status(&herd).await;
    let reissued = status["active"]["version"].as_u64().unwrap() as u32;
    assert!(reissued > top && status["active"]["decision_id"] == json!(s.id));
    assert!(staged(&ms).iter().all(|m| m.boundary_version.unwrap() > reissued));
    assert_eq!(staged(&ms).len(), 14);
    assert_eq!(s.next_index, 1);
}

#[tokio::test]
async fn hold_moves_every_open_one_occurrence_and_keeps_the_place() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    let s = sched::hold(&app.ctx, &s.id, &every(start)).await.unwrap();
    let ms = app.moves(&s).await;
    let held: Vec<&ScheduledMove> = ms.iter().filter(|m| m.skipped.as_deref() == Some("held")).collect();
    assert_eq!(held.len(), 1);
    assert_eq!((held[0].index, held[0].at), (0, start), "the herd stays on strip 1 at the time strip 2 would have opened");
    for k in 1..6u32 {
        let o = ms.iter().find(|m| m.index == k && m.step == 0).unwrap();
        assert_eq!(o.at, start + Duration::minutes(EVERY * k as i64), "strip {} one occurrence later", k + 1);
    }
    let close = ms.iter().find(|m| m.index == 1 && m.step == 1).unwrap();
    assert_eq!(close.at, start + Duration::minutes(EVERY + 2));
    let active = &app.status(&herd).await["active"];
    assert_eq!(active["decision_id"], json!(s.id), "the current strip went again as an immediate version");
    assert!(staged(&ms).iter().all(|m| m.boundary_version.unwrap() > active["version"].as_u64().unwrap() as u32));
    // Twice: two held places.
    let s = sched::hold(&app.ctx, &s.id, &every(start)).await.unwrap();
    let ms = app.moves(&s).await;
    assert_eq!(ms.iter().filter(|m| m.skipped.as_deref() == Some("held")).count(), 2);
    assert_eq!(ms.iter().find(|m| m.index == 1 && m.step == 0).unwrap().at, start + Duration::minutes(2 * EVERY));
}

#[tokio::test]
async fn move_now_opens_the_next_strip_at_once_and_later_ones_keep_their_times() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    let s = sched::move_now(&app.ctx, &s.id).await.unwrap();
    let ms = app.moves(&s).await;
    let open2 = ms.iter().find(|m| m.index == 1 && m.step == 0).unwrap();
    assert_eq!(open2.state, MoveState::Done);
    let active = &app.status(&herd).await["active"];
    assert_eq!(active["version"].as_u64(), open2.boundary_version.map(u64::from));
    assert!(active["effective_at"].is_null());
    // Its back fence follows from now; strip 3 and later keep their times.
    let close = ms.iter().find(|m| m.index == 1 && m.step == 2).unwrap();
    assert!((close.at - open2.at - Duration::minutes(3)).num_seconds().abs() <= 1);
    assert_eq!(ms.iter().find(|m| m.index == 2 && m.step == 0).unwrap().at, start + Duration::minutes(EVERY));
    assert_eq!(s.next_index, 2);
    assert!(staged(&ms).iter().all(|m| m.boundary_version.unwrap() > open2.boundary_version.unwrap()));
    assert_eq!(staged(&ms).len(), 14);
}

#[tokio::test]
async fn edit_time_moves_one_open_with_its_back_fence() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    let new = start + Duration::minutes(EVERY + 5);
    let s = sched::set_time(&app.ctx, &s.id, 2, new).await.unwrap();
    let ms = app.moves(&s).await;
    let three: Vec<DateTime<Utc>> = ms.iter().filter(|m| m.index == 2).map(|m| m.at).collect();
    assert_eq!(three, vec![new, new + Duration::minutes(2), new + Duration::minutes(3)]);
    // Staged again in time order.
    let st = staged(&ms);
    assert_eq!(st.len(), 15);
    assert!(st.windows(2).all(|w| w[0].at < w[1].at && w[0].boundary_version < w[1].boundary_version));
    // Not past the next open, nor into the past.
    let e = sched::set_time(&app.ctx, &s.id, 2, start + Duration::minutes(2 * EVERY + 1)).await.unwrap_err();
    assert_eq!(e.status, StatusCode::BAD_REQUEST, "{}", e.message);
    let e = sched::set_time(&app.ctx, &s.id, 2, Utc::now() - Duration::minutes(1)).await.unwrap_err();
    assert_eq!(e.status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn pause_drops_what_is_staged_resume_moves_passed_opens_on_and_end_stops_it() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(2));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    let s = sched::pause(&app.ctx, &s.id).await.unwrap();
    assert_eq!(s.status, ScheduleStatus::Paused);
    let ms = app.moves(&s).await;
    assert!(staged(&ms).is_empty());
    assert!(app.status(&herd).await["staged"].as_array().is_none_or(|a| a.is_empty()), "the collars drop them");
    // Paused, the driver stages nothing, even past an open.
    sched::drive(&app.ctx, &s.id, start + Duration::minutes(1)).await.unwrap();
    assert!(app.moves(&s).await.iter().all(|m| m.state == MoveState::Planned));

    // Resumed after strip 2's time passed: every open moves on by whole occurrences.
    // (The clock can't be wound forward here, so every time goes back three minutes instead.)
    for m in app.moves(&s).await {
        sqlx::query("UPDATE schedule_moves SET at = ? WHERE schedule_id = ? AND strip = ? AND step = ?")
            .bind(op_core::time::to_db(&(m.at - Duration::minutes(3))))
            .bind(&s.id)
            .bind(i64::from(m.index))
            .bind(i64::from(m.step))
            .execute(app.ctx.db())
            .await
            .unwrap();
    }
    let passed = app.moves(&s).await.iter().find(|m| m.index == 1 && m.step == 0).unwrap().at;
    assert!(passed < Utc::now());
    let occ = move |n: u32| passed + Duration::minutes(EVERY * i64::from(n));
    let s = sched::resume(&app.ctx, &s.id, &occ).await.unwrap();
    assert_eq!(s.status, ScheduleStatus::Active);
    let ms = app.moves(&s).await;
    let open2 = ms.iter().find(|m| m.index == 1 && m.step == 0).unwrap();
    assert_eq!(open2.at, passed + Duration::minutes(EVERY));
    assert_eq!(staged(&ms).len(), 15);

    let s = sched::end(&app.ctx, &s.id).await.unwrap();
    assert_eq!(s.status, ScheduleStatus::Done);
    assert!(app.moves(&s).await.iter().all(|m| m.state == MoveState::Skipped));
    assert!(app.status(&herd).await["staged"].as_array().is_none_or(|a| a.is_empty()));
    assert_eq!(sched::end(&app.ctx, &s.id).await.unwrap_err().status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn a_move_to_another_paddock_ends_the_schedule() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    let mut d: op_core::Decision = serde_json::from_value(json!({
        "id": "dec_1", "herd_id": herd, "source": "farmer", "status": "applied", "action": "MOVE", "to_paddock_id": pad,
        "inputs": {}, "created_at": ts(Utc::now())
    }))
    .unwrap();
    // A move inside the same paddock keeps it.
    assert!(sched::on_decision(&app.ctx, &d).await.unwrap().is_none());
    d.to_paddock_id = Some("pad_elsewhere".into());
    let ended = sched::on_decision(&app.ctx, &d).await.unwrap().unwrap();
    assert_eq!(ended.status, ScheduleStatus::Done);
    assert!(sched::running(&app.ctx, &herd).await.unwrap().is_none());
    assert!(app.moves(&s).await.iter().all(|m| m.state == MoveState::Skipped));
}

#[tokio::test]
async fn schedules_that_cant_work_are_refused() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    // No boundary yet: nothing to start from.
    let e = sched::create(&app.ctx, new_schedule(&herd, &pad, start, quick_fence()), &every(start)).await.unwrap_err();
    assert_eq!(e.status, StatusCode::CONFLICT, "{}", e.message);
    app.on_strip_one(&herd).await;
    // A start already past.
    let past = whole(Utc::now() - Duration::minutes(1));
    let e = sched::create(&app.ctx, new_schedule(&herd, &pad, past, quick_fence()), &every(past)).await.unwrap_err();
    assert_eq!(e.status, StatusCode::BAD_REQUEST);
    // A back fence still closing when the next strip opens.
    let slow = BackFence { close_after_min: 9, close_steps: 3, close_every_min: 1, ..quick_fence() };
    let e = sched::create(&app.ctx, new_schedule(&herd, &pad, start, slow), &every(start)).await.unwrap_err();
    assert!(e.message.contains("back fence"), "{}", e.message);
    // Strips that don't touch can't make one boundary.
    let mut apart = new_schedule(&herd, &pad, start, quick_fence());
    apart.strips = vec![strips()[0].clone(), strips()[2].clone()];
    apart.next_index = Some(1);
    let e = sched::create(&app.ctx, apart, &every(start)).await.unwrap_err();
    assert!(e.message.contains("don't join"), "{}", e.message);
    // One running per herd.
    sched::create(&app.ctx, new_schedule(&herd, &pad, start, quick_fence()), &every(start)).await.unwrap();
    let e = sched::create(&app.ctx, new_schedule(&herd, &pad, start, quick_fence()), &every(start)).await.unwrap_err();
    assert_eq!(e.status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn strips_that_overlap_are_refused_rather_than_staged_with_a_hole() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    // The whole paddock, then a strip inside it: opening the second would have staged the
    // first with a hole where the second is.
    let mut inside = new_schedule(&herd, &pad, start, BackFence { enabled: false, ..quick_fence() });
    inside.strips = vec![rect(0.0, 0.0, 300.0, 200.0), rect(100.0, 50.0, 150.0, 150.0)];
    inside.next_index = Some(1);
    let e = sched::create(&app.ctx, inside, &every(start)).await.unwrap_err();
    assert_eq!(e.status, StatusCode::BAD_REQUEST);
    assert_eq!(e.message, "Strips 1 and 2 overlap. Each piece of ground belongs to one strip.");
    // Strips that cross into each other by 10 m, with a back fence.
    let mut over = new_schedule(&herd, &pad, start, quick_fence());
    over.strips = vec![rect(0.0, 0.0, 60.0, 200.0), rect(50.0, 0.0, 100.0, 200.0), rect(100.0, 0.0, 150.0, 200.0)];
    let e = sched::create(&app.ctx, over, &every(start)).await.unwrap_err();
    assert!(e.message.starts_with("Strips 1 and 2 overlap."), "{}", e.message);
    // Strips that only share edges are the usual case and still make a schedule.
    let s = sched::create(&app.ctx, new_schedule(&herd, &pad, start, quick_fence()), &every(start)).await.unwrap();
    assert_eq!(s.status, ScheduleStatus::Active);
}

#[tokio::test]
async fn staging_250_collars_is_quick() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    let mut keys = Vec::new();
    for _ in 0..250 {
        let (s, v) = app.call("POST", "/api/collars", Some(json!({"herd_id": herd}))).await;
        assert_eq!(s, StatusCode::CREATED);
        keys.push(v["key"].as_str().unwrap().to_owned());
    }
    let dev = |k: &str| Dev { id: String::new(), key: k.to_owned(), store: SlotStore::new(CollarLimits::V0, None, None) };
    for k in &keys {
        app.report(&dev(k), json!({"device": device(CollarLimits::V0), "fixes": [fix(m_at(25.0, 100.0), Utc::now())]})).await;
    }
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    for k in &keys {
        let slots: Vec<Value> = app
            .moves(&s)
            .await
            .iter()
            .take(8)
            .map(|m| json!({"version": m.boundary_version, "status": "received", "effective_at": op_protocol::wire_time::format(&m.at)}))
            .collect();
        app.report(&dev(k), json!({"slots": slots})).await;
    }
    let t = std::time::Instant::now();
    let split = op_ingest::herd_boundaries(app.ctx.db(), &herd, Utc::now()).await.unwrap();
    let room = sched::budget(&app.ctx, &herd, &split, &Default::default(), Utc::now()).await.unwrap();
    let took = t.elapsed();
    assert_eq!(room.collars, 250);
    assert!(took < std::time::Duration::from_millis(500), "budget over 250 collars took {took:?}");
    let t = std::time::Instant::now();
    sched::drive(&app.ctx, &s.id, Utc::now()).await.unwrap();
    assert!(t.elapsed() < std::time::Duration::from_secs(2), "a pass took {:?}", t.elapsed());
}

// ---- the ground the herd is on (FX-FENCE) ----

/// Staged versions of a schedule, as the collars get them (prepared).
async fn staged_shape(app: &App, herd: &str, version: u32) -> Polygon {
    let status = app.status(herd).await;
    let b = status["staged"].as_array().unwrap().iter().find(|b| b["version"].as_u64() == Some(u64::from(version))).expect("staged").clone();
    serde_json::from_value(b["geometry"].clone()).unwrap()
}

#[tokio::test]
async fn after_a_late_open_the_next_open_keeps_the_ground_the_herd_is_on() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    let mut dev = app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(5));
    let hourly = move |n: u32| start + Duration::minutes(60 * i64::from(n));
    let s = sched::create(&app.ctx, new_schedule(&herd, &pad, start, quick_fence()), &hourly).await.unwrap();
    // A lone boundary drops the staged moves; the server next looks 40 minutes after strip 2's time.
    op_ingest::send_boundary(&app.ctx, &herd, strips()[0].clone(), Default::default(), "dec_other").await.unwrap();
    sched::drive(&app.ctx, &s.id, start + Duration::minutes(40)).await.unwrap();
    let ms = app.moves(&s).await;
    assert!(ms.iter().filter(|m| m.index == 1).all(|m| m.skipped.as_deref() == Some("late")));
    // The herd still stands on strip 1: strip 3's open keeps it, and the back fence closes it behind them.
    let herd_at = m_at(25.0, 100.0);
    let open3 = ms.iter().find(|m| m.index == 2 && m.step == 0).unwrap();
    assert_eq!(open3.state, MoveState::Staged);
    assert!(open3.geometry.contains(herd_at), "strip 3's open leaves the herd on strip 1 outside");
    assert!(staged_shape(&app, &herd, open3.boundary_version.unwrap()).await.contains(herd_at), "as staged too");
    let closes: Vec<&ScheduledMove> = ms.iter().filter(|m| m.index == 2 && m.step > 0).collect();
    let last = closes.iter().max_by_key(|m| m.step).unwrap();
    assert!((last.geometry.area_ha() * 1e4 - 50.0 * 200.0).abs() < 100.0, "closes to strip 3 alone");
    // Two strips of old ground close at the back fence's pace: two steps each, all before strip 4 opens.
    assert_eq!(closes.len(), 4);
    let open4 = ms.iter().find(|m| m.index == 3 && m.step == 0).unwrap();
    assert!(closes.iter().all(|m| m.at > open3.at && m.at < open4.at));
    // The collar applies it on its own clock with the herd inside.
    app.sync(&mut dev, Utc::now()).await;
    dev.store.tick(open3.at + Duration::seconds(1)).expect("strip 3 opens");
    assert!(dev.store.active().unwrap().cmd.polygon().contains(herd_at));
}

#[tokio::test]
async fn back_fence_steps_missed_while_the_server_was_away_stay_in_the_next_open() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    // Two slots: one move staged ahead at a time.
    app.collar(&herd, limits(2, 0)).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(5));
    let hourly = move |n: u32| start + Duration::minutes(60 * i64::from(n));
    let s = sched::create(&app.ctx, new_schedule(&herd, &pad, start, quick_fence()), &hourly).await.unwrap();
    assert_eq!(staged(&app.moves(&s).await).len(), 1, "strip 2's open");
    // Strip 2 opened on the collars' clocks; the server was away through its back fence.
    sched::drive(&app.ctx, &s.id, start + Duration::minutes(40)).await.unwrap();
    let ms = app.moves(&s).await;
    let two: Vec<(u32, MoveState, Option<String>)> = ms.iter().filter(|m| m.index == 1).map(|m| (m.step, m.state, m.skipped.clone())).collect();
    assert_eq!(two[0].1, MoveState::Done);
    assert!(two[1..].iter().all(|m| m.2.as_deref() == Some("late")), "{two:?}");
    // The herd still has strips 1 and 2: strip 3's open keeps both.
    let open3 = ms.iter().find(|m| m.index == 2 && m.step == 0).unwrap();
    assert_eq!(open3.state, MoveState::Staged);
    for x in [25.0, 75.0, 125.0] {
        assert!(open3.geometry.contains(m_at(x, 100.0)), "x {x}");
    }
}

#[tokio::test]
async fn a_schedule_from_the_whole_paddock_keeps_it_at_the_first_open_and_closes_it_behind_the_herd() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    let dev = app.collar(&herd, CollarLimits::V0).await;
    let (st, m) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": rect(0.0, 0.0, 300.0, 200.0)}))).await;
    assert_eq!((st, m["status"].as_str()), (StatusCode::CREATED, Some("done")), "{m}");
    app.report(&dev, json!({"fixes": [fix(m_at(250.0, 100.0), Utc::now())]})).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    assert_eq!(s.next_index, 0);
    let ms = app.moves(&s).await;
    let open1 = ms.iter().find(|m| m.index == 0 && m.step == 0).unwrap();
    // Nobody is left outside at the open: it is the whole paddock still.
    for x in [25.0, 150.0, 250.0, 295.0] {
        assert!(open1.geometry.contains(m_at(x, 100.0)), "x {x}");
    }
    // The back fence closes the other five strips from the far side, two steps a strip, before strip 2 opens.
    let closes: Vec<&ScheduledMove> = ms.iter().filter(|m| m.index == 0 && m.step > 0).collect();
    assert_eq!(closes.len(), 10);
    let areas: Vec<f64> = closes.iter().map(|m| m.geometry.area_ha() * 1e4).collect();
    assert!(areas.windows(2).all(|w| w[1] < w[0]), "{areas:?}");
    assert!(!closes[0].geometry.contains(m_at(295.0, 100.0)) && closes[0].geometry.contains(m_at(25.0, 100.0)));
    assert!((areas[9] - 50.0 * 200.0).abs() < 100.0, "strip 1 alone");
    let open2 = ms.iter().find(|m| m.index == 1 && m.step == 0).unwrap();
    assert!(closes.iter().all(|m| m.at > open1.at && m.at < open2.at));
}

#[tokio::test]
async fn a_schedule_from_three_strips_keeps_them_at_the_first_open() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    let (st, _) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": rect(0.0, 0.0, 150.0, 200.0)}))).await;
    assert_eq!(st, StatusCode::CREATED);
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    assert_eq!(s.next_index, 3);
    let ms = app.moves(&s).await;
    let open4 = ms.iter().find(|m| m.index == 3 && m.step == 0).unwrap();
    assert!(open4.geometry.contains(m_at(25.0, 100.0)) && open4.geometry.contains(m_at(175.0, 100.0)));
    let last = ms.iter().filter(|m| m.index == 3).max_by_key(|m| m.step).unwrap();
    assert!((last.geometry.area_ha() * 1e4 - 50.0 * 200.0).abs() < 100.0, "strip 4 alone at the end");
}

#[tokio::test]
async fn move_now_drops_the_back_fence_left_from_the_strip_before() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::seconds(2));
    let bf = BackFence { enabled: true, lag_strips: 0, close_after_min: 5, close_steps: 1, close_every_min: 1 };
    let s = app.schedule(&herd, &pad, start, bf).await;
    tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
    sched::drive(&app.ctx, &s.id, Utc::now()).await.unwrap();
    assert_eq!(app.moves(&s).await.iter().find(|m| m.index == 1 && m.step == 0).unwrap().state, MoveState::Done);
    // Strip 2 is open (strips 1 and 2); its back fence is still to come. Move now opens strip 3.
    let s = sched::move_now(&app.ctx, &s.id).await.unwrap();
    let ms = app.moves(&s).await;
    let close2 = ms.iter().find(|m| m.index == 1 && m.step == 1).unwrap();
    assert_eq!(close2.state, MoveState::Skipped, "strip 2's back fence would cut strip 3 off");
    let open3 = ms.iter().find(|m| m.index == 2 && m.step == 0).unwrap();
    assert_eq!(open3.state, MoveState::Done);
    assert!(open3.geometry.contains(m_at(25.0, 100.0)), "the herd keeps strip 1 until its back fence closes it");
    // Nothing still to come fences the herd out of strip 3 before strip 4 opens.
    for m in ms.iter().filter(|m| matches!(m.state, MoveState::Planned | MoveState::Staged) && m.index <= 2) {
        assert!(m.geometry.contains(m_at(125.0, 100.0)), "strip {} step {}", m.index + 1, m.step);
    }
    let active: Polygon = serde_json::from_value(app.status(&herd).await["active"]["geometry"].clone()).unwrap();
    assert!(active.contains(m_at(25.0, 100.0)) && active.contains(m_at(125.0, 100.0)));
}

#[tokio::test]
async fn an_open_that_never_happened_takes_its_back_fence_steps_with_it() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(5));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    // Strip 2's open couldn't be sent (as when prepare refuses it), and a lone
    // boundary dropped what was staged.
    sqlx::query("UPDATE schedule_moves SET state = 'skipped', skipped = 'skipped', boundary_id = NULL, boundary_version = NULL WHERE schedule_id = ? AND strip = 1 AND step = 0")
        .bind(&s.id)
        .execute(app.ctx.db())
        .await
        .unwrap();
    op_ingest::send_boundary(&app.ctx, &herd, strips()[0].clone(), Default::default(), "dec_other").await.unwrap();
    sched::drive(&app.ctx, &s.id, Utc::now() + Duration::seconds(61)).await.unwrap();
    let ms = app.moves(&s).await;
    // Its back fence would have closed onto strip 2 with the herd on strip 1.
    assert!(
        ms.iter().filter(|m| m.index == 1).all(|m| m.state == MoveState::Skipped),
        "{:?}",
        ms.iter().map(|m| (m.index, m.step, m.state)).collect::<Vec<_>>()
    );
    let open3 = ms.iter().find(|m| m.index == 2 && m.step == 0).unwrap();
    assert!(open3.geometry.contains(m_at(25.0, 100.0)));
    assert!(ms.iter().filter(|m| m.state == MoveState::Staged).all(|m| m.geometry.contains(m_at(125.0, 100.0)) || m.index > 2));
}

#[tokio::test]
async fn a_move_off_every_mapped_paddock_ends_the_schedule() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    // The farmer draws the unmapped field next door: the decision names no paddock.
    let (st, m) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": rect(320.0, 0.0, 520.0, 200.0)}))).await;
    assert_eq!(st, StatusCode::CREATED, "{m}");
    let row = sqlx::query("SELECT * FROM decisions WHERE id = ?").bind(m["decision_id"].as_str().unwrap()).fetch_one(app.ctx.db()).await.unwrap();
    let d = op_core::store::decision_from_row(&row).unwrap();
    assert_eq!(d.to_paddock_id, None);
    // The draw itself ends it: nothing waits on the event bus.
    let s = sched::get(&app.ctx, &s.id).await.unwrap().unwrap();
    assert_eq!(s.status, ScheduleStatus::Done);
    assert!(app.moves(&s).await.iter().all(|m| m.state != MoveState::Staged && m.state != MoveState::Planned));
    // And the move ending restages nothing of the old paddock.
    app.call("POST", &format!("/api/herds/{herd}/move/stop"), None).await;
    sched::drive(&app.ctx, &s.id, Utc::now() + Duration::seconds(61)).await.unwrap();
    assert!(app.status(&herd).await["staged"].as_array().is_none_or(|a| a.is_empty()));
    let e = sched::on_decision(&app.ctx, &d).await.unwrap();
    assert!(e.is_none(), "already ended");
}

#[tokio::test]
async fn a_boundary_mostly_off_the_strips_ends_the_schedule_on_the_next_pass() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    app.collar(&herd, CollarLimits::V0).await;
    app.on_strip_one(&herd).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let s = app.schedule(&herd, &pad, start, quick_fence()).await;
    // A boundary the schedule never heard about: mostly the field next door.
    op_ingest::send_boundary(&app.ctx, &herd, rect(280.0, 0.0, 480.0, 200.0), Default::default(), "dec_elsewhere").await.unwrap();
    sched::drive(&app.ctx, &s.id, Utc::now() + Duration::seconds(61)).await.unwrap();
    let s = sched::get(&app.ctx, &s.id).await.unwrap().unwrap();
    assert_eq!(s.status, ScheduleStatus::Done, "the strips would drag the herd back across");
    assert!(app.moves(&s).await.iter().all(|m| m.state != MoveState::Staged && m.state != MoveState::Planned));
    // The collars drop anything of the schedule's still staged.
    assert!(app.status(&herd).await["staged"].as_array().is_none_or(|a| a.is_empty()));
}

#[tokio::test]
async fn a_schedule_over_part_of_the_paddock_starts_from_the_whole_of_it() {
    let app = App::new().await;
    let (herd, pad) = app.herd("Cows").await;
    let dev = app.collar(&herd, CollarLimits::V0).await;
    // The herd has the whole paddock; the farmer strip-grazes its west third.
    let (st, m) = app.call("POST", &format!("/api/herds/{herd}/boundary"), Some(json!({"geometry": rect(0.0, 0.0, 300.0, 200.0)}))).await;
    assert_eq!((st, m["status"].as_str()), (StatusCode::CREATED, Some("done")), "{m}");
    app.report(&dev, json!({"fixes": [fix(m_at(250.0, 100.0), Utc::now())]})).await;
    let start = whole(Utc::now() + Duration::minutes(30));
    let mut n = new_schedule(&herd, &pad, start, quick_fence());
    n.strips = strips()[..2].to_vec();
    let s = sched::create(&app.ctx, n, &every(start)).await.unwrap();
    let s = sched::get(&app.ctx, &s.id).await.unwrap().unwrap();
    assert_eq!(s.status, ScheduleStatus::Active, "the herd's paddock is where the strips are");
    let ms = app.moves(&s).await;
    let open1 = ms.iter().find(|m| m.index == 0 && m.step == 0).unwrap();
    assert_eq!(open1.state, MoveState::Staged);
    assert!(open1.geometry.contains(m_at(250.0, 100.0)), "nobody left outside at the open");
    sched::drive(&app.ctx, &s.id, Utc::now() + Duration::seconds(61)).await.unwrap();
    assert_eq!(sched::get(&app.ctx, &s.id).await.unwrap().unwrap().status, ScheduleStatus::Active);
}
