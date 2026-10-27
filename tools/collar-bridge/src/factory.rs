//! `collar-bridge factory`: the station beside the test fixture, one unit
//! after another (`docs/api/fleet.md`).
//!
//! For each unit: scan its serial, plug it in, run the plan for its class
//! over the serial console, register it, post the run (the server gives the
//! verdict), and on a pass provision it. A step this station can't test is
//! sent as `skip` with the reason, never as a pass; the server fails a run
//! whose required steps were skipped.
//!
//! What firmware 0.2's console can show today: that it runs (`fw_flash`) and
//! GNSS (`gnss_fix`, and `gnss_ttff` when the station saw it boot). Readings
//! from the fixture's instruments or the operator's meter can be typed in
//! with `--operator`.

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
use crate::collar::{Collar, REPLY_WAIT, say};
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

/// Each plan step, from what the console showed and what the operator said.
pub fn evaluate(plan: &TestPlan, seen: &Observed, mut operator: Option<&mut dyn Operator>) -> Vec<StepResult> {
    let mut out = Vec::with_capacity(plan.steps.len());
    for step in &plan.steps {
        let name = step.name.as_str();
        let r = match name {
            "fw_flash" => match &seen.fw {
                Some(fw) if step.kind == "record" => StepResult { value: Some(json!(fw)), ..pass(name, format!("firmware {fw} answers on the console")) },
                Some(fw) => pass(name, format!("firmware {fw} answers on the console")),
                None => fail(name, "nothing answered `status` on the console"),
            },
            "gnss_fix" => match &seen.first_fix {
                Some(f) => pass(
                    name,
                    format!("{:.6},{:.6} ±{:.1} m, {} sats", f.lat, f.lon, f.accuracy_m, f.sats.map_or("?".into(), |s| s.to_string())),
                ),
                None => fail(name, format!("no fix in {} s", seen.waited.as_secs())),
            },
            "gnss_ttff" => match (&seen.first_fix, seen.gnss_started_ms) {
                (Some(FirstFix { uptime_ms: Some(fix), .. }), Some(start)) => StepResult {
                    name: name.into(),
                    status: "pass".into(),
                    value: Some(json!(((fix.saturating_sub(start)) as f64 / 100.0).round() / 10.0)),
                    unit: Some("s".into()),
                    detail: Some("GNSS start to first fix, on the collar's clock".into()),
                },
                (None, _) => fail(name, format!("no fix in {} s", seen.waited.as_secs())),
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
                Some(v) => StepResult { name: name.into(), status: "pass".into(), value: Some(v), unit: step.unit.clone(), detail: Some("operator reading".into()) },
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
            Line::Boot { fw } => seen.fw = Some(fw),
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

pub async fn run(opts: Options) -> anyhow::Result<()> {
    let station = Station::new(&opts.server, &opts.station_token);
    let plan = station.plan(&opts.device_class).await?;
    say(format!("plan {} v{} for {}: {} steps", plan.id, plan.version, opts.device_class, plan.steps.len()));
    loop {
        let serial = match &opts.serial {
            Some(s) => s.clone(),
            None => match read_line("\nScan or type the serial (blank to stop): ").map(|s| s.trim().to_owned()) {
                Some(s) if !s.is_empty() => s,
                _ => return Ok(()),
            },
        };
        let collar = unit(&opts, &station, &plan, &serial).await;
        if opts.serial.is_some() {
            return Ok(());
        }
        // Don't test the same board twice by accident: wait for it to go.
        if let Some(mut c) = collar {
            say("unplug the unit");
            while c.gone().is_none() {
                c.next(Duration::from_millis(500)).await;
            }
        }
    }
}

/// One unit through the plan. The collar is returned so the caller can wait for the unplug.
async fn unit(opts: &Options, station: &Station, plan: &TestPlan, serial: &str) -> Option<Collar> {
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
                Line::Boot { fw } => seen.fw = Some(fw),
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
    let steps = evaluate(plan, &seen, operator);
    let finished_at = Utc::now();

    let run = run_body(plan, opts.station.as_deref(), opts.fixture_id.as_deref(), started_at, finished_at, &steps);
    let mut screen = Screen::new(serial);
    let outcome = submit(opts, station, serial, &seen, &run, &mut screen).await;
    if let Some(Verdict::Pass) = outcome
        && let Err(e) = provision(station, serial, &mut collar, &mut screen).await
    {
        screen.line(format!("not provisioned: {e}"));
    }
    save(&opts.results, serial, &seen, &run, &screen);
    screen.show(&steps, outcome);
    Some(collar)
}

/// A test run as `POST /api/fleet/units/:serial/tests` takes it.
pub fn run_body(plan: &TestPlan, station: Option<&str>, fixture_id: Option<&str>, started_at: DateTime<Utc>, finished_at: DateTime<Utc>, steps: &[StepResult]) -> Value {
    json!({
        "plan_id": plan.id, "plan_version": plan.version, "station": station, "fixture_id": fixture_id,
        "started_at": started_at, "finished_at": finished_at, "steps": steps,
    })
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Verdict {
    Pass,
    Fail,
}

/// Register the unit and post the run; the server's verdict.
async fn submit(opts: &Options, station: &Station, serial: &str, seen: &Observed, run: &Value, screen: &mut Screen) -> Option<Verdict> {
    let Some(fw) = seen.fw.clone() else {
        screen.line("not registered: the unit never answered on the console".into());
        return None;
    };
    // The server registers a unit only with its device key, which the unit
    // makes and prints. Firmware 0.2 has none (protocol v2 adds it).
    let Some(device_pubkey) = seen.device_pubkey.clone() else {
        screen.line(format!("not registered: firmware {fw} prints no device key (protocol v2 firmware makes one). The run is saved locally only."));
        return None;
    };
    let unit = json!({
        "serial": serial, "device_class": opts.device_class, "hw_rev": opts.hw_rev, "fw_version": fw,
        "device_pubkey": device_pubkey, "station": opts.station,
    });
    match station.call(reqwest::Method::POST, "/api/fleet/units", Some(&unit)).await {
        Ok((200 | 201, _)) => {}
        Ok((status, v)) => {
            screen.line(format!("not registered ({status}): {}", err_of(&v)));
            return None;
        }
        Err(e) => {
            screen.line(format!("not registered: {e:#}"));
            return None;
        }
    }
    match station.post_run(serial, run).await {
        Ok(v) => {
            screen.server = Some(v.clone());
            Some(if v.get("result").and_then(Value::as_str) == Some("pass") { Verdict::Pass } else { Verdict::Fail })
        }
        Err(e) => {
            screen.line(format!("{e:#}"));
            None
        }
    }
}

/// Provision the unit on the server; write anything meant for its console.
async fn provision(station: &Station, serial: &str, collar: &mut Collar, screen: &mut Screen) -> anyhow::Result<()> {
    let (status, v) = station.call(reqwest::Method::POST, &format!("/api/fleet/units/{serial}/provision"), Some(&json!({}))).await?;
    if status != 200 && status != 201 {
        bail!("{status}: {}", err_of(&v));
    }
    if let Some(cc) = v.get("claim_code").and_then(Value::as_str) {
        screen.line(format!("provisioned, claim code {cc}"));
    }
    if let Some(card) = v.get("card").and_then(Value::as_str) {
        screen.line(format!("card {card}"));
    }
    // A console line, when the server gives one. Protocol v2 puts the claim
    // code on the unit at the fixture; firmware 0.2 has no command for it.
    let Some(line) = v.get("line").and_then(Value::as_str) else {
        screen.line("nothing written to the unit: its firmware has no command to store a claim code yet".into());
        return Ok(());
    };
    match collar.command(line, REPLY_WAIT).await {
        Ok(Reply::Provisioned(id)) => screen.line(format!("the unit took its provisioning: {id}")),
        Ok(Reply::Error(code)) => bail!("the unit refused its provisioning: {code}"),
        Ok(other) => bail!("unexpected reply: {other:?}"),
        Err(e) => bail!("{e}"),
    }
    Ok(())
}

fn save(dir: &PathBuf, serial: &str, seen: &Observed, run: &Value, screen: &Screen) {
    let rec = json!({ "serial": serial, "fw": seen.fw, "run": run, "server": screen.server, "notes": screen.notes });
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
    notes: Vec<String>,
}

impl Screen {
    fn new(serial: &str) -> Self {
        Self { serial: serial.into(), server: None, notes: Vec::new() }
    }

    fn line(&mut self, s: String) {
        self.notes.push(s);
    }

    fn show(&self, sent: &[StepResult], verdict: Option<Verdict>) {
        let rule = "─".repeat(64);
        let word = match verdict {
            Some(Verdict::Pass) => "PASS",
            Some(Verdict::Fail) => "FAIL",
            None => "NOT RECORDED",
        };
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
        for n in &self.notes {
            println!(" {n}");
        }
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
        let r = evaluate(&plan(), &seen, None);
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
        let r = evaluate(&plan(), &Observed { waited: Duration::from_secs(120), ..Observed::default() }, None);
        assert_eq!(r[0].status, "fail");
        assert_eq!((r[2].status.as_str(), r[2].detail.as_deref()), ("fail", Some("no fix in 120 s")));
        let seen = Observed {
            fw: Some("0.2.0".into()),
            first_fix: Some(FirstFix { uptime_ms: Some(900_000), lat: 0.0, lon: 0.0, accuracy_m: 5.0, sats: None }),
            ..Observed::default()
        };
        assert_eq!(evaluate(&plan(), &seen, None)[3].status, "skip");
    }

    #[test]
    fn operator_readings_go_in_as_given() {
        let mut ops = Answers(vec![Some(json!(3.31)), None, Some(Value::Bool(false))]);
        let r = evaluate(&plan(), &Observed::default(), Some(&mut ops));
        assert_eq!((r[1].status.as_str(), r[1].value.clone(), r[1].unit.as_deref()), ("pass", Some(json!(3.31)), Some("V")));
        assert_eq!(r[4].status, "skip");
        assert_eq!((r[5].status.as_str(), r[5].detail.as_deref()), ("fail", Some("operator")));
    }
}
