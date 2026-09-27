//! `collar-sim`: emulated OpenCollar devices for an openpasture server, until
//! real collars report. It links collars through the public app API like a
//! person would, then runs each one as a device over the real collar
//! endpoints with its own key: fix batches, signed boundary downloads, acks.
//!
//! `--caps v0` (the default) and `--caps v1` run firmware 0.2 (protocol v1):
//! a `device` block and the slot list in every report, `free`/`free_bytes`
//! on downloads, holes, cue kinds and episodes, and the signed config from
//! the report reply (report and poll cadence, fast mode until its GNSS time,
//! herd, endpoint). `--caps legacy` runs firmware 0.1. Staged boundaries
//! apply on each collar's own clock, and acks are kept until the server
//! takes them, so a collar cut off from the server behaves as in the field.

mod firmware;
mod herd;
mod sensors;

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use clap::Parser;
use firmware::{Firmware, Profile};
use op_geo::LonLat;
use op_geo::projection::Projection;
use op_protocol::{Health, PositionReport, VerifyingKey, WireCue, WireEpisode, WireFix};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use herd::{Animal, Area, Collar, Herd};

#[derive(Parser)]
#[command(name = "collar-sim", about = "Emulated collars for an openpasture server.")]
struct Args {
    /// The openpasture server.
    #[arg(long, default_value = "http://127.0.0.1:7878")]
    server: String,
    /// Herd id or name. Default: the first herd.
    #[arg(long)]
    herd: Option<String>,
    /// Collars to run. Collars linked by an earlier run are reused.
    #[arg(long, default_value_t = 12)]
    count: usize,
    /// App token, needed when the server is bound beyond localhost.
    #[arg(long)]
    token: Option<String>,
    /// Where collar keys are kept. Default: ~/.collar-sim/state.json
    #[arg(long)]
    state: Option<PathBuf>,
    /// Seconds between fixes.
    #[arg(long, default_value_t = 5)]
    fix_secs: u64,
    /// Fixes per report, until a config sets the cadence (legacy: always).
    #[arg(long, default_value_t = 2)]
    batch: u32,
    /// Firmware the collars run: v0 or v1 (0.2, protocol v1, with V0 or V1
    /// limits) or legacy (0.1: one ring of 64 corners, no config).
    #[arg(long, value_enum, default_value_t = Profile::V0)]
    caps: Profile,
    /// Collars have an IMU: reports carry seconds without moving and tilt.
    #[arg(long)]
    imu: bool,
    /// This many collars come off their animals within the first few
    /// minutes and lie where they fell.
    #[arg(long, default_value_t = 0)]
    drop: usize,
    /// Collars report the LTE-M cell they measure (signal, quality, band).
    #[arg(long)]
    cell: bool,
}

/// A collar this tool linked. Keys are only ever shown once, so they live here.
#[derive(Clone, Serialize, Deserialize)]
struct Linked {
    server: String,
    herd_id: String,
    collar_id: String,
    key: String,
    endpoint: String,
    public_key: String,
    tag: String,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    collars: Vec<Linked>,
}

fn state_path(args: &Args) -> PathBuf {
    args.state.clone().unwrap_or_else(|| dirs::home_dir().unwrap_or_default().join(".collar-sim").join("state.json"))
}

fn load_state(path: &PathBuf) -> anyhow::Result<State> {
    match std::fs::read_to_string(path) {
        Ok(s) => serde_json::from_str(&s).with_context(|| format!("reading {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(e) => Err(e.into()),
    }
}

fn save_state(path: &PathBuf, state: &State) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Collar keys: never readable by other users, not even briefly.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        opts.mode(0o600);
        // An older file may have been created with wider permissions.
        if path.exists() {
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let mut f = opts.open(path)?;
    std::io::Write::write_all(&mut f, &serde_json::to_vec_pretty(state)?)?;
    Ok(())
}

/// The app API, as a person's browser would use it.
struct Api {
    http: reqwest::Client,
    server: String,
    token: Option<String>,
}

impl Api {
    async fn call(&self, method: reqwest::Method, path: &str, body: Option<Value>) -> anyhow::Result<Value> {
        let mut req = self.http.request(method.clone(), format!("{}{path}", self.server));
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        if let Some(b) = body {
            req = req.json(&b);
        }
        let res = req.send().await.with_context(|| format!("{method} {path}"))?;
        let status = res.status();
        let v: Value = res.json().await.unwrap_or(Value::Null);
        if !status.is_success() {
            bail!("{method} {path}: {status} {}", v["error"].as_str().unwrap_or(""));
        }
        Ok(v)
    }
    async fn get(&self, path: &str) -> anyhow::Result<Value> {
        self.call(reqwest::Method::GET, path, None).await
    }
    async fn post(&self, path: &str, body: Value) -> anyhow::Result<Value> {
        self.call(reqwest::Method::POST, path, Some(body)).await
    }
}

fn polygon(geometry: &Value) -> Option<op_geo::Polygon> {
    serde_json::from_value::<op_geo::Polygon>(geometry.clone()).ok()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();
    let server = args.server.trim_end_matches('/').to_owned();
    let api = Api { http: reqwest::Client::builder().timeout(Duration::from_secs(10)).build()?, server: server.clone(), token: args.token.clone() };

    let st = api.get("/api/state").await.context("is the server running?")?;
    let farm_center: Option<LonLat> = serde_json::from_value(st["farm"]["center"].clone()).ok();
    let herds = st["herds"].as_array().cloned().unwrap_or_default();
    let herd = match &args.herd {
        Some(h) => herds.iter().find(|x| x["id"] == h.as_str() || x["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(h))),
        None => herds.first(),
    }
    .cloned()
    .context("no such herd; create a farm and a herd in the app first")?;
    let herd_id = herd["id"].as_str().unwrap_or_default().to_owned();
    println!("herd {} ({herd_id})", herd["name"].as_str().unwrap_or(""));

    // Where the animals start, fenced in until the first boundary: the herd's paddock, else
    // the active boundary, else a 150 m square at the farm centre.
    let paddock = st["paddocks"].as_array().and_then(|ps| ps.iter().find(|p| p["id"] == herd["paddock_id"])).and_then(|p| polygon(&p["geometry"]));
    let status = api.get(&format!("/api/herds/{herd_id}/boundary")).await?;
    let shape = paddock.or_else(|| polygon(&status["active"]["geometry"])).or_else(|| {
        farm_center.map(|c| {
            let p = Projection::new(c);
            op_geo::Polygon::from_ring(vec![p.offset(-75.0, -75.0), p.offset(75.0, -75.0), p.offset(75.0, 75.0), p.offset(-75.0, 75.0)])
        })
    });
    let area = shape.as_ref().and_then(Area::from_polygon).context("the herd has no paddock and the farm has no centre")?;

    // Reuse collars from earlier runs that still exist; link the rest.
    let path = state_path(&args);
    let mut state = load_state(&path)?;
    let existing: Vec<String> = api
        .get(&format!("/api/collars?herd_id={herd_id}"))
        .await?
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|c| c["id"].as_str().map(String::from))
        .collect();
    state.collars.retain(|c| c.server != server || c.herd_id != herd_id || existing.contains(&c.collar_id));
    let mut mine: Vec<Linked> = state.collars.iter().filter(|c| c.server == server && c.herd_id == herd_id).cloned().collect();
    if mine.len() < args.count {
        let animals = api.get(&format!("/api/animals?herd_id={herd_id}")).await?;
        let mut tag = animals.as_array().into_iter().flatten().filter_map(|a| a["tag"].as_str()?.parse::<u32>().ok()).max().map_or(101, |t| t.max(100) + 1);
        while mine.len() < args.count {
            let linked = api.post("/api/collars", json!({ "name": tag.to_string(), "herd_id": herd_id })).await?;
            let collar_id = linked["collar"]["id"].as_str().context("collar id")?.to_owned();
            api.post("/api/animals", json!({ "tag": tag.to_string(), "herd_id": herd_id, "collar_id": collar_id })).await?;
            let l = Linked {
                server: server.clone(),
                herd_id: herd_id.clone(),
                collar_id,
                key: linked["key"].as_str().context("key")?.to_owned(),
                endpoint: linked["endpoint"].as_str().context("endpoint")?.to_owned(),
                public_key: linked["public_key"].as_str().context("public_key")?.to_owned(),
                tag: tag.to_string(),
            };
            println!("linked collar {} on animal {tag}", l.collar_id);
            state.collars.push(l.clone());
            save_state(&path, &state)?;
            mine.push(l);
            tag += 1;
        }
    }
    mine.truncate(args.count);

    // Animals resume at their last reported position.
    let last: HashMap<String, LonLat> = api
        .get(&format!("/api/positions?herd_id={herd_id}"))
        .await?
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| Some((p["collar_id"].as_str()?.to_owned(), serde_json::from_value(p["fix"]["point"].clone()).ok()?)))
        .collect();

    let world: World = Arc::new(Mutex::new(HashMap::new()));
    println!("running {} {:?} collars, a fix every {} s; ctrl-c to stop", mine.len(), args.caps, args.fix_secs);
    let radio = args.cell.then(|| sensors::Radio::near(area.interior()));
    let mut tasks = Vec::new();
    for (i, l) in mine.into_iter().enumerate() {
        let start = last.get(&l.collar_id).copied();
        let cfg = Run { fix_secs: args.fix_secs.max(1), batch: args.batch.max(1), profile: args.caps, imu: args.imu, drop: i < args.drop, radio };
        tasks.push(tokio::spawn(run_collar(l, start, area.clone(), world.clone(), api.http.clone(), cfg)));
    }
    tokio::select! {
        _ = tokio::signal::ctrl_c() => println!("stopping"),
        _ = futures_all(tasks) => println!("every collar has stopped"),
    }
    Ok(())
}

async fn futures_all(tasks: Vec<tokio::task::JoinHandle<()>>) {
    for t in tasks {
        let _ = t.await;
    }
}

/// True positions and velocities of the herd, so each animal can keep near
/// and follow the others.
type World = Arc<Mutex<HashMap<String, (LonLat, [f64; 2])>>>;

fn herd_view(world: &World, except: &str) -> Option<Herd> {
    let w = world.lock().unwrap_or_else(|e| e.into_inner());
    Herd::of(w.iter().filter(|(k, _)| k.as_str() != except).map(|(_, v)| *v))
}

#[derive(Clone, Copy)]
struct Run {
    fix_secs: u64,
    batch: u32,
    profile: Profile,
    imu: bool,
    /// This collar comes off its animal.
    drop: bool,
    radio: Option<sensors::Radio>,
}

/// The device side of the protocol for one collar.
struct Device {
    http: reqwest::Client,
    l: Linked,
    server_key: VerifyingKey,
}

enum Outcome {
    Ok,
    /// The key was refused: the collar was removed in the app.
    Unlinked,
    Failed,
}

impl Device {
    fn url(&self, fw: &Firmware, path: &str) -> String {
        format!("{}{path}", fw.endpoint.current.trim_end_matches('/'))
    }

    fn outcome(&self, what: &str, res: reqwest::Result<reqwest::Response>) -> (Outcome, Option<reqwest::Response>) {
        match res {
            Ok(r) if r.status() == reqwest::StatusCode::UNAUTHORIZED => (Outcome::Unlinked, None),
            Ok(r) if r.status().is_success() => (Outcome::Ok, Some(r)),
            Ok(r) => {
                eprintln!("{}: {what} refused: {}", self.l.tag, r.status());
                // The server will never take it; don't resend.
                (if r.status().is_client_error() { Outcome::Ok } else { Outcome::Failed }, None)
            }
            Err(_) => (Outcome::Failed, None),
        }
    }

    /// Send a report; the reply (latest version, maybe a config) when it got through.
    async fn report(&self, fw: &Firmware, rep: &PositionReport) -> (Outcome, Option<Value>) {
        let res = self.http.post(self.url(fw, "/report")).bearer_auth(&self.l.key).json(rep).send().await;
        match self.outcome("report", res) {
            (Outcome::Ok, Some(r)) => (Outcome::Ok, Some(r.json::<Value>().await.unwrap_or(Value::Null))),
            (o, _) => (o, None),
        }
    }

    /// Deliver owed acks, oldest first, until one doesn't get through.
    async fn deliver_acks(&self, fw: &mut Firmware) -> Outcome {
        while let Some(a) = fw.pending_acks().first().cloned() {
            let res = self.http.post(self.url(fw, "/ack")).bearer_auth(&self.l.key).json(&a).send().await;
            match self.outcome("ack", res).0 {
                Outcome::Ok => fw.ack_done(),
                o => return o,
            }
        }
        Outcome::Ok
    }

    /// Download, verify and offer every boundary past what the collar holds.
    async fn sync(&self, collar: &mut Collar, now: DateTime<Utc>) -> Outcome {
        for _ in 0..4 {
            let url = self.url(&collar.firmware, &format!("/boundary?{}", collar.firmware.boundary_query()));
            let res = self.http.get(url).bearer_auth(&self.l.key).send().await;
            let r = match self.outcome("boundary", res) {
                (Outcome::Ok, Some(r)) if r.status() == reqwest::StatusCode::OK => r,
                (Outcome::Ok, _) => return Outcome::Ok,
                (o, _) => return o,
            };
            let Ok(bytes) = r.bytes().await else { return Outcome::Failed };
            let Some(a) = collar.offer(&bytes, &self.server_key, now) else {
                eprintln!("{}: ignored a download that names no boundary", self.l.tag);
                return Outcome::Ok;
            };
            match a.code {
                Some(code) => println!("{}: boundary v{} {} ({})", self.l.tag, a.version, a.status.as_str(), code.as_str()),
                None => println!("{}: boundary v{} {}", self.l.tag, a.version, a.status.as_str()),
            }
            if let o @ (Outcome::Unlinked | Outcome::Failed) = self.deliver_acks(&mut collar.firmware).await {
                return o;
            }
        }
        Outcome::Ok
    }
}

/// Keep the newest `max` entries of a batch the server hasn't taken yet.
fn cap<T>(v: &mut Vec<T>, max: usize) {
    let over = v.len().saturating_sub(max);
    v.drain(..over);
}

async fn run_collar(l: Linked, start: Option<LonLat>, area: Area, world: World, http: reqwest::Client, cfg: Run) {
    let mut rng = StdRng::from_entropy();
    let Ok(server_key) = op_protocol::decode_public_key(&l.public_key) else {
        eprintln!("{}: bad server public key in the state file", l.tag);
        return;
    };
    let pos = start.unwrap_or_else(|| {
        let p = Projection::new(area.interior()).offset(12.0 * herd::normal(&mut rng), 12.0 * herd::normal(&mut rng));
        if area.margin(p) > 0.0 { p } else { area.interior() }
    });
    let id = l.collar_id.clone();
    let firmware = Firmware::new(cfg.profile, &l.herd_id, &l.collar_id, &l.endpoint);
    let dev = Device { http, l, server_key };
    let stubborn = rng.gen_bool(0.2);
    let mut collar = Collar::new(Animal::new(pos, stubborn, &mut rng), rng.gen_range(0.82..1.0), Some(area), firmware);
    if cfg.imu {
        collar.motion = Some(sensors::Motion::new(chrono::Utc::now()));
    }
    if cfg.drop {
        collar.fall = Some(sensors::Fall::at(chrono::Utc::now() + chrono::Duration::seconds(rng.gen_range(60..180))));
    }
    world.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), (pos, [0.0, 0.0]));

    // Stagger so collars don't report in lockstep.
    tokio::time::sleep(Duration::from_millis(rng.gen_range(0..cfg.fix_secs * 1000))).await;
    let booted = chrono::Utc::now();
    let mut tick = tokio::time::interval(Duration::from_secs(cfg.fix_secs));
    let (mut fixes, mut cues, mut episodes): (Vec<WireFix>, Vec<WireCue>, Vec<WireEpisode>) = (Vec::new(), Vec::new(), Vec::new());
    // Before a config: a report every `batch` fixes and a poll after it.
    let every = (cfg.fix_secs * u64::from(cfg.batch)) as i64;
    let (mut last_report, mut last_poll): (Option<DateTime<Utc>>, Option<DateTime<Utc>>) = (None, None);
    let mut shown: Option<(i64, i64)> = None;
    'run: loop {
        tick.tick().await;
        // Receivers time fixes to the second; this is the collar's GNSS clock.
        let now = op_protocol::wire_time::trunc_secs(chrono::Utc::now());
        let herd = herd_view(&world, &id);
        let held = collar.held_version();
        let fallen = collar.fallen();
        let (fix, cue) = collar.tick(now, cfg.fix_secs as f64, herd, &mut rng);
        if !fallen && collar.fallen() {
            let p = collar.position();
            println!("{}: collar fell off its animal at [{:.6}, {:.6}]", dev.l.tag, p[0], p[1]);
        }
        if collar.held_version() != held {
            println!("{}: boundary v{} applied", dev.l.tag, collar.held_version().unwrap_or(0));
        }
        world.lock().unwrap_or_else(|e| e.into_inner()).insert(id.clone(), (collar.animal.pos, collar.animal.vel));
        fixes.extend(fix);
        cues.extend(cue);
        episodes.extend(collar.take_episodes());

        let (report_s, poll_s) = collar.firmware.cadence(now).map_or((every, every), |(r, p)| (i64::from(r), i64::from(p)));
        if shown != Some((report_s, poll_s)) {
            let fast = collar.firmware.config().and_then(|c| c.fast_until.filter(|t| *t > now).map(|t| format!(" until {}", t.format("%H:%M:%S"))));
            println!("{}: reporting every {report_s} s, polling every {poll_s} s{}", dev.l.tag, fast.unwrap_or_default());
            shown = Some((report_s, poll_s));
        }
        let due = |last: Option<DateTime<Utc>>, every: i64| last.is_none_or(|t| (now - t).num_seconds() >= every);
        let mut poll = due(last_poll, poll_s) && last_report.is_some();
        if due(last_report, report_s) {
            if let Outcome::Unlinked = dev.deliver_acks(&mut collar.firmware).await {
                break 'run;
            }
            let version = collar.held_version();
            let legacy = collar.firmware.profile.is_legacy();
            let device = collar.firmware.device();
            let rep = PositionReport {
                collar_id: Some(id.clone()),
                boundary_version: version,
                device: device.clone(),
                slots: collar.firmware.slots(),
                // A fix's own version only when it differs from the report's.
                fixes: fixes
                    .iter()
                    .cloned()
                    .map(|mut f| {
                        if f.boundary_version == version {
                            f.boundary_version = None;
                        }
                        f
                    })
                    .collect(),
                cues: cues.clone(),
                episodes: episodes.clone(),
                battery: Some((collar.battery * 1000.0).round() / 1000.0),
                health: (!legacy).then(|| {
                    let imu = collar.imu(now);
                    Health {
                        fix_attempts: Some(collar.fix_attempts),
                        fix_ok: Some(collar.fix_ok),
                        uptime_s: Some((now - booted).num_seconds().max(0) as u64),
                        still_s: imu.map(|m| m.0),
                        tilt_deg: imu.map(|m| m.1),
                        cell: cfg.radio.map(|r| r.measure(collar.position(), now, &mut rng)),
                        ..Default::default()
                    }
                }),
            };
            last_report = Some(now);
            match dev.report(&collar.firmware, &rep).await {
                (Outcome::Ok, reply) => {
                    fixes.clear();
                    cues.clear();
                    episodes.clear();
                    (collar.fix_attempts, collar.fix_ok) = (0, 0);
                    collar.firmware.endpoint.succeeded();
                    collar.firmware.reported(device.as_ref());
                    let reply = reply.unwrap_or(Value::Null);
                    if let Some(c) = reply.get("config").filter(|c| !c.is_null()) {
                        match collar.firmware.apply_config(c, &dev.server_key, now) {
                            Ok(c) => println!(
                                "{}: config v{} herd {} report {} s, fast {} s{}",
                                dev.l.tag,
                                c.version,
                                c.herd_id.as_deref().unwrap_or("-"),
                                c.report_s,
                                c.fast_report_s.map_or("-".into(), |f| f.to_string()),
                                c.fast_until.map_or(String::new(), |t| format!(" until {}", t.format("%H:%M:%S")))
                            ),
                            Err(code) => println!("{}: config refused ({})", dev.l.tag, code.as_str()),
                        }
                    }
                    let latest = reply.get("latest_version").and_then(Value::as_u64).unwrap_or(0);
                    poll |= latest > u64::from(collar.firmware.have());
                }
                (Outcome::Unlinked, _) => break 'run,
                // Keep the batch, like a collar out of coverage; cap it.
                (Outcome::Failed, _) => {
                    cap(&mut fixes, 2000);
                    cap(&mut cues, 2000);
                    cap(&mut episodes, 500);
                    if collar.firmware.endpoint.failed(now) {
                        println!("{}: back to {} after a day without the new endpoint", dev.l.tag, collar.firmware.endpoint.current);
                    }
                    poll = false;
                }
            }
        }
        if poll {
            last_poll = Some(now);
            if let Outcome::Unlinked = dev.sync(&mut collar, now).await {
                break 'run;
            }
        }
    }
    println!("{}: collar {id} was removed in the app; stopping it", dev.l.tag);
    world.lock().unwrap_or_else(|e| e.into_inner()).remove(&id);
}
