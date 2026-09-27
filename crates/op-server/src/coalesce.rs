//! The live layer's coalescer: one task per event bus turns the stream of
//! bus events into what `/api/live` sends.
//!
//! At 250 collars the bus carries a fix, a collar and often a cue per report
//! (about 100 events a second), and a sweep step adds two acks and two collar
//! events per collar. Every 500 ms the coalescer sends instead, per herd:
//! - `positions`: the newest fix of each collar that fixed, plus telemetry-only
//!   collar changes (battery, last contact, fence state);
//! - `ack_batch`: each collar's latest ack, plus collar changes that only move
//!   `boundary_version`;
//! - `cue_batch`: every cue.
//!
//! The window is the farm's, not a herd's: it opens with the first event of
//! any herd and closes 500 ms later, and everything it gathered goes out as
//! one message. A window with one herd's `positions` only is that message; a
//! window with more is one `{"type":"batch","events":[…]}` holding them in
//! herd order. So several herds live at once still make at most two batched
//! messages a second.
//!
//! A `collar` event is sent on its own only when the collar's JSON minus
//! [`COLLAR_VOLATILE_KEYS`] changed (renamed, relinked, parked, new fields
//! from other streams all count). Every other event passes straight through,
//! in bus order. Each outgoing message is serialized once and shared by every
//! socket; a socket drops what its identity can't see ([`Event::min_role`]).

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::Utf8Bytes;
use op_core::live::{AckItem, COLLAR_VOLATILE_KEYS, CueItem, PositionItem};
use op_core::{AckStatus, Collar, Event, Role, Store};
use serde::Serialize;
use serde_json::Value;
use tokio::sync::broadcast::{self, WeakSender, error::RecvError};
use tokio::time::Instant;

/// How long events of one herd gather before they go out.
pub const WINDOW: Duration = Duration::from_millis(500);
/// Outgoing messages a socket may fall behind before it is told to resync.
const OUT_CAPACITY: usize = 1024;

/// One outgoing message: its JSON, serialized once, and who may see it.
#[derive(Clone)]
pub struct Out {
    pub role: Role,
    pub text: Utf8Bytes,
}

/// A running coalescer: sockets subscribe here instead of to the bus.
pub struct Hub {
    out: broadcast::Sender<Out>,
    serialized: AtomicU64,
}

impl Hub {
    pub fn subscribe(&self) -> broadcast::Receiver<Out> {
        self.out.subscribe()
    }

    /// Sockets listening now.
    pub fn subscribers(&self) -> usize {
        self.out.receiver_count()
    }

    /// Outgoing messages serialized so far (one per message, whatever the
    /// number of sockets).
    pub fn serialized(&self) -> u64 {
        self.serialized.load(Ordering::Relaxed)
    }

    fn send(&self, ev: &Event) {
        self.send_json(ev.min_role(), ev);
    }

    /// One window's batches as one message per role that may see them: a lone
    /// event as itself, several as a `batch` envelope, in the order given.
    fn send_window(&self, events: Vec<Event>) {
        let mut roles: Vec<Role> = events.iter().map(Event::min_role).collect();
        roles.sort();
        roles.dedup();
        for role in roles {
            let group: Vec<&Event> = events.iter().filter(|e| e.min_role() == role).collect();
            match group.as_slice() {
                [one] => self.send(one),
                many => self.send_json(role, &Envelope { kind: "batch", events: many }),
            }
        }
    }

    fn send_json(&self, role: Role, value: &impl Serialize) {
        if self.out.receiver_count() == 0 {
            return;
        }
        let text = match serde_json::to_string(value) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("live event didn't serialize: {e}");
                return;
            }
        };
        self.serialized.fetch_add(1, Ordering::Relaxed);
        let _ = self.out.send(Out { role, text: text.into() });
    }
}

/// Several batches from one window, as one WebSocket message.
#[derive(Serialize)]
struct Envelope<'a> {
    #[serde(rename = "type")]
    kind: &'static str,
    events: &'a [&'a Event],
}

/// One hub per event bus (per data dir the process has open). The registry
/// holds the bus weakly, so a closed `Ctx` leaves nothing running.
static HUBS: Mutex<Vec<(WeakSender<Event>, Arc<Hub>)>> = Mutex::new(Vec::new());

/// The hub for this context's bus, started on first use.
pub fn hub(ctx: &op_core::Ctx) -> Arc<Hub> {
    let mut hubs = HUBS.lock().unwrap_or_else(|e| e.into_inner());
    hubs.retain(|(bus, _)| bus.upgrade().is_some());
    if let Some((_, hub)) = hubs.iter().find(|(bus, _)| bus.upgrade().is_some_and(|b| b.same_channel(ctx.events()))) {
        return hub.clone();
    }
    let (out, _) = broadcast::channel(OUT_CAPACITY);
    let hub = Arc::new(Hub { out, serialized: AtomicU64::new(0) });
    // Subscribe before returning, so nothing published after this call is missed.
    let rx = ctx.subscribe();
    tokio::spawn(run(rx, ctx.store().clone(), hub.clone()));
    hubs.push((ctx.events().downgrade(), hub.clone()));
    hub
}

/// A collar as last seen on the bus (or in the database at start).
struct Known {
    collar: Collar,
    /// Its JSON minus the volatile keys: what a forwarded `collar` event carries that matters.
    ident: Value,
}

fn ident(c: &Collar) -> Value {
    let mut v = serde_json::to_value(c).unwrap_or(Value::Null);
    if let Value::Object(m) = &mut v {
        for k in COLLAR_VOLATILE_KEYS {
            m.remove(k);
        }
    }
    v
}

/// What one herd gathered in the current window.
#[derive(Default)]
struct Batch {
    positions: BTreeMap<String, PositionItem>,
    acks: BTreeMap<String, AckItem>,
    cues: Vec<CueItem>,
}

struct Coalescer {
    store: Store,
    hub: Arc<Hub>,
    collars: HashMap<String, Known>,
    /// Per herd, in herd order.
    batches: BTreeMap<String, Batch>,
    /// When the farm's open window closes; none while nothing is gathered.
    due: Option<Instant>,
    /// Cues of collars whose herd isn't known yet, placed from the database
    /// when the window ends.
    unplaced: Vec<CueItem>,
}

async fn run(mut rx: broadcast::Receiver<Event>, store: Store, hub: Arc<Hub>) {
    let mut c = Coalescer { store, hub, collars: HashMap::new(), batches: BTreeMap::new(), due: None, unplaced: Vec::new() };
    c.seed().await;
    loop {
        let due = c.due;
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => c.on_event(ev),
                Err(RecvError::Lagged(n)) => {
                    // The batches may be missing events: every client refetches.
                    tracing::warn!("live coalescer lagged, dropped {n} events; sending resync");
                    c.hub.send(&Event::Resync);
                }
                Err(RecvError::Closed) => break,
            },
            _ = sleep_until(due), if due.is_some() => c.flush().await,
        }
    }
}

async fn sleep_until(due: Option<Instant>) {
    if let Some(d) = due {
        tokio::time::sleep_until(d).await;
    }
}

impl Coalescer {
    /// Every collar as stored now, so the first report of each isn't mistaken for a change.
    async fn seed(&mut self) {
        match op_core::live::stored_collars(&self.store).await {
            Ok(list) => {
                for collar in list {
                    self.collars.insert(collar.id.clone(), Known { ident: ident(&collar), collar });
                }
            }
            Err(e) => tracing::warn!("live coalescer couldn't read collars: {e:#}"),
        }
    }

    /// The herd's batch in the farm's window, opening the window if none is.
    fn batch(&mut self, herd_id: &str) -> &mut Batch {
        self.due.get_or_insert_with(|| Instant::now() + WINDOW);
        self.batches.entry(herd_id.to_owned()).or_default()
    }

    fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Fix { collar_id, animal_id, herd_id, fix, state } => {
                let known = self.collars.get(&collar_id).map(|k| (k.collar.battery, k.collar.last_seen));
                let b = self.batch(&herd_id);
                match b.positions.get_mut(&collar_id) {
                    Some(p) if p.fix.at > fix.at => {}
                    Some(p) => {
                        p.fix = fix;
                        p.state = state;
                        if animal_id.is_some() {
                            p.animal_id = animal_id;
                        }
                    }
                    None => {
                        let (battery, last_seen) = known.unwrap_or_default();
                        b.positions.insert(collar_id.clone(), PositionItem { collar_id, animal_id, fix, state, battery, last_seen });
                    }
                }
            }
            Event::Ack { collar_id, herd_id, version, status, reason, code } => {
                self.batch(&herd_id).acks.insert(collar_id.clone(), AckItem { collar_id, version, status, code, reason });
            }
            Event::Cue { collar_id, at, level, margin_m, kind, ring } => {
                let item = CueItem { collar_id, at, level, margin_m, kind, ring };
                match self.collars.get(&item.collar_id).map(|k| k.collar.herd_id.clone()) {
                    Some(herd) => self.batch(&herd).cues.push(item),
                    None => {
                        self.due.get_or_insert_with(|| Instant::now() + WINDOW);
                        self.unplaced.push(item);
                    }
                }
            }
            Event::Collar { collar } => self.on_collar(collar),
            // Built here, never on the bus; pass through if someone publishes one anyway.
            other => self.hub.send(&other),
        }
    }

    fn on_collar(&mut self, collar: Collar) {
        let id = ident(&collar);
        let prev = self.collars.get(&collar.id).filter(|k| k.ident == id).map(|k| k.collar.clone());
        let Some(prev) = prev else {
            // New to us, or something that matters changed: send it whole.
            self.hub.send(&Event::Collar { collar: collar.clone() });
            self.refresh_position(&collar);
            self.collars.insert(collar.id.clone(), Known { ident: id, collar });
            return;
        };
        if collar.boundary_version != prev.boundary_version {
            if let Some(v) = collar.boundary_version {
                let b = self.batch(&collar.herd_id);
                if b.acks.get(&collar.id).is_none_or(|a| a.version != v) {
                    b.acks
                        .insert(collar.id.clone(), AckItem { collar_id: collar.id.clone(), version: v, status: AckStatus::Applied, code: None, reason: None });
                }
            }
        }
        let telemetry = collar.last_seen != prev.last_seen
            || collar.battery != prev.battery
            || collar.last_fix != prev.last_fix
            || collar.state != prev.state
            || collar.outside_since != prev.outside_since;
        if telemetry {
            if let Some(fix) = collar.last_fix.clone() {
                let b = self.batch(&collar.herd_id);
                let p = b.positions.entry(collar.id.clone()).or_insert_with(|| PositionItem {
                    collar_id: collar.id.clone(),
                    animal_id: collar.animal_id.clone(),
                    fix: fix.clone(),
                    state: collar.state,
                    battery: None,
                    last_seen: None,
                });
                if fix.at >= p.fix.at {
                    p.fix = fix;
                    p.state = collar.state;
                }
                p.battery = collar.battery;
                p.last_seen = collar.last_seen;
                p.animal_id = collar.animal_id.clone();
            }
        }
        self.collars.insert(collar.id.clone(), Known { ident: id, collar });
    }

    /// A collar sent whole also brings a pending position up to date, so the
    /// `positions` that follow never carry older telemetry than it did.
    fn refresh_position(&mut self, collar: &Collar) {
        for (herd, b) in self.batches.iter_mut() {
            let Some(p) = b.positions.get_mut(&collar.id) else { continue };
            if *herd != collar.herd_id {
                b.positions.remove(&collar.id);
                continue;
            }
            if let Some(fix) = collar.last_fix.as_ref().filter(|f| f.at >= p.fix.at) {
                p.fix = fix.clone();
                p.state = collar.state;
            }
            p.battery = collar.battery;
            p.last_seen = collar.last_seen;
            p.animal_id = collar.animal_id.clone();
        }
    }

    /// The window closed: every herd's batches, as one message.
    async fn flush(&mut self) {
        self.due = None;
        if !self.unplaced.is_empty() {
            self.place_cues().await;
        }
        let mut out = Vec::new();
        for (herd_id, b) in std::mem::take(&mut self.batches) {
            if !b.positions.is_empty() {
                out.push(Event::Positions { herd_id: herd_id.clone(), items: b.positions.into_values().collect() });
            }
            if !b.acks.is_empty() {
                out.push(Event::AckBatch { herd_id: herd_id.clone(), items: b.acks.into_values().collect() });
            }
            if !b.cues.is_empty() {
                out.push(Event::CueBatch { herd_id, items: b.cues });
            }
        }
        if !out.is_empty() {
            self.hub.send_window(out);
        }
    }

    /// Cues from collars the coalescer hasn't seen: their herd from the
    /// database. They go out with that herd's batch in this window.
    async fn place_cues(&mut self) {
        let mut herds: HashMap<String, Option<String>> = HashMap::new();
        for item in std::mem::take(&mut self.unplaced) {
            if !herds.contains_key(&item.collar_id) {
                let herd = match op_core::live::collar_herd(&self.store, &item.collar_id).await {
                    Ok(h) => h,
                    Err(e) => {
                        tracing::warn!("live coalescer couldn't place a cue: {e:#}");
                        None
                    }
                };
                herds.insert(item.collar_id.clone(), herd);
            }
            // A collar that no longer exists: its cue has nowhere to go.
            if let Some(Some(herd)) = herds.get(&item.collar_id).cloned() {
                self.batches.entry(herd).or_default().cues.push(item);
            }
        }
    }
}
