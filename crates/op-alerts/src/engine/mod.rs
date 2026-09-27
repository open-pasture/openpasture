//! The alert engine: evaluates rules on a 10 s tick (each on its own
//! cadence) and early on a few bus events, and keeps the `alerts` table in
//! step with what the rules find. Evaluation is idempotent from database
//! state, so a restart or a lagged bus loses nothing.
//!
//! Lifecycle: a new key opens an alert; a changed title, place, severity or
//! targets updates it; a key gone for `clear_after_min` (and at least two of
//! its rule's own runs) resolves it (back inside that time, it keeps its
//! row); ack stops re-notification; resolve closes it (it stays closed while
//! its condition lasts); a key back after it resolved opens a new row.
//! `rollup_min` or more collar alerts of one kind in one herd are one alert
//! `<kind>:herd:<herd id>`, which keeps taking members until the last one
//! clears (`herd_silent` takes in its herd's `silent` alerts the same way).
//! Once people were told about a rollup (a text, an ack), members that join
//! it are a new breakout when they are at least as many as the told ones
//! still in it: it opens again as a new alert with every member, sent like
//! any new alert, and the old one resolves into it. Closed by hand, it keeps
//! only the members it was closed with while they stay out; new ones alert
//! on their own, or as a new rollup of every member once there are
//! `rollup_min` of them.

pub mod config;
pub mod store;

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock, Mutex};

use chrono::{DateTime, Duration, Utc};
use op_core::alert::{Alert, AlertStatus};
use op_core::time::now;
use op_core::{Ctx, Event, LonLat, Severity, id};
use serde_json::{Value, json};
use tokio::sync::Notify;

use crate::rules::{Candidate, Rule, RuleConfig, RuleDescriptor, rules};
use crate::{routing, text};
use store::Row;

/// Rules that wait `start_grace_min` after a server start: every collar
/// looks silent until it reports again.
const GRACED: &[&str] = &["silent", "herd_silent"];
/// (absorber, absorbed): an open absorber alert for a herd takes in that
/// herd's alerts of the absorbed kind.
const ABSORBS: &[(&str, &str)] = &[("herd_silent", "silent")];
/// Kinds that resolve as soon as their key is gone: they follow a record
/// (a decision answered), not patchy telemetry.
const CLEAR_AT_ONCE: &[&str] = &["decision_waiting"];
/// A place moving less than this doesn't update an alert.
const MOVED_M: f64 = 3.0;
/// A rule a little early on its cadence still runs (the loop's clock jitters).
const CADENCE_SLACK: Duration = Duration::milliseconds(900);
/// Base tick of the rules.
const TICK_S: i64 = 10;
/// Wake-ups wait for this much quiet on the bus, and no longer than `WAKE_MAX`.
const WAKE_QUIET: std::time::Duration = std::time::Duration::from_secs(2);
const WAKE_MAX: std::time::Duration = std::time::Duration::from_secs(10);

// ---- per-server state -----------------------------------------------------------------

#[derive(Default)]
struct Shared {
    started: Option<DateTime<Utc>>,
    wake: Arc<Notify>,
    woken: WokenRuns,
}

/// Evaluations the loop ran because of bus events, and the kinds the last one
/// covered (for tests and logs).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WokenRuns {
    pub runs: u64,
    pub last_kinds: Vec<String>,
}

/// The loop's wake-up evaluations so far.
pub fn woken_runs(ctx: &Ctx) -> WokenRuns {
    with_shared(ctx, |s| s.woken.clone())
}

static SERVERS: LazyLock<Mutex<HashMap<PathBuf, Shared>>> = LazyLock::new(Default::default);

fn with_shared<T>(ctx: &Ctx, f: impl FnOnce(&mut Shared) -> T) -> T {
    let mut m = SERVERS.lock().unwrap_or_else(|e| e.into_inner());
    f(m.entry(ctx.data_dir().to_path_buf()).or_default())
}

/// When this server's engine started (grace for `silent`). Tests set it.
pub fn set_started(ctx: &Ctx, at: DateTime<Utc>) {
    with_shared(ctx, |s| s.started = Some(at));
}

pub fn started(ctx: &Ctx) -> Option<DateTime<Utc>> {
    with_shared(ctx, |s| s.started)
}

/// Within `start_grace_min` of the engine's start.
pub async fn in_grace(ctx: &Ctx, now: DateTime<Utc>) -> anyhow::Result<bool> {
    let Some(start) = started(ctx) else { return Ok(false) };
    let grace = Duration::minutes(config::policy(ctx).await?.start_grace_min as i64);
    Ok(now < start + grace)
}

/// Ask the engine to evaluate every rule now (rules or policy changed).
pub fn wake(ctx: &Ctx) {
    with_shared(ctx, |s| s.wake.clone()).notify_one();
}

fn wake_handle(ctx: &Ctx) -> Arc<Notify> {
    with_shared(ctx, |s| s.wake.clone())
}

/// Mean of some points.
pub fn centroid(points: &[LonLat]) -> Option<LonLat> {
    if points.is_empty() {
        return None;
    }
    let n = points.len() as f64;
    Some([points.iter().map(|p| p[0]).sum::<f64>() / n, points.iter().map(|p| p[1]).sum::<f64>() / n])
}

// ---- evaluation -----------------------------------------------------------------------

/// What one evaluation changed.
#[derive(Debug, Default)]
pub struct Changes {
    pub opened: Vec<Alert>,
    pub updated: Vec<Alert>,
    pub resolved: Vec<Alert>,
}

impl Changes {
    fn extend(&mut self, o: Changes) {
        self.opened.extend(o.opened);
        self.updated.extend(o.updated);
        self.resolved.extend(o.resolved);
    }
}

pub struct Engine {
    rules: Vec<Box<dyn Rule>>,
    last: HashMap<&'static str, DateTime<Utc>>,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Self { rules: rules(), last: HashMap::new() }
    }

    pub fn descriptors(&self) -> Vec<RuleDescriptor> {
        self.rules.iter().map(|r| r.descriptor()).collect()
    }

    /// Kinds whose cadence is due at `now` (every kind the first time).
    pub fn due(&self, now: DateTime<Utc>) -> HashSet<&'static str> {
        self.descriptors()
            .into_iter()
            .filter(|d| self.last.get(d.kind).is_none_or(|t| now - *t >= Duration::seconds(d.cadence_s.max(1) as i64) - CADENCE_SLACK || now < *t))
            .map(|d| d.kind)
            .collect()
    }

    /// Kinds a bus event of type `ty` wakes.
    pub fn woken_by(&self, types: &HashSet<String>) -> HashSet<&'static str> {
        self.descriptors().into_iter().filter(|d| d.wake_on.iter().any(|w| types.contains(*w))).map(|d| d.kind).collect()
    }

    /// Evaluate these kinds at `now` and store what changed.
    pub async fn evaluate(&mut self, ctx: &Ctx, now: DateTime<Utc>, kinds: &HashSet<&str>) -> anyhow::Result<Changes> {
        let policy = config::policy(ctx).await?;
        let configs = config::rule_configs(ctx).await?;
        let grace = in_grace(ctx, now).await?;
        let mut found: Vec<(&'static str, u32, RuleConfig, Vec<Candidate>)> = Vec::new();
        for r in &self.rules {
            let d = r.descriptor();
            if !kinds.contains(d.kind) {
                continue;
            }
            self.last.insert(d.kind, now);
            if grace && GRACED.contains(&d.kind) {
                continue; // no evaluation, and nothing clears
            }
            let cfg = configs.get(d.kind).cloned().unwrap_or(d.default.clone());
            let candidates = if cfg.enabled {
                match r.evaluate(ctx, &cfg, now).await {
                    Ok(c) => c,
                    Err(e) => {
                        // A failing rule keeps its alerts as they are.
                        tracing::warn!(rule = d.kind, "alert rule failed: {e:#}");
                        continue;
                    }
                }
            } else {
                vec![]
            };
            found.push((d.kind, d.cadence_s, cfg, candidates));
        }
        // Absorbers first, so what they take in is known.
        found.sort_by_key(|(k, ..)| !ABSORBS.iter().any(|(a, _)| a == k));
        let mut changes = Changes::default();
        for (kind, cadence_s, cfg, candidates) in found {
            changes.extend(reconcile(ctx, kind, cadence_s, &cfg, candidates, &policy, now).await?);
        }
        Ok(changes)
    }

    /// One pass of the loop: kinds due by cadence plus those `types` woke.
    pub async fn tick(&mut self, ctx: &Ctx, now: DateTime<Utc>, types: &HashSet<String>) -> anyhow::Result<Changes> {
        let mut kinds = self.due(now);
        kinds.extend(self.woken_by(types));
        if kinds.is_empty() {
            return Ok(Changes::default());
        }
        self.evaluate(ctx, now, &kinds).await
    }
}

/// Evaluate every rule at `now` (a fresh engine), for tests and tools.
pub async fn evaluate_all(ctx: &Ctx, now: DateTime<Utc>) -> anyhow::Result<Changes> {
    let mut e = Engine::new();
    let kinds: HashSet<&str> = e.descriptors().iter().map(|d| d.kind).collect();
    e.evaluate(ctx, now, &kinds).await
}

fn effective_severity(cfg: &RuleConfig, c: &Candidate) -> Severity {
    c.severity.map_or(cfg.severity, |s| s.max(cfg.severity))
}

fn moved(a: Option<LonLat>, b: Option<LonLat>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => op_geo::projection::distance_m(a, b) > MOVED_M,
        (None, None) => false,
        _ => true,
    }
}

/// A herd's collar candidates as one rollup candidate.
fn rollup(kind: &str, herd_id: &str, members: &[Candidate], severity: Severity) -> Candidate {
    let first = &members[0].data;
    let points: Vec<LonLat> = members.iter().filter_map(|m| m.at).collect();
    let mut targets: Vec<(String, String)> = Vec::new();
    for m in members {
        for t in &m.targets {
            if !targets.contains(t) {
                targets.push(t.clone());
            }
        }
    }
    let since = members.iter().filter_map(|m| m.data["since"].as_str()).min().map(str::to_owned);
    let paddock = first["paddock"].as_str();
    let mut data = json!({
        "herd": first["herd"],
        "count": members.len(),
        "members": members.iter().map(|m| member_facts(&m.data)).collect::<Vec<_>>(),
    });
    if let Some(p) = paddock {
        data["paddock"] = json!(p);
    }
    if let Some(s) = since {
        data["since"] = json!(s);
    }
    Candidate {
        key: format!("{kind}:herd:{herd_id}"),
        subject: ("herd".into(), herd_id.to_owned()),
        herd_id: Some(herd_id.to_owned()),
        title: text::rollup_title(kind, members.len(), paddock),
        body: None,
        at: centroid(&points),
        targets,
        data,
        severity: Some(severity),
    }
}

/// What the texts and the brief need of one member.
fn member_facts(d: &Value) -> Value {
    let mut m = serde_json::Map::new();
    for k in ["label", "pct", "accuracy_m"] {
        if let Some(v) = d.get(k).filter(|v| !v.is_null()) {
            m.insert(k.into(), v.clone());
        }
    }
    Value::Object(m)
}

/// When a new alert's first send is due: with a pending batch of its kind,
/// herd and severity if one is still open, else after its window. A prompt
/// with a deadline ([`routing::deadline`]) goes after the critical window, on
/// its own.
async fn batch_at(
    ctx: &Ctx,
    kind: &str,
    herd_id: Option<&str>,
    severity: Severity,
    urgent: bool,
    policy: &config::Policy,
    now: DateTime<Utc>,
) -> anyhow::Result<DateTime<Utc>> {
    if urgent {
        return Ok(now + Duration::seconds(policy.critical_window_s as i64));
    }
    let pending: Option<String> = sqlx::query_scalar(
        "SELECT MIN(batch_at) FROM alerts WHERE kind = ? AND herd_id IS ? AND severity = ? AND status != 'resolved' AND notify = 1
           AND routed_at IS NULL AND batch_at > ?",
    )
    .bind(kind)
    .bind(herd_id)
    .bind(op_core::DbEnum::as_db(&severity))
    .bind(op_core::time::to_db(&now))
    .fetch_one(ctx.db())
    .await?;
    if let Some(t) = pending.and_then(|t| op_core::time::from_db(&t).ok()) {
        return Ok(t);
    }
    let window = if severity == Severity::Critical { policy.critical_window_s } else { policy.group_window_s };
    Ok(now + Duration::seconds(window as i64))
}

/// How long a key must be gone before its alert resolves: `clear_after_min`,
/// and never less than two runs of its rule (a 300 s rule that misses one
/// run keeps its alert). `clear_after_min` 0 resolves at once.
fn clear_after(policy: &config::Policy, cadence_s: u32) -> Duration {
    if policy.clear_after_min == 0 {
        return Duration::zero();
    }
    Duration::minutes(policy.clear_after_min as i64).max(Duration::seconds(2 * cadence_s.max(1) as i64))
}

/// The newest row of a key when someone closed it by hand and its cause was
/// seen within `clear_after` since: it stays closed.
async fn closed_by_hand(ctx: &Ctx, key: &str, clear_after: Duration, now: DateTime<Utc>) -> anyhow::Result<Option<Row>> {
    Ok(store::last_resolved(ctx, key)
        .await?
        .filter(|last| last.alert.resolved_by.is_some() && last.alert.rolled_into.is_none() && now - last.seen_at < clear_after))
}

/// The collars people were told about in this rollup: when it was texted,
/// acked or closed (`announced`; an acked or closed row from before that
/// column, its targets). `None` while nobody was told.
fn told_of(r: &Row) -> Option<HashSet<String>> {
    if let Some(a) = &r.announced {
        return Some(a.iter().cloned().collect());
    }
    (r.alert.status != AlertStatus::Open).then(|| r.alert.targets.iter().filter(|(k, _)| k == "collar").map(|(_, id)| id.clone()).collect())
}

/// Bring one kind's alerts in step with its candidates.
async fn reconcile(
    ctx: &Ctx,
    kind: &str,
    cadence_s: u32,
    cfg: &RuleConfig,
    candidates: Vec<Candidate>,
    policy: &config::Policy,
    now: DateTime<Utc>,
) -> anyhow::Result<Changes> {
    let clear_after = clear_after(policy, cadence_s);
    let mut changes = Changes::default();
    let mut existing: HashMap<String, Row> = store::unresolved(ctx, kind).await?.into_iter().map(|r| (r.alert.key.clone(), r)).collect();

    // Herds whose absorber alert is open take in this kind's alerts there.
    let mut absorbers: HashMap<String, String> = HashMap::new();
    for (absorber, _) in ABSORBS.iter().filter(|(_, absorbed)| *absorbed == kind) {
        for r in store::unresolved(ctx, absorber).await? {
            if let Some(h) = r.alert.herd_id.clone() {
                absorbers.insert(h, r.alert.id.clone());
            }
        }
    }
    let mut rolled: HashMap<String, String> = HashMap::new(); // member key -> rollup id

    // Collar candidates by herd, for rollups.
    let mut singles = Vec::new();
    let mut by_herd: HashMap<String, Vec<Candidate>> = HashMap::new();
    for c in candidates {
        match &c.herd_id {
            Some(h) if absorbers.contains_key(h) => {}
            Some(h) if c.subject.0 == "collar" => by_herd.entry(h.clone()).or_default().push(c),
            _ => singles.push(c),
        }
    }
    let mut herds: Vec<String> = by_herd.keys().cloned().collect();
    herds.sort();
    let mut finals = singles;
    let mut new_ids: HashMap<String, String> = HashMap::new();
    let mut touched = Vec::new();
    let rollup_min = policy.rollup_min as usize;
    for h in herds {
        let members = by_herd.remove(&h).unwrap_or_default();
        let key = format!("{kind}:herd:{h}");
        // The row that holds this herd's rollup: open or acked, else closed by
        // hand while members it was closed with stay out.
        let closed = if existing.contains_key(&key) { None } else { closed_by_hand(ctx, &key, clear_after, now).await? };
        let current = existing.get(&key).or(closed.as_ref());
        let renew = match current {
            None if members.len() < rollup_min => {
                finals.extend(members);
                continue;
            }
            None => false,
            Some(r) => match told_of(r) {
                None => false,
                Some(told) => {
                    // Members new since people were told are a new breakout:
                    // after a close, `rollup_min` of them; else as many as it
                    // still has of the told ones (so it texts again when it
                    // doubles, and a straggler left out doesn't hide the next).
                    let (fresh, kept): (Vec<Candidate>, Vec<Candidate>) = members.iter().cloned().partition(|m| !told.contains(&m.subject.1));
                    match &closed {
                        Some(c) if fresh.len() < rollup_min => {
                            // The ones it was closed with stay closed; new ones alert on their own.
                            if !kept.is_empty() {
                                touched.push(c.alert.id.clone());
                            }
                            for m in &kept {
                                rolled.insert(m.key.clone(), c.alert.id.clone());
                            }
                            finals.extend(fresh);
                            continue;
                        }
                        Some(_) => true,
                        None => fresh.len() >= kept.len().max(1),
                    }
                }
            },
        };
        let id = match current {
            Some(r) if !renew => r.alert.id.clone(),
            _ => id::new_id(store::ALERT),
        };
        for m in &members {
            rolled.insert(m.key.clone(), id.clone());
        }
        // A new alert for all of them; the one people were told about goes into it.
        if renew
            && let Some(old) = existing.remove(&key)
            && let Some(a) = store::resolve(ctx, &old.alert.id, None, Some(&id), now).await?
        {
            tracing::info!(kind, key = %a.key, into = %id, "rollup opened again for a new breakout");
            changes.resolved.push(a);
        }
        let sev = members.iter().map(|m| effective_severity(cfg, m)).max().unwrap_or(cfg.severity);
        new_ids.insert(key, id);
        finals.push(rollup(kind, &h, &members, sev));
    }

    for c in finals {
        let severity = effective_severity(cfg, &c);
        if let Some(row) = existing.remove(&c.key) {
            let old = &row.alert;
            let changed =
                old.title != c.title || old.severity != severity || old.targets != c.targets || old.body != c.body || old.data != c.data || moved(old.at, c.at);
            if changed {
                let mut a = old.clone();
                a.title = c.title;
                a.severity = severity;
                a.targets = c.targets;
                a.body = c.body;
                a.data = c.data;
                if moved(old.at, c.at) {
                    a.at = c.at;
                }
                a.updated_at = now;
                if let Some(a) = store::update(ctx, &a, now).await? {
                    ctx.publish(Event::Alert { alert: store::public(a.clone()) });
                    changes.updated.push(a);
                }
            } else {
                touched.push(old.id.clone());
            }
            continue;
        }
        // Closed by hand while the condition lasted: stays closed (rollups were
        // settled above).
        if !new_ids.contains_key(&c.key)
            && let Some(last) = closed_by_hand(ctx, &c.key, clear_after, now).await?
        {
            touched.push(last.alert.id.clone());
            continue;
        }
        let notify = cfg.notify && severity >= Severity::Warning;
        let urgent = routing::deadline(kind, &c.data, now).is_some();
        let batch = if notify { Some(batch_at(ctx, kind, c.herd_id.as_deref(), severity, urgent, policy, now).await?) } else { None };
        let row = Row {
            alert: Alert {
                id: new_ids.get(&c.key).cloned().unwrap_or_else(|| id::new_id(store::ALERT)),
                kind: kind.to_owned(),
                key: c.key,
                severity,
                status: AlertStatus::Open,
                herd_id: c.herd_id,
                title: c.title,
                body: c.body,
                at: c.at,
                targets: c.targets,
                data: c.data,
                opened_at: now,
                updated_at: now,
                acked_at: None,
                acked_by: None,
                resolved_at: None,
                resolved_by: None,
                rolled_into: None,
            },
            seen_at: now,
            notify,
            batch_at: batch,
            routed_at: None,
            tier: 0,
            escalated_at: None,
            renotified: 0,
            renotified_at: None,
            announced: None,
        };
        if store::insert(ctx, &row).await? {
            tracing::info!(kind, key = %row.alert.key, severity = ?row.alert.severity, "alert opened");
            ctx.publish(Event::Alert { alert: store::public(row.alert.clone()) });
            changes.opened.push(row.alert);
        }
    }
    store::touch(ctx, &touched, now).await?;

    // Keys that are gone: into a rollup or an absorber at once, else after a while.
    for (key, row) in existing {
        let into = rolled.get(&key).cloned().or_else(|| row.alert.herd_id.as_ref().and_then(|h| absorbers.get(h).cloned()));
        let resolved = if let Some(into) = into {
            store::resolve(ctx, &row.alert.id, None, Some(&into), now).await?
        } else if CLEAR_AT_ONCE.contains(&kind) || now - row.seen_at >= clear_after {
            store::resolve(ctx, &row.alert.id, None, None, now).await?
        } else {
            None
        };
        if let Some(a) = resolved {
            tracing::info!(kind, key = %a.key, rolled_into = ?a.rolled_into, "alert resolved");
            changes.resolved.push(a);
        }
    }
    Ok(changes)
}

// ---- the loop ---------------------------------------------------------------------------

/// Bus event types that can wake rules. Frequent telemetry (`fix`, `collar`,
/// `cue`) never does.
fn wake_type(e: &Event) -> Option<String> {
    match e {
        Event::Fix { .. } | Event::Collar { .. } | Event::Cue { .. } | Event::DecisionLog { .. } | Event::Alert { .. } | Event::Message { .. } => None,
        Event::Escape { .. } => Some("escape".into()),
        Event::Decision { .. } => Some("decision".into()),
        Event::Move { .. } => Some("move".into()),
        Event::Ack { .. } => Some("ack".into()),
        Event::Boundary { .. } => Some("boundary".into()),
        other => serde_json::to_value(other).ok().and_then(|v| v["type"].as_str().map(str::to_owned)),
    }
}

/// Start the engine: rules on their cadence, wake-ups, routing every 2 s.
pub fn start(ctx: Ctx) {
    set_started(&ctx, now());
    text::brief::register(&ctx);
    tokio::spawn(run(ctx));
}

async fn run(ctx: Ctx) {
    let mut engine = Engine::new();
    let mut rx = ctx.subscribe();
    let wake = wake_handle(&ctx);
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut woken: HashSet<String> = HashSet::new();
    let mut first_wake: Option<tokio::time::Instant> = None;
    let mut last_wake: Option<tokio::time::Instant> = None;
    let mut all = false;
    let mut n: u64 = 0;
    loop {
        tokio::select! {
            _ = ctx.on_shutdown() => break,
            _ = tick.tick() => {}
            _ = wake.notified() => { all = true; }
            ev = rx.recv() => {
                match ev {
                    Ok(e) => if let Some(t) = wake_type(&e) {
                        woken.insert(t);
                        let at = tokio::time::Instant::now();
                        first_wake.get_or_insert(at);
                        last_wake = Some(at);
                    },
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => all = true,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
                continue;
            }
        }
        let t = now();
        let settled = last_wake.is_some_and(|l| l.elapsed() >= WAKE_QUIET) || first_wake.is_some_and(|f| f.elapsed() >= WAKE_MAX);
        let types = if settled { std::mem::take(&mut woken) } else { HashSet::new() };
        if settled {
            first_wake = None;
            last_wake = None;
        }
        let res = if all {
            all = false;
            let kinds: HashSet<&str> = engine.descriptors().iter().map(|d| d.kind).collect();
            engine.evaluate(&ctx, t, &kinds).await
        } else if n % TICK_S as u64 == 0 || !types.is_empty() {
            if !types.is_empty() {
                let mut kinds: Vec<String> = engine.woken_by(&types).into_iter().map(str::to_owned).collect();
                kinds.sort();
                with_shared(&ctx, |s| {
                    s.woken.runs += 1;
                    s.woken.last_kinds = kinds;
                });
            }
            engine.tick(&ctx, t, &types).await
        } else {
            Ok(Changes::default())
        };
        if let Err(e) = res {
            tracing::warn!("alert engine: {e:#}");
        }
        if n % 2 == 0
            && let Err(e) = routing::route(&ctx, t).await
        {
            tracing::warn!("alert routing: {e:#}");
        }
        n = n.wrapping_add(1);
    }
}
