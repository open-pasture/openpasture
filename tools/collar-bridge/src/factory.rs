//! `collar-bridge factory`: the station beside the test fixture, one unit
//! after another (`docs/api/fleet.md`).
//!
//! For each unit: scan its serial, plug it in, run the plan for its class
//! over the serial console, register it, post the run (the server gives the
//! verdict). A step this station can't test is sent as `skip` with the
//! reason, never as a pass; the server fails a run whose required steps
//! were skipped.
//!
//! What firmware 0.2's console can show today: which version runs
//! (`fw_flash`, checked against `--fw`) and GNSS (`gnss_fix`, and
//! `gnss_ttff` when the station saw it boot). Readings from the fixture's
//! instruments or the operator's meter can be typed in with `--operator`.
//!
//! Every unit ends in one [`Outcome`], shown on its screen and in the exit
//! code. Today no unit can be finished here: firmware 0.2 prints no device
//! key, so the server can't register it ([`Outcome::NoDeviceKey`]), and no
//! firmware has a command to store a claim code, so a passing unit isn't
//! provisioned ([`Outcome::NotProvisioned`]): the server would otherwise
//! say "provisioned" for a unit that doesn't hold its claim code.

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::capture::Capture;
use crate::collar::{Collar, say};
use crate::console::{Line, Reply};
use crate::uplink::error_text;

pub struct Options {
    pub server: String,
    pub station_token: String,
    pub port: PathBuf,
    pub device_class: String,
    pub hw_rev: String,
    pub station: Option<String>,
    pub fixture_id: Option<String>,
    /// One unit with this serial, then stop. Without it, serials are asked for.
    pub serial: Option<String>,
    pub gnss_timeout: Duration,
    /// The firmware version the fixture flashes; `fw_flash` passes only when
    /// the unit runs it.
    pub expect_fw: Option<String>,
    /// Ask the operator for readings and checks the console can't give.
    pub operator: bool,
    /// Every run, appended as JSON lines whatever the server says.
    pub results: PathBuf,
    pub capture: Arc<Capture>,
    pub verbose: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TestPlan {
    pub id: String,
    pub version: u32,
    pub steps: Vec<PlanStep>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct PlanStep {
    pub name: String,
    pub kind: String,
    pub limit_lo: Option<f64>,
    pub limit_hi: Option<f64>,
    pub unit: Option<String>,
    #[serde(default)]
    pub required: bool,
}

/// One step as the station sends it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepResult {
    pub name: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl StepResult {
    fn skip(name: &str, why: impl Into<String>) -> Self {
        Self { name: name.into(), status: "skip".into(), value: None, unit: None, detail: Some(why.into()) }
    }
}

/// What the console showed during the run.
#[derive(Debug, Clone, Default)]
pub struct Observed {
    pub fw: Option<String>,
    /// The unit's Ed25519 public key, base64, when its firmware prints one.
    pub device_pubkey: Option<String>,
    /// Collar uptime when GNSS started (only when the station saw it boot).
    pub gnss_started_ms: Option<u64>,
    pub first_fix: Option<FirstFix>,
    /// How long the station waited for a fix.
    pub waited: Duration,
}

#[derive(Debug, Clone)]
pub struct FirstFix {
    pub uptime_ms: Option<u64>,
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: f64,
    pub sats: Option<u32>,
}

/// The operator's answer for a step: a reading, yes/no, or nothing.
pub trait Operator {
    fn ask(&mut self, step: &PlanStep) -> Option<Value>;
}

/// Asks on the terminal.
pub struct Terminal;

impl Operator for Terminal {
    fn ask(&mut self, step: &PlanStep) -> Option<Value> {
        let limits = match (step.limit_lo, step.limit_hi) {
            (Some(lo), Some(hi)) => format!(" {lo}–{hi}"),
            (Some(lo), None) => format!(" ≥ {lo}"),
            (None, Some(hi)) => format!(" ≤ {hi}"),
            (None, None) => String::new(),
        };
        let unit = step.unit.as_deref().map(|u| format!(" {u}")).unwrap_or_default();
        let prompt = match step.kind.as_str() {
            "check" => format!("  {} ok? [y/n, blank to skip] ", step.name),
            _ => format!("  {} ({}{limits}{unit}), blank to skip: ", step.name, step.kind),
        };
        let answer = read_line(&prompt)?;
        let answer = answer.trim();
        match step.kind.as_str() {
            _ if answer.is_empty() => None,
            "check" => Some(Value::Bool(matches!(answer, "y" | "Y" | "yes"))),
            "measure" => answer.parse::<f64>().ok().map(Value::from),
            _ => Some(Value::String(answer.into())),
        }
    }
}

fn read_line(prompt: &str) -> Option<String> {
    print!("{prompt}");
    std::io::stdout().flush().ok()?;
    let mut s = String::new();
    std::io::stdin().read_line(&mut s).ok()?;
    Some(s)
}

/// A firmware version as the console prints it: `major.minor.patch`, an
/// optional `-suffix`.
fn is_version(v: &str) -> bool {
    let core = v.split_once('-').map_or(v, |(c, _)| c);
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3 && parts.iter().all(|p| !p.is_empty() && p.len() <= 5 && p.bytes().all(|b| b.is_ascii_digit())) && v.len() <= 32
}

/// Worst accuracy a factory fix may claim, in metres.
pub const MAX_FIX_ACCURACY_M: f64 = 50.0;
/// Fewest satellites a fix may claim, when it says.
pub const MIN_FIX_SATS: u32 = 4;

/// Why a first fix isn't a fix, if it isn't.
fn bad_fix(f: &FirstFix) -> Option<String> {
    if !(f.lat.is_finite() && f.lon.is_finite() && f.accuracy_m.is_finite()) {
        return Some("the fix has a non-number in it".into());
    }
    if f.lat.abs() > 90.0 || f.lon.abs() > 180.0 {
        return Some(format!("{:.6},{:.6} isn't on Earth", f.lat, f.lon));
    }
    if f.lat == 0.0 && f.lon == 0.0 {
        return Some("0,0: the receiver's empty position, not a fix".into());
    }
    if !(f.accuracy_m > 0.0 && f.accuracy_m <= MAX_FIX_ACCURACY_M) {
        return Some(format!("accuracy {:.1} m (needs 0–{MAX_FIX_ACCURACY_M:.0} m)", f.accuracy_m));
    }
    if let Some(n) = f.sats.filter(|&n| n < MIN_FIX_SATS) {
        return Some(format!("{n} satellites (needs {MIN_FIX_SATS})"));
    }
    None
}

/// Each plan step, from what the console showed and what the operator said.
pub fn evaluate(plan: &TestPlan, seen: &Observed, expect_fw: Option<&str>, mut operator: Option<&mut dyn Operator>) -> Vec<StepResult> {
    let mut out = Vec::with_capacity(plan.steps.len());
    for step in &plan.steps {
        let name = step.name.as_str();
        let r = match name {
            "fw_flash" => match (&seen.fw, expect_fw) {
                (None, _) => fail(name, "nothing answered `status` on the console"),
                (Some(fw), _) if !is_version(fw) => fail(name, format!("the console answered {fw:?}, which isn't a firmware version")),
                (Some(fw), None) => StepResult::skip(name, format!("runs {fw}, but the station wasn't told which version it flashes (--fw)")),
                (Some(fw), Some(want)) if fw != want => fail(name, format!("runs {fw}, the fixture flashes {want}")),
                (Some(fw), Some(_)) if step.kind == "record" => StepResult { value: Some(json!(fw)), ..pass(name, format!("runs {fw}, the version flashed")) },
                (Some(fw), Some(_)) => pass(name, format!("runs {fw}, the version flashed")),
            },
            "gnss_fix" => match &seen.first_fix {
                Some(f) => match bad_fix(f) {
                    Some(why) => fail(name, why),
                    None => pass(name, format!("{:.6},{:.6} ±{:.1} m, {} sats", f.lat, f.lon, f.accuracy_m, f.sats.map_or("?".into(), |s| s.to_string()))),
                },
                None => fail(name, format!("no fix in {} s", seen.waited.as_secs())),
            },
            "gnss_ttff" => match (&seen.first_fix, seen.gnss_started_ms) {
                (None, _) => fail(name, format!("no fix in {} s", seen.waited.as_secs())),
                (Some(f), _) if bad_fix(f).is_some() => fail(name, "no valid fix to time"),
                (Some(FirstFix { uptime_ms: Some(fix), .. }), Some(start)) if *fix <= start => {
                    fail(name, format!("the fix (uptime {fix} ms) came before GNSS started ({start} ms): the collar's clock went back, so it rebooted"))
                }
                (Some(FirstFix { uptime_ms: Some(fix), .. }), Some(start)) => StepResult {
                    name: name.into(),
                    status: "pass".into(),
                    value: Some(json!(((fix - start) as f64 / 100.0).round() / 10.0)),
                    unit: Some("s".into()),
                    detail: Some("GNSS start to first fix, on the collar's clock".into()),
                },
                _ => StepResult::skip(name, "the station didn't see the collar boot: power it with the station listening to time its first fix"),
            },
            _ => match operator.as_deref_mut().and_then(|o| o.ask(step)) {
                Some(Value::Bool(ok)) => {
                    if ok {
                        pass(name, "operator")
                    } else {
                        fail(name, "operator")
                    }
                }
                Some(v) => {
                    StepResult { name: name.into(), status: "pass".into(), value: Some(v), unit: step.unit.clone(), detail: Some("operator reading".into()) }
                }
                None => StepResult::skip(name, why_not(name)),
            },
        };
        out.push(r);
    }
    out
}

fn pass(name: &str, detail: impl Into<String>) -> StepResult {
    StepResult { name: name.into(), status: "pass".into(), value: None, unit: None, detail: Some(detail.into()) }
}

fn fail(name: &str, detail: impl Into<String>) -> StepResult {
    StepResult { name: name.into(), status: "fail".into(), value: None, unit: None, detail: Some(detail.into()) }
}

/// Why a step couldn't be tested here.
fn why_not(name: &str) -> String {
    let instrument = ["rail_", "sleep_current", "charger", "fuel_gauge"];
    let firmware = ["gnss_cn0", "flash_ext", "imu", "hall_wake", "buzzer", "lora_", "ble", "lte_", "sim_iccid"];
    if instrument.iter().any(|p| name.starts_with(p)) {
        "measured by the fixture's instruments; none are wired to this station (type readings with --operator)".into()
    } else if firmware.iter().any(|p| name.starts_with(p)) {
        "the collar's firmware has no console command for this yet".into()
    } else {
        format!("this station doesn't know how to test {name}")
    }
}

/// Watch the console until the first valid fix or the timeout.
async fn watch(collar: &mut Collar, timeout: Duration, seen: &mut Observed) {
    let started = Instant::now();
    while started.elapsed() < timeout && collar.gone().is_none() {
        let Some(l) = collar.next(Duration::from_millis(500)).await else { continue };
        match l.line {
            Line::Log(msg) if msg == "GNSS started" => seen.gnss_started_ms = l.uptime_ms,
            // A reboot starts a new clock: what it timed before doesn't count.
            Line::Boot { fw } => {
                seen.fw = Some(fw);
                seen.gnss_started_ms = None;
            }
            Line::Fix(f) => {
                seen.first_fix = Some(FirstFix { uptime_ms: l.uptime_ms, lat: f.lat, lon: f.lon, accuracy_m: f.accuracy_m, sats: f.sats });
                break;
            }
            _ => {}
        }
    }
    seen.waited = started.elapsed();
}

/// The fleet API as a station token sees it.
pub struct Station {
    http: reqwest::Client,
    base: String,
    token: String,
}

impl Station {
    pub fn new(server: &str, token: &str) -> Self {
        Self { http: reqwest::Client::new(), base: server.trim_end_matches('/').to_owned(), token: token.to_owned() }
    }

    /// The current plan for a device class.
    pub async fn plan(&self, device_class: &str) -> anyhow::Result<TestPlan> {
        let (status, plan) = self.call(reqwest::Method::GET, &format!("/api/fleet/test-plans/{device_class}"), None).await?;
        if status != 200 {
            bail!("no test plan for {device_class} ({status}): {}", err_of(&plan));
        }
        serde_json::from_value(plan).context("the test plan")
    }

    /// Post a run; the server's `TestRun` with its verdict.
    pub async fn post_run(&self, serial: &str, run: &Value) -> anyhow::Result<Value> {
        match self.call(reqwest::Method::POST, &format!("/api/fleet/units/{serial}/tests"), Some(run)).await? {
            (200 | 201, v) => Ok(v),
            (status, v) => bail!("results not taken ({status}): {}", err_of(&v)),
        }
    }

    async fn call(&self, method: reqwest::Method, path: &str, body: Option<&Value>) -> anyhow::Result<(u16, Value)> {
        let mut req = self.http.request(method, format!("{}{path}", self.base)).bearer_auth(&self.token);
        if let Some(b) = body {
            req = req.json(b);
        }
        let res = req.send().await.with_context(|| format!("{} unreachable", self.base))?;
        let status = res.status().as_u16();
        let text = res.text().await.unwrap_or_default();
        let v = serde_json::from_str(&text).unwrap_or(Value::String(text));
        Ok((status, v))
    }
}

fn err_of(v: &Value) -> String {
    match v {
        Value::String(s) => error_text(s),
        _ => v.get("error").and_then(Value::as_str).map_or_else(|| v.to_string(), str::to_owned),
    }
}

/// How a unit left the station. None of these is "finished": see the
/// module docs for why no unit can be, with today's firmware.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Nothing answered on the console.
    NoAnswer,
    /// The firmware prints no device key, so the server can't register the
    /// unit. The steps ran; the run is in `runs.jsonl` only.
    NoDeviceKey { fw: String },
    /// The server didn't take the unit or its run (the reason).
    NotRecorded(String),
    /// The server judged the run a failure.
    Failed,
    /// The server judged it a pass; it isn't provisioned (the reason).
    NotProvisioned(String),
}

impl Outcome {
    /// The screen's one word.
    pub fn word(&self) -> &'static str {
        match self {
            Outcome::NoAnswer => "NO ANSWER",
            Outcome::NoDeviceKey { .. } => "CAN'T REGISTER",
            Outcome::NotRecorded(_) => "NOT RECORDED",
            Outcome::Failed => "FAIL",
            Outcome::NotProvisioned(_) => "PASS, NOT PROVISIONED",
        }
    }

    /// What to do next, in a line.
    pub fn next_step(&self) -> String {
        match self {
            Outcome::NoAnswer => "check power and the cable; nothing was sent to the server".into(),
            Outcome::NoDeviceKey { fw } => {
                format!("can't register: firmware {fw} doesn't print a device key yet. Nothing is on the server; the run is in runs.jsonl")
            }
            Outcome::NotRecorded(why) => format!("the server has no record of this run: {why}. Fix that and test the unit again"),
            Outcome::Failed => "set the unit aside for rework; the failed steps are above".into(),
            Outcome::NotProvisioned(why) => format!("tested on the server, not provisioned: {why}"),
        }
    }

    /// The process exit code: 1 for a failure or no answer, 2 when the
    /// server didn't record it, 3 when the firmware can't go further yet.
    pub fn code(&self) -> i32 {
        match self {
            Outcome::NoAnswer | Outcome::Failed => 1,
            Outcome::NotRecorded(_) => 2,
            Outcome::NoDeviceKey { .. } | Outcome::NotProvisioned(_) => 3,
        }
    }
}

/// Why a passing unit isn't provisioned here: the server's claim code has
/// to be written to the unit, and no firmware has a command for it yet.
pub const NO_CLAIM_COMMAND: &str = "its firmware has no command to store a claim code, so no claim code was drawn and no label printed";

/// Units one after another (or the one `--serial`). The worst exit code of
/// the session (see [`Outcome::code`]); 0 only when nothing was tested.
pub async fn run(opts: Options) -> anyhow::Result<i32> {
    let station = Station::new(&opts.server, &opts.station_token);
    let plan = station.plan(&opts.device_class).await?;
    say(format!("plan {} v{} for {}: {} steps", plan.id, plan.version, opts.device_class, plan.steps.len()));
    if opts.expect_fw.is_none() {
        say("no --fw given: fw_flash will be skipped, which fails every run");
    }
    let mut tally: Vec<Outcome> = Vec::new();
    loop {
        let serial = match &opts.serial {
            Some(s) => s.clone(),
            None => match read_line("\nScan or type the serial (blank to stop): ").map(|s| s.trim().to_owned()) {
                Some(s) if !s.is_empty() => s,
                _ => break,
            },
        };
        let (collar, outcome) = unit(&opts, &station, &plan, &serial).await;
        tally.push(outcome);
        if opts.serial.is_some() {
            break;
        }
        // Don't test the same board twice by accident: wait for it to go.
        if let Some(mut c) = collar {
            say("unplug the unit");
            while c.gone().is_none() {
                c.next(Duration::from_millis(500)).await;
            }
        }
    }
    if tally.len() > 1 {
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for o in &tally {
            match counts.iter_mut().find(|(w, _)| *w == o.word()) {
                Some((_, n)) => *n += 1,
                None => counts.push((o.word(), 1)),
            }
        }
        say(format!("{} units: {}", tally.len(), counts.iter().map(|(w, n)| format!("{n} {w}")).collect::<Vec<_>>().join(", ")));
    }
    Ok(tally.iter().map(Outcome::code).max().unwrap_or(0))
}

/// One unit through the plan. The collar is returned so the caller can wait for the unplug.
async fn unit(opts: &Options, station: &Station, plan: &TestPlan, serial: &str) -> (Option<Collar>, Outcome) {
    say(format!("{serial}: plug it in"));
    let mut collar = loop {
        match Collar::open(&opts.port, opts.capture.clone(), opts.verbose) {
            Ok(c) => break c,
            Err(_) => tokio::time::sleep(Duration::from_millis(500)).await,
        }
    };
    opts.capture.note(format!("unit {serial}"));
    let started_at = Utc::now();
    let mut seen = Observed::default();

    // Lines from the boot (if the station saw it) come before the first status.
    let boot_until = Instant::now() + Duration::from_secs(2);
    while Instant::now() < boot_until {
        if let Some(l) = collar.next(Duration::from_millis(200)).await {
            match l.line {
                Line::Boot { fw } => {
                    seen.fw = Some(fw);
                    seen.gnss_started_ms = None;
                }
                Line::Log(msg) if msg == "GNSS started" => seen.gnss_started_ms = l.uptime_ms,
                _ => {}
            }
        }
    }
    for _ in 0..3 {
        match collar.status().await {
            Ok(Reply::Status(s)) => seen.fw = Some(s.fw),
            Ok(Reply::Unprovisioned { fw }) => seen.fw = Some(fw),
            _ => continue,
        }
        break;
    }
    if plan.steps.iter().any(|s| s.name.starts_with("gnss_")) && seen.fw.is_some() {
        say(format!("waiting up to {} s for a GNSS fix", opts.gnss_timeout.as_secs()));
        watch(&mut collar, opts.gnss_timeout, &mut seen).await;
    }
    let mut terminal = Terminal;
    let operator: Option<&mut dyn Operator> = if opts.operator { Some(&mut terminal) } else { None };
    let steps = evaluate(plan, &seen, opts.expect_fw.as_deref(), operator);
    let finished_at = Utc::now();

    let mut run = run_body(plan, opts.station.as_deref(), opts.fixture_id.as_deref(), started_at, finished_at, &steps);
    run["run_key"] = json!(run_key(opts.station.as_deref().or(opts.fixture_id.as_deref()), serial, started_at));
    let mut screen = Screen::new(serial);
    let outcome = submit(opts, station, serial, &seen, &mut run, &mut screen).await;
    save(&opts.results, serial, &seen, &run, &screen, &outcome);
    screen.show(&steps, &outcome);
    (Some(collar), outcome)
}

/// The station's own id for a run, so a retry is the same run on the server.
pub fn run_key(station: Option<&str>, serial: &str, started_at: DateTime<Utc>) -> String {
    format!("{}/{serial}/{}", station.unwrap_or("station"), started_at.timestamp_millis())
}

/// A test run as `POST /api/fleet/units/:serial/tests` takes it.
pub fn run_body(
    plan: &TestPlan,
    station: Option<&str>,
    fixture_id: Option<&str>,
    started_at: DateTime<Utc>,
    finished_at: DateTime<Utc>,
    steps: &[StepResult],
) -> Value {
    json!({
        "plan_id": plan.id, "plan_version": plan.version, "station": station, "fixture_id": fixture_id,
        "started_at": started_at, "finished_at": finished_at, "steps": steps,
    })
}

/// Register the unit and post the run; how it went. A passing unit is not
/// provisioned ([`NO_CLAIM_COMMAND`]).
async fn submit(opts: &Options, station: &Station, serial: &str, seen: &Observed, run: &mut Value, screen: &mut Screen) -> Outcome {
    let Some(fw) = seen.fw.clone() else { return Outcome::NoAnswer };
    // The server registers a unit only with its device key, which the unit
    // makes and prints. Firmware 0.2 has none (protocol v2 adds it).
    let Some(device_pubkey) = seen.device_pubkey.clone() else { return Outcome::NoDeviceKey { fw } };
    let unit = json!({
        "serial": serial, "device_class": opts.device_class, "hw_rev": opts.hw_rev, "fw_version": fw,
        "device_pubkey": device_pubkey, "station": opts.station,
    });
    match station.call(reqwest::Method::POST, "/api/fleet/units", Some(&unit)).await {
        // The run belongs to the unit's generation as registered now: if it is
        // sent back meanwhile, the server refuses this run.
        Ok((200 | 201, u)) => run["generation"] = u.get("generation").cloned().unwrap_or(Value::Null),
        Ok((status, v)) => return Outcome::NotRecorded(format!("registering it: {status} {}", err_of(&v))),
        Err(e) => return Outcome::NotRecorded(format!("registering it: {e:#}")),
    }
    match station.post_run(serial, run).await {
        Ok(v) => {
            screen.server = Some(v.clone());
            if v.get("counted").and_then(Value::as_bool) == Some(false) {
                return Outcome::NotRecorded("the server kept the run but didn't count it (a later run of this unit is already in)".into());
            }
            if v.get("result").and_then(Value::as_str) == Some("pass") { Outcome::NotProvisioned(NO_CLAIM_COMMAND.into()) } else { Outcome::Failed }
        }
        Err(e) => Outcome::NotRecorded(format!("{e:#}")),
    }
}

fn save(dir: &PathBuf, serial: &str, seen: &Observed, run: &Value, screen: &Screen, outcome: &Outcome) {
    let rec = json!({ "serial": serial, "fw": seen.fw, "run": run, "server": screen.server, "outcome": outcome.word(), "next": outcome.next_step() });
    let res = std::fs::create_dir_all(dir).and_then(|_| {
        let mut f = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("runs.jsonl"))?;
        writeln!(f, "{rec}")
    });
    if let Err(e) = res {
        eprintln!("couldn't save the run to {}: {e}", dir.display());
    }
}

/// One screen per unit.
struct Screen {
    serial: String,
    server: Option<Value>,
}

impl Screen {
    fn new(serial: &str) -> Self {
        Self { serial: serial.into(), server: None }
    }

    fn show(&self, sent: &[StepResult], outcome: &Outcome) {
        let rule = "─".repeat(64);
        let word = outcome.word();
        println!("\n{rule}");
        println!(" {:<24} {word}", self.serial);
        // The server's verdict per step when it gave one, else what the station saw.
        let judged: HashMap<String, (String, Option<String>)> = self
            .server
            .as_ref()
            .and_then(|v| v.get("steps"))
            .and_then(Value::as_array)
            .map(|steps| {
                steps
                    .iter()
                    .filter_map(|s| {
                        let name = s.get("name")?.as_str()?.to_owned();
                        let verdict = s.get("verdict")?.as_str()?.to_owned();
                        Some((name, (verdict, s.get("why").and_then(Value::as_str).map(str::to_owned))))
                    })
                    .collect()
            })
            .unwrap_or_default();
        for s in sent {
            let (status, why) = judged.get(&s.name).cloned().unwrap_or((s.status.clone(), None));
            let mark = if status == "fail" || status == "missing" { status.to_uppercase() } else { status };
            let value = s.value.as_ref().map(|v| format!("{} {}", v, s.unit.as_deref().unwrap_or(""))).unwrap_or_default();
            let detail = why.or_else(|| s.detail.clone()).unwrap_or_default();
            println!("   {mark:<7} {:<16} {:<10} {detail}", s.name, value.trim());
        }
        if let Some(failed) = self.server.as_ref().and_then(|v| v.get("failed")).and_then(Value::as_array).filter(|f| !f.is_empty()) {
            let names: Vec<&str> = failed.iter().filter_map(Value::as_str).collect();
            println!(" failed: {}", names.join(", "));
        }
        println!(" next: {}", outcome.next_step());
        println!("{rule}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan() -> TestPlan {
        serde_json::from_value(json!({"id": "node-fct", "version": 1, "steps": [
            {"name": "fw_flash", "kind": "check", "required": true},
            {"name": "rail_3v3", "kind": "measure", "limit_lo": 3.2, "limit_hi": 3.4, "unit": "V", "required": true},
            {"name": "gnss_fix", "kind": "check", "required": true},
            {"name": "gnss_ttff", "kind": "measure", "limit_hi": 60.0, "unit": "s", "required": true},
            {"name": "lora_tx_rssi", "kind": "measure", "unit": "dBm", "required": true},
            {"name": "buzzer_left", "kind": "check", "required": true},
        ]}))
        .unwrap()
    }

    struct Answers(Vec<Option<Value>>);
    impl Operator for Answers {
        fn ask(&mut self, _: &PlanStep) -> Option<Value> {
            self.0.remove(0)
        }
    }

    #[test]
    fn nothing_the_console_cant_show_passes() {
        let seen = Observed {
            fw: Some("0.2.0".into()),
            gnss_started_ms: Some(420),
            first_fix: Some(FirstFix { uptime_ms: Some(31_870), lat: 38.12, lon: -92.41, accuracy_m: 3.2, sats: Some(9) }),
            waited: Duration::from_secs(31),
            ..Observed::default()
        };
        let r = evaluate(&plan(), &seen, Some("0.2.0"), None);
        let by: HashMap<_, _> = r.iter().map(|s| (s.name.as_str(), s)).collect();
        assert_eq!(by["fw_flash"].status, "pass");
        assert_eq!(by["gnss_fix"].status, "pass");
        assert_eq!((by["gnss_ttff"].value.clone(), by["gnss_ttff"].unit.as_deref()), (Some(json!(31.5)), Some("s")));
        for name in ["rail_3v3", "lora_tx_rssi", "buzzer_left"] {
            assert_eq!(by[name].status, "skip", "{name}");
        }
        assert!(by["rail_3v3"].detail.as_deref().unwrap().contains("instruments"));
        assert!(by["lora_tx_rssi"].detail.as_deref().unwrap().contains("firmware"));
    }

    #[test]
    fn a_silent_unit_fails_and_an_unseen_boot_skips_ttff() {
        let r = evaluate(&plan(), &Observed { waited: Duration::from_secs(120), ..Observed::default() }, Some("0.2.0"), None);
        assert_eq!(r[0].status, "fail");
        assert_eq!((r[2].status.as_str(), r[2].detail.as_deref()), ("fail", Some("no fix in 120 s")));
        let seen = Observed {
            fw: Some("0.2.0".into()),
            first_fix: Some(FirstFix { uptime_ms: Some(900_000), lat: 38.12, lon: -92.41, accuracy_m: 5.0, sats: None }),
            ..Observed::default()
        };
        assert_eq!(evaluate(&plan(), &seen, Some("0.2.0"), None)[3].status, "skip");
    }

    #[test]
    fn operator_readings_go_in_as_given() {
        let mut ops = Answers(vec![Some(json!(3.31)), None, Some(Value::Bool(false))]);
        let r = evaluate(&plan(), &Observed::default(), None, Some(&mut ops));
        assert_eq!((r[1].status.as_str(), r[1].value.clone(), r[1].unit.as_deref()), ("pass", Some(json!(3.31)), Some("V")));
        assert_eq!(r[4].status, "skip");
        assert_eq!((r[5].status.as_str(), r[5].detail.as_deref()), ("fail", Some("operator")));
    }

    /// Review P1-08: garbage firmware, a NaN fix and a fix timed before GNSS
    /// started all passed. Each fails now, and a version nobody named skips.
    #[test]
    fn garbage_answers_and_impossible_fixes_dont_pass() {
        let by = |r: &[StepResult], n: &str| r.iter().find(|s| s.name == n).unwrap().clone();
        let garbage = Observed {
            fw: Some("garbage".into()),
            gnss_started_ms: Some(5000),
            first_fix: Some(FirstFix { uptime_ms: Some(1000), lat: f64::NAN, lon: f64::NAN, accuracy_m: f64::NAN, sats: Some(0) }),
            ..Observed::default()
        };
        let r = evaluate(&plan(), &garbage, Some("0.2.0"), None);
        assert_eq!(by(&r, "fw_flash").status, "fail");
        assert_eq!(by(&r, "gnss_fix").status, "fail");
        assert_eq!(by(&r, "gnss_ttff").status, "fail");

        let rebooted = Observed {
            fw: Some("0.2.0".into()),
            gnss_started_ms: Some(5000),
            first_fix: Some(FirstFix { uptime_ms: Some(1000), lat: 38.12, lon: -92.41, accuracy_m: 3.2, sats: Some(9) }),
            ..Observed::default()
        };
        let r = evaluate(&plan(), &rebooted, Some("0.2.1"), None);
        assert_eq!((by(&r, "fw_flash").status.as_str(), by(&r, "fw_flash").detail.as_deref()), ("fail", Some("runs 0.2.0, the fixture flashes 0.2.1")));
        assert_eq!(by(&r, "gnss_fix").status, "pass");
        assert_eq!(by(&r, "gnss_ttff").status, "fail", "fix before GNSS start: the clock went back");
        assert_eq!(evaluate(&plan(), &rebooted, None, None)[0].status, "skip", "no --fw, no pass");

        for (lat, lon, acc, sats) in
            [(0.0, 0.0, 3.0, Some(9)), (38.1, -92.4, 0.0, Some(9)), (38.1, -92.4, 500.0, Some(9)), (38.1, -92.4, 3.0, Some(2)), (91.0, 0.5, 3.0, None)]
        {
            let seen =
                Observed { fw: Some("0.2.0".into()), first_fix: Some(FirstFix { uptime_ms: None, lat, lon, accuracy_m: acc, sats }), ..Observed::default() };
            assert_eq!(by(&evaluate(&plan(), &seen, Some("0.2.0"), None), "gnss_fix").status, "fail", "{lat},{lon} ±{acc} {sats:?}");
        }
    }

    #[test]
    fn every_outcome_is_nonzero_and_says_what_next() {
        for o in [
            Outcome::NoAnswer,
            Outcome::NoDeviceKey { fw: "0.2.0".into() },
            Outcome::NotRecorded("x".into()),
            Outcome::Failed,
            Outcome::NotProvisioned(NO_CLAIM_COMMAND.into()),
        ] {
            assert_ne!(o.code(), 0, "{o:?}");
            assert!(!o.next_step().is_empty());
        }
        assert!(Outcome::NoDeviceKey { fw: "0.2.0".into() }.next_step().starts_with("can't register: firmware 0.2.0 doesn't print a device key"));
        assert_eq!(run_key(Some("bench-1"), "OCN-1", DateTime::from_timestamp_millis(1_700_000_000_123).unwrap()), "bench-1/OCN-1/1700000000123");
    }
}
