//! The bridge's server side against a real openpasture server: the collar's
//! console lines (a transcript in firmware 0.2's format, standing in for the
//! serial port) become a report; a boundary drawn in the app downloads,
//! verifies as the collar would verify it, and is acked; a config rides the
//! next reply; the server's wire log shows every exchange; and a capture
//! replays.

use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{SecondsFormat, Utc};
use collar_bridge::capture::{self, Capture, Record};
use collar_bridge::console::{Line, Reply, parse};
use collar_bridge::replay;
use collar_bridge::session::Session;
use collar_bridge::uplink::{Sent, Uplink};
use op_protocol::{AckStatus, PositionReport, WireFix};
use op_server::{ServeOptions, serve};
use serde_json::{Value, json};

const CENTRE: [f64; 2] = [-92.41, 38.12];

fn square(half: f64) -> Value {
    let [x, y] = CENTRE;
    json!({"type": "Polygon", "coordinates": [[[x - half, y - half], [x + half, y - half], [x + half, y + half], [x - half, y + half], [x - half, y - half]]]})
}

struct Api {
    http: reqwest::Client,
    base: String,
}

impl Api {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> (u16, Value) {
        let mut req = self.http.request(method, format!("{}{path}", self.base));
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.unwrap();
        let status = res.status().as_u16();
        let text = res.text().await.unwrap();
        (status, serde_json::from_str(&text).unwrap_or(Value::Null))
    }
    async fn post(&self, path: &str, body: Value) -> Value {
        let (s, v) = self.call(reqwest::Method::POST, path, Some(body)).await;
        assert!(s == 200 || s == 201, "{path}: {s} {v}");
        v
    }
    async fn get(&self, path: &str) -> Value {
        let (s, v) = self.call(reqwest::Method::GET, path, None).await;
        assert_eq!(s, 200, "{path}: {v}");
        v
    }
}

/// The collar's console over the last half minute, as firmware 0.2 prints it.
fn transcript(collar: &str, herd: &str) -> Vec<String> {
    let t0 = Utc::now() - chrono::Duration::seconds(30);
    let mut lines = vec![
        "[00:00:00.251,000] <inf> opencollar: OpenCollar 0.2.0 starting".to_owned(),
        format!("collar {collar} herd {herd} fw 0.2.0 config - active - slots -"),
    ];
    for i in 0..5 {
        let at = (t0 + chrono::Duration::seconds(i)).to_rfc3339_opts(SecondsFormat::Secs, true);
        lines.push(format!(
            "[00:01:{:02}.001,000] <inf> opencollar: fix {:.6},{:.6} acc=3.{i}m sats=8 (no boundary) at={at}",
            10 + i,
            CENTRE[1] + i as f64 * 1e-5,
            CENTRE[0]
        ));
    }
    lines
}

#[tokio::test]
async fn a_bench_collar_reports_takes_a_boundary_and_is_logged() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(ServeOptions { data_dir: Some(dir.path().join("data")), free_port: true, ..Default::default() }).await.unwrap();
    // The owner's client: the app token on every request.
    let mut auth = reqwest::header::HeaderMap::new();
    auth.insert("authorization", format!("Bearer {}", server.ctx().settings().await.unwrap().server.app_token).parse().unwrap());
    let api = Api { http: reqwest::Client::builder().default_headers(auth).build().unwrap(), base: server.url().to_owned() };

    api.post("/api/farm", json!({"name": "Bench", "center": CENTRE, "timezone": "America/Chicago"})).await;
    let paddock = api.post("/api/paddocks", json!({"name": "Bench paddock", "geometry": square(0.002)})).await;
    let herd = api.post("/api/herds", json!({"name": "Bench", "species": "cattle", "count": 1, "paddock_id": paddock["id"]})).await;
    let herd_id = herd["id"].as_str().unwrap();
    let linked = api.post("/api/collars", json!({"herd_id": herd_id, "name": "Bench collar"})).await;
    let collar_id = linked["collar"]["id"].as_str().unwrap();
    let key = linked["key"].as_str().unwrap();
    let server_key = op_protocol::decode_public_key(linked["public_key"].as_str().unwrap()).unwrap();

    let cap_path = dir.path().join("capture.jsonl");
    let capture = Arc::new(Capture::open(&cap_path).unwrap());
    let uplink = Uplink::new(server.url(), key, capture.clone());
    let mut s = Session::new();
    let now = Instant::now();
    for raw in transcript(collar_id, herd_id) {
        capture.serial(capture::Dir::In, &raw);
        let l = parse(&raw);
        if let Line::Reply(Reply::Status(st)) = &l.line {
            s.status = Some(st.clone());
        }
        s.on_line(&l, now);
    }

    // The report: five fixes with the collar's own times.
    let rep = s.take_report();
    assert_eq!(rep.fixes.len(), 5);
    let reply = match uplink.report(&rep).await {
        Sent::Ok(reply) => reply,
        other => panic!("report refused: {other:?}"),
    };
    // A collar with the config cap and no config gets one on the first reply.
    let (config_wire, config) = reply.config.expect("a config for a collar with the config cap");
    op_protocol::verify_config_wire(config_wire.as_bytes(), &server_key).unwrap();
    assert_eq!(config.collar_id, collar_id);
    s.config_accepted(config.clone());
    let c = api.get(&format!("/api/collars/{collar_id}")).await;
    assert_eq!((c["fw"].as_str(), c["caps"].as_array().map(Vec::len)), (Some("0.2.0"), Some(6)));
    assert_eq!(c["last_fix"]["at"].as_str().map(|t| t.starts_with(&rep.fixes[4].at.format("%Y-%m-%dT%H:%M:%S").to_string())), Some(true));

    // Nothing to download yet.
    assert!(matches!(uplink.boundary(0, Some(16)).await, Sent::Ok(None)));

    // A boundary drawn in the app reaches the collar exactly as signed.
    api.post(&format!("/api/herds/{herd_id}/boundary"), json!({"geometry": square(0.001)})).await;
    let Sent::Ok(Some(d)) = uplink.boundary(0, Some(16)).await else { panic!("no download") };
    assert!(!d.wire.contains('\n'));
    let cmd = op_protocol::verify_wire(d.wire.as_bytes(), &server_key).unwrap();
    assert_eq!((cmd.version, cmd.command_id.as_str()), (d.version, d.command_id.as_str()));

    // The console said `applied N`: the ack carries the collar's clock.
    s.delivered(d.version, &d.command_id);
    s.boundary_reply(d.version, AckStatus::Applied, None, Instant::now());
    let acks = s.take_acks(Instant::now());
    let Ok(owed) = &acks[0] else { panic!("{acks:?}") };
    assert!(!owed.host_time);
    assert!(matches!(uplink.ack(&owed.ack).await, Sent::Ok(())));
    let status = api.get(&format!("/api/herds/{herd_id}/boundary")).await;
    let acked = status["acks"].as_array().unwrap().iter().any(|a| a["collar_id"] == collar_id && a["status"] == "applied" && a["version"] == d.version);
    assert!(acked, "{status}");

    // A report the server refuses: its reason shows in the wire log.
    let bad =
        PositionReport { fixes: vec![WireFix { at: Utc::now(), point: [0.0, 95.0], accuracy_m: 3.0, ..WireFix::default() }], ..PositionReport::default() };
    let Sent::Refused { status: 400, message } = uplink.report(&bad).await else { panic!("a fix at 95° N was taken") };

    let wire = api.get(&format!("/api/collars/{collar_id}/wire?limit=64")).await;
    let entries = wire["entries"].as_array().unwrap();
    let routes: Vec<(&str, &str)> = entries.iter().map(|e| (e["dir"].as_str().unwrap(), e["route"].as_str().unwrap())).collect();
    for want in [("in", "report"), ("out", "report"), ("out", "config"), ("in", "boundary"), ("out", "boundary"), ("in", "ack"), ("out", "ack")] {
        assert!(routes.contains(&want), "{want:?} in {routes:?}");
    }
    assert_eq!(entries[0]["note"], "5 fixes, 0 cues, 0 episodes");
    assert!(entries.iter().any(|e| e["note"] == format!("v{} {}", d.version, d.command_id)));
    assert!(entries.iter().any(|e| e["dir"] == "out" && e["status"] == 400 && e["note"].as_str() == Some(message.as_str())));
    assert!(!wire.to_string().contains(key), "the collar key never shows");
    assert_eq!(api.get(&format!("/api/collars/{collar_id}/wire?limit=2")).await["entries"].as_array().unwrap().len(), 2);
    assert_eq!(api.call(reqwest::Method::GET, "/api/collars/col_nope/wire", None).await.0, 404);

    // The capture has every exchange, and replays.
    drop(uplink);
    let recs = capture::read(&cap_path).unwrap();
    let http = recs.iter().filter(|r| matches!(r, Record::Http { .. })).count();
    assert_eq!(http, 5);
    assert!(!std::fs::read_to_string(&cap_path).unwrap().contains(key));
    let token = server.ctx().settings().await.unwrap().server.app_token;
    let replay_to = |api_key: Option<String>, into_farm: Option<&str>| replay::Options {
        capture: cap_path.clone(),
        target: Some((server.url().to_owned(), key.to_owned())),
        api_key,
        into_farm: into_farm.map(str::to_owned),
        pace: false,
        out: Arc::new(Capture::none()),
    };
    // Review P1-10: this collar isn't simulated. Without a check, a replay
    // wrote old reports to it. Now nothing is sent unless the farm is named.
    let wire_before = api.get(&format!("/api/collars/{collar_id}/wire?limit=64")).await["entries"].as_array().unwrap().len();
    let e = replay::run(replay_to(None, None)).await.unwrap_err().to_string();
    assert!(e.contains("--api-key"), "{e}");
    let e = replay::run(replay_to(Some(token.clone()), None)).await.unwrap_err().to_string();
    assert!(e.contains("aren't simulated") && e.contains("Bench collar") && e.contains(r#"--into-farm "Bench""#), "{e}");
    let e = replay::run(replay_to(Some(token.clone()), Some("Other farm"))).await.unwrap_err().to_string();
    assert!(e.contains("not \"Other farm\""), "{e}");
    assert_eq!(
        api.get(&format!("/api/collars/{collar_id}/wire?limit=64")).await["entries"].as_array().unwrap().len(),
        wire_before,
        "nothing reached the server"
    );
    let sum = replay::run(replay_to(Some(token.clone()), Some("Bench"))).await.unwrap();
    // The first download found nothing then; now the boundary is there for a collar that says it has none.
    assert_eq!(sum, replay::Summary { requests: 5, same: 4, different: 1 });

    tokio::time::timeout(Duration::from_secs(10), server.shutdown()).await.unwrap().unwrap();
}

/// A server whose every collar is simulated takes a replay without naming
/// the farm (review P1-10's other half).
#[tokio::test]
async fn a_simulated_farm_takes_a_replay() {
    let dir = tempfile::tempdir().unwrap();
    let server = serve(ServeOptions { data_dir: Some(dir.path().join("data")), free_port: true, ..Default::default() }).await.unwrap();
    let token = server.ctx().settings().await.unwrap().server.app_token;
    let mut auth = reqwest::header::HeaderMap::new();
    auth.insert("authorization", format!("Bearer {token}").parse().unwrap());
    let api = Api { http: reqwest::Client::builder().default_headers(auth).build().unwrap(), base: server.url().to_owned() };
    api.post("/api/farm", json!({"name": "Sandbox", "center": CENTRE, "timezone": "America/Chicago"})).await;
    let herd = api.post("/api/herds", json!({"name": "Sim", "species": "cattle", "count": 1, "simulated": true})).await;
    api.post("/api/collars", json!({"herd_id": herd["id"], "name": "Sim 1", "simulated": true})).await;
    assert_eq!(replay::check_target(server.url(), Some(&token), None).await.unwrap(), replay::Target::Simulated { collars: 1 });
    tokio::time::timeout(Duration::from_secs(10), server.shutdown()).await.unwrap().unwrap();
}

/// The station's plan and run match the fleet API: the server judges what
/// the console could show, and fails the run on what it couldn't.
#[tokio::test]
async fn a_station_run_is_judged_by_the_fleet_api() {
    use collar_bridge::factory::{FirstFix, Observed, Station, evaluate, run_body};

    let dir = tempfile::tempdir().unwrap();
    let server = serve(ServeOptions { data_dir: Some(dir.path().join("data")), free_port: true, ..Default::default() }).await.unwrap();
    // The owner's client: the app token on every request.
    let mut auth = reqwest::header::HeaderMap::new();
    auth.insert("authorization", format!("Bearer {}", server.ctx().settings().await.unwrap().server.app_token).parse().unwrap());
    let api = Api { http: reqwest::Client::builder().default_headers(auth).build().unwrap(), base: server.url().to_owned() };
    let created = api.post("/api/fleet/stations", json!({"name": "bench-1"})).await;
    let station = Station::new(server.url(), created["token"].as_str().unwrap());

    let plan = station.plan("node").await.unwrap();
    assert!(plan.steps.iter().any(|s| s.name == "gnss_fix"));
    assert!(station.plan("trough").await.unwrap_err().to_string().contains("no test plan for trough"));

    // Firmware 0.2 prints no device key; this test registers the unit with
    // one of its own to reach the run route.
    let device_key = op_protocol::encode_public_key(&op_protocol::generate_signing_key().verifying_key());
    api.post(
        "/api/fleet/units",
        json!({"serial": "OC1-N-TEST01", "device_class": "node", "hw_rev": "N1.0", "fw_version": "0.2.0", "device_pubkey": device_key}),
    )
    .await;

    let seen = Observed {
        fw: Some("0.2.0".into()),
        gnss_started_ms: Some(420),
        first_fix: Some(FirstFix { uptime_ms: Some(31_870), lat: 38.12, lon: -92.41, accuracy_m: 3.2, sats: Some(9) }),
        waited: Duration::from_secs(31),
        ..Observed::default()
    };
    let steps = evaluate(&plan, &seen, Some("0.2.0"), None);
    let now = Utc::now();
    let run = run_body(&plan, Some("bench-1"), Some("fx-01"), now - chrono::Duration::seconds(40), now, &steps);
    let judged = station.post_run("OC1-N-TEST01", &run).await.unwrap();
    assert_eq!(judged["result"], "fail", "{judged}");
    let verdict = |name: &str| judged["steps"].as_array().unwrap().iter().find(|s| s["name"] == name).map(|s| s["verdict"].as_str().unwrap().to_owned());
    assert_eq!(verdict("fw_flash").as_deref(), Some("pass"));
    assert_eq!(verdict("gnss_fix").as_deref(), Some("pass"));
    assert_eq!(verdict("rail_3v3").as_deref(), Some("skip"));
    assert!(judged["failed"].as_array().unwrap().iter().any(|f| f == "rail_3v3"));

    tokio::time::timeout(Duration::from_secs(10), server.shutdown()).await.unwrap().unwrap();
}
