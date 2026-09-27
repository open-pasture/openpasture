//! The collar firmware's protocol side, without I/O: what it holds (slots,
//! from op-protocol's reference `SlotStore`), its config, what it tells the
//! server about itself, and the acks it owes.
//!
//! Three profiles: `v0` and `v1` run firmware 0.2 (protocol v1: holes, slots,
//! collar-scoped boundaries, cue kinds, episodes, config) with V0 or V1
//! limits; `legacy` runs firmware 0.1: one ring of up to 64 corners, one
//! staged slot, no `device` block, no slot list, no reject codes.
//!
//! Time: staged boundaries take effect on the collar's own GNSS clock (every
//! fix), so a collar that can't reach the server keeps applying what it
//! holds and owes the acks until it can deliver them.

use chrono::{DateTime, Utc};
use op_protocol::{
    Ack, AckStatus, BoundaryCommand, CollarLimits, ConfigCommand, ConfigReject, DeviceInfo, RejectCode, SlotReport, SlotStore, VerifyingKey, caps,
};
use serde_json::Value;

/// Firmware version the 0.2 profiles report.
pub const FW: &str = "0.2.0";

/// Which collar a simulated one is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Profile {
    /// Firmware 0.2 on an nRF9151, slots in internal flash.
    V0,
    /// Firmware 0.2 with NOR flash for slots.
    V1,
    /// Firmware 0.1: no caps.
    Legacy,
}

impl Profile {
    pub fn limits(self) -> CollarLimits {
        match self {
            Self::V0 => CollarLimits::V0,
            Self::V1 => CollarLimits::V1,
            Self::Legacy => CollarLimits::LEGACY,
        }
    }

    pub fn is_legacy(self) -> bool {
        self == Self::Legacy
    }

    fn caps(self) -> Vec<String> {
        if self.is_legacy() { vec![] } else { caps::ALL.iter().map(|c| c.to_string()).collect() }
    }
}

/// Where the collar reports: the provisioned endpoint, or one a config
/// moved it to, with the previous one kept for 24 hours in case the new one
/// never answers.
#[derive(Debug, Clone, PartialEq)]
pub struct Endpoint {
    pub current: String,
    previous: Option<(String, DateTime<Utc>)>,
}

/// How long a new endpoint may fail before the collar goes back to the old one.
const ENDPOINT_FALLBACK: chrono::Duration = chrono::Duration::hours(24);

impl Endpoint {
    pub fn new(url: impl Into<String>) -> Self {
        Self { current: url.into(), previous: None }
    }

    /// A report to the current endpoint answered: it stays.
    pub fn succeeded(&mut self) {
        self.previous = None;
    }

    /// A report failed at `now`: after 24 h of that on a new endpoint, go back.
    pub fn failed(&mut self, now: DateTime<Utc>) -> bool {
        match self.previous.take() {
            Some((old, since)) if now - since >= ENDPOINT_FALLBACK => {
                self.current = old;
                true
            }
            p => {
                self.previous = p;
                false
            }
        }
    }

    fn switch(&mut self, url: &str, now: DateTime<Utc>) {
        if url != self.current {
            let old = std::mem::replace(&mut self.current, url.to_owned());
            self.previous = Some((old, now));
        }
    }
}

pub struct Firmware {
    pub profile: Profile,
    collar_id: String,
    store: SlotStore,
    config: Option<ConfigCommand>,
    /// A refused config, reported once.
    config_reject: Option<ConfigReject>,
    /// Acks not yet delivered, oldest first (NVS ID 2 on the collar).
    acks: Vec<Ack>,
    pub endpoint: Endpoint,
}

impl Firmware {
    pub fn new(profile: Profile, herd_id: &str, collar_id: &str, endpoint: &str) -> Self {
        // Firmware 0.1 knows its herd but not its own id.
        let own = (!profile.is_legacy()).then(|| collar_id.to_owned());
        Self {
            profile,
            collar_id: collar_id.to_owned(),
            store: SlotStore::new(profile.limits(), Some(herd_id.to_owned()), own),
            config: None,
            config_reject: None,
            acks: Vec::new(),
            endpoint: Endpoint::new(endpoint),
        }
    }

    /// The boundary being enforced.
    pub fn active(&self) -> Option<&BoundaryCommand> {
        self.store.active().map(|s| &s.cmd)
    }

    pub fn active_version(&self) -> Option<u32> {
        self.active().map(|c| c.version)
    }

    /// Highest version held (`have=`).
    pub fn have(&self) -> u32 {
        self.store.have()
    }

    #[cfg(test)]
    pub fn staged(&self) -> Vec<u32> {
        self.store.staged().iter().map(|s| s.cmd.version).collect()
    }

    /// The download query: `have`, and for firmware 0.2 `free` and `free_bytes`.
    pub fn boundary_query(&self) -> String {
        if self.profile.is_legacy() {
            return format!("have={}", self.have());
        }
        let mut q = format!("have={}&free={}", self.have(), self.store.free());
        if let Some(b) = self.store.free_bytes() {
            q.push_str(&format!("&free_bytes={b}"));
        }
        q
    }

    /// A downloaded command as the collar gets it (raw bytes). Checks the
    /// signature and the wire form, then offers it to the slots. Returns the
    /// ack (also queued), or `None` for bytes that don't even name a command.
    pub fn receive(&mut self, wire: &[u8], key: &VerifyingKey, modem_now: DateTime<Utc>) -> Option<Ack> {
        let ack = match op_protocol::verify_wire(wire, key) {
            Ok(cmd) => self.store.insert(cmd, None).to_ack(modem_now),
            Err(code) => {
                // Refused before parsing: name it from the raw JSON if possible.
                let v: Value = serde_json::from_slice(wire).ok()?;
                Ack {
                    command_id: v.get("command_id")?.as_str()?.to_owned(),
                    version: u32::try_from(v.get("version")?.as_u64()?).ok()?,
                    status: AckStatus::Rejected,
                    code: Some(code),
                    reason: Some(code.message().to_owned()),
                    at: modem_now,
                    ..Default::default()
                }
            }
        };
        Some(self.owe(ack))
    }

    /// A fix at GNSS time `now`: a staged boundary whose time has come applies.
    pub fn tick(&mut self, now: DateTime<Utc>) -> Option<Ack> {
        let a = self.store.tick(now)?;
        Some(self.owe(a.to_ack(now)))
    }

    fn owe(&mut self, mut ack: Ack) -> Ack {
        ack.collar_id = Some(self.collar_id.clone());
        if self.profile.is_legacy() {
            // Firmware 0.1 has no reject codes, only a reason.
            ack.code = None;
        }
        self.acks.push(ack.clone());
        ack
    }

    /// Acks owed, oldest first.
    pub fn pending_acks(&self) -> &[Ack] {
        &self.acks
    }

    /// The oldest owed ack reached the server (or it will never take it).
    pub fn ack_done(&mut self) {
        if !self.acks.is_empty() {
            self.acks.remove(0);
        }
    }

    /// A config from a report reply (its JSON). Checked like a boundary; a
    /// refused one is reported once in the next `device` block.
    pub fn apply_config(&mut self, raw: &Value, key: &VerifyingKey, now: DateTime<Utc>) -> Result<&ConfigCommand, RejectCode> {
        if !self.store_takes_config() {
            return Err(RejectCode::BadConfig);
        }
        let result = op_protocol::verify_config_wire(raw.to_string().as_bytes(), key)
            .and_then(|cmd| cmd.check(&self.collar_id, self.config.as_ref().map(|c| c.version)).map(|_| cmd));
        match result {
            Ok(cmd) => {
                self.store.set_herd(cmd.herd_id.clone());
                if let Some(e) = &cmd.endpoint {
                    self.endpoint.switch(e, now);
                }
                Ok(self.config.insert(cmd))
            }
            Err(code) => {
                let version = raw.get("version").and_then(Value::as_u64).and_then(|v| u32::try_from(v).ok()).unwrap_or(0);
                self.config_reject = Some(ConfigReject { version, code: Some(code) });
                Err(code)
            }
        }
    }

    fn store_takes_config(&self) -> bool {
        !self.profile.is_legacy()
    }

    pub fn config(&self) -> Option<&ConfigCommand> {
        self.config.as_ref()
    }

    /// `(report_s, poll_s)` at GNSS time `now`, once a config has arrived.
    pub fn cadence(&self, now: DateTime<Utc>) -> Option<(u32, u32)> {
        self.config.as_ref().map(|c| c.cadence(now))
    }

    /// What the collar says about itself in a report (firmware 0.2 only),
    /// with a refused config until a report carrying it got through.
    pub fn device(&self) -> Option<DeviceInfo> {
        if self.profile.is_legacy() {
            return None;
        }
        Some(DeviceInfo {
            fw: Some(FW.into()),
            caps: self.profile.caps(),
            limits: Some(self.profile.limits()),
            config_version: self.config.as_ref().map(|c| c.version),
            config_reject: self.config_reject.clone(),
        })
    }

    /// A report got through: a refusal it carried is said.
    pub fn reported(&mut self, device: Option<&DeviceInfo>) {
        if device.and_then(|d| d.config_reject.as_ref()) == self.config_reject.as_ref() {
            self.config_reject = None;
        }
    }

    /// Every boundary held (firmware 0.2 only; 0.1 doesn't say).
    pub fn slots(&self) -> Option<Vec<SlotReport>> {
        (!self.profile.is_legacy()).then(|| self.store.report())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use op_geo::Polygon;
    use op_geo::projection::Projection;

    const O: [f64; 2] = [-93.6225, 42.0318];

    fn t0() -> DateTime<Utc> {
        op_protocol::wire_time::parse("2026-09-27T12:00:00Z").unwrap()
    }

    fn key() -> op_protocol::SigningKey {
        op_protocol::SigningKey::from_bytes(&[3; 32])
    }

    fn ring(e: f64, n: f64, w: f64, h: f64) -> Vec<[f64; 2]> {
        let p = Projection::new(O);
        vec![p.offset(e, n), p.offset(e + w, n), p.offset(e + w, n + h), p.offset(e, n + h)]
    }

    /// Signed wire bytes, as the server sends them.
    fn wire(version: u32, holes: Vec<Vec<[f64; 2]>>, at: Option<DateTime<Utc>>, collar: Option<&str>) -> Vec<u8> {
        let poly = Polygon::from_rings(ring(0.0, 0.0, 200.0, 150.0), holes);
        let mut c = BoundaryCommand::from_shape(format!("bnd_{version}"), version, &poly, at).unwrap();
        c.herd_id = Some("herd_1".into());
        c.collar_id = collar.map(str::to_owned);
        c.warn_m = Some(5.0);
        c.hysteresis_m = Some(1.0);
        op_protocol::sign_command(&mut c, &key());
        serde_json::to_vec(&c).unwrap()
    }

    fn fw(p: Profile) -> Firmware {
        Firmware::new(p, "herd_1", "col_a", "https://a.example.com/collar/v1")
    }

    #[test]
    fn profiles_say_what_they_are() {
        let v0 = fw(Profile::V0);
        let d = v0.device().unwrap();
        assert_eq!((d.fw.as_deref(), d.limits, d.caps.len()), (Some(FW), Some(CollarLimits::V0), 6));
        assert_eq!(fw(Profile::V1).device().unwrap().limits, Some(CollarLimits::V1));
        let legacy = fw(Profile::Legacy);
        assert!(legacy.device().is_none() && legacy.slots().is_none());
        assert_eq!(legacy.boundary_query(), "have=0");
        assert_eq!(v0.boundary_query(), "have=0&free=16&free_bytes=24576");
        assert_eq!(v0.slots(), Some(vec![]));
    }

    #[test]
    fn slots_by_count_and_bytes_and_offline_activation() {
        let mut f = fw(Profile::V0);
        let k = key().verifying_key();
        let a = f.receive(&wire(1, vec![], None, None), &k, t0()).unwrap();
        assert_eq!(a.status, AckStatus::Applied);
        for (i, v) in [2, 3, 4].into_iter().enumerate() {
            let a = f.receive(&wire(v, vec![], Some(t0() + Duration::minutes(2 * (i as i64 + 1))), None), &k, t0()).unwrap();
            assert_eq!(a.status, AckStatus::Received);
        }
        assert_eq!((f.have(), f.staged()), (4, vec![2, 3, 4]));
        assert_eq!(f.boundary_query(), format!("have=4&free=12&free_bytes={}", 24_576 - 4 * (192 + 32)));
        let held: Vec<(u32, AckStatus)> = f.slots().unwrap().iter().map(|s| (s.version, s.status)).collect();
        assert_eq!(held, [(1, AckStatus::Applied), (2, AckStatus::Received), (3, AckStatus::Received), (4, AckStatus::Received)]);
        // No server: fixes alone apply them at their times.
        assert!(f.tick(t0() + Duration::seconds(119)).is_none());
        let a = f.tick(t0() + Duration::seconds(121)).unwrap();
        assert_eq!((a.version, a.status, a.at), (2, AckStatus::Applied, t0() + Duration::seconds(121)));
        // Past both others at once: the highest applies, the other is dropped.
        let a = f.tick(t0() + Duration::minutes(7)).unwrap();
        assert_eq!((a.version, f.active_version(), f.staged()), (4, Some(4), vec![]));
        // Every ack is owed until delivered, in order.
        let owed: Vec<(u32, AckStatus)> = f.pending_acks().iter().map(|a| (a.version, a.status)).collect();
        assert_eq!(
            owed,
            [
                (1, AckStatus::Applied),
                (2, AckStatus::Received),
                (3, AckStatus::Received),
                (4, AckStatus::Received),
                (2, AckStatus::Applied),
                (4, AckStatus::Applied)
            ]
        );
        assert!(f.pending_acks().iter().all(|a| a.collar_id.as_deref() == Some("col_a")));
        f.ack_done();
        assert_eq!(f.pending_acks().len(), 5);
        // Legacy holds one staged slot: a second is refused, without a code.
        let mut l = fw(Profile::Legacy);
        l.receive(&wire(1, vec![], None, None), &k, t0());
        l.receive(&wire(2, vec![], Some(t0() + Duration::minutes(2)), None), &k, t0());
        let a = l.receive(&wire(3, vec![], Some(t0() + Duration::minutes(4)), None), &k, t0()).unwrap();
        assert_eq!((a.status, a.code), (AckStatus::Rejected, None));
        assert!(a.reason.is_some());
        // Bytes: a V0 collar full by bytes refuses a big staged record.
        let mut tight = Firmware::new(Profile::V0, "herd_1", "col_a", "https://a.example.com");
        tight.store = SlotStore::new(CollarLimits { slot_bytes: 400, ..CollarLimits::V0 }, Some("herd_1".into()), Some("col_a".into()));
        tight.receive(&wire(1, vec![], None, None), &k, t0());
        let a = tight.receive(&wire(2, vec![], Some(t0() + Duration::minutes(2)), None), &k, t0()).unwrap();
        assert_eq!((a.status, a.code), (AckStatus::Rejected, Some(RejectCode::SlotsFull)));
    }

    #[test]
    fn holes_collar_ids_and_signatures() {
        let k = key().verifying_key();
        let pond = ring(80.0, 60.0, 30.0, 30.0);
        let mut v0 = fw(Profile::V0);
        assert_eq!(v0.receive(&wire(1, vec![pond.clone()], None, Some("col_a")), &k, t0()).unwrap().status, AckStatus::Applied);
        assert_eq!(v0.active().unwrap().holes.len(), 1);
        let other = v0.receive(&wire(2, vec![], None, Some("col_b")), &k, t0()).unwrap();
        assert_eq!(other.code, Some(RejectCode::WrongCollar));
        // A legacy collar can't hold the hole.
        let mut l = fw(Profile::Legacy);
        assert_eq!(l.receive(&wire(1, vec![pond], None, None), &k, t0()).unwrap().status, AckStatus::Rejected);
        // Anything not signed by our server.
        let mut bad = wire(3, vec![], None, None);
        let at = bad.iter().rposition(|b| *b == b'5').unwrap();
        bad[at] = b'6';
        let a = v0.receive(&bad, &k, t0()).unwrap();
        assert_eq!((a.version, a.code), (3, Some(RejectCode::BadSig)));
    }

    fn config(version: u32, herd: &str, fast_until: Option<DateTime<Utc>>) -> Value {
        let mut c = ConfigCommand {
            command_id: format!("cfg_{version}"),
            collar_id: "col_a".into(),
            version,
            herd_id: Some(herd.into()),
            endpoint: None,
            report_s: 60,
            poll_s: 60,
            fast_report_s: fast_until.map(|_| 10),
            fast_poll_s: fast_until.map(|_| 10),
            fast_until,
            sig: None,
        };
        op_protocol::sign_config(&mut c, &key());
        serde_json::to_value(&c).unwrap()
    }

    #[test]
    fn config_sets_cadence_fast_mode_and_herd() {
        let k = key().verifying_key();
        let mut f = fw(Profile::V0);
        assert_eq!(f.cadence(t0()), None, "until a config arrives");
        f.apply_config(&config(1, "herd_1", None), &k, t0()).unwrap();
        assert_eq!(f.cadence(t0()), Some((60, 60)));
        let until = t0() + Duration::minutes(40);
        f.apply_config(&config(2, "herd_1", Some(until)), &k, t0()).unwrap();
        assert_eq!(f.cadence(t0() + Duration::minutes(39)), Some((10, 10)));
        assert_eq!(f.cadence(until), Some((60, 60)), "fast mode ends on GNSS time alone");
        assert_eq!(f.device().unwrap().config_version, Some(2));
        // Stale: refused, and said until a report carrying it gets through.
        assert_eq!(f.apply_config(&config(2, "herd_1", None), &k, t0()).unwrap_err(), RejectCode::Stale);
        let d = f.device().unwrap();
        assert_eq!(d.config_reject, Some(ConfigReject { version: 2, code: Some(RejectCode::Stale) }));
        assert!(f.device().unwrap().config_reject.is_some(), "that report failed");
        f.reported(Some(&d));
        assert!(f.device().unwrap().config_reject.is_none());
        // A new herd: its boundaries are taken, the old herd's refused.
        f.apply_config(&config(3, "herd_2", None), &k, t0()).unwrap();
        let mut c = BoundaryCommand::from_shape("bnd_9", 9, &Polygon::from_ring(ring(0.0, 0.0, 50.0, 50.0)), None).unwrap();
        c.herd_id = Some("herd_2".into());
        op_protocol::sign_command(&mut c, &key());
        assert_eq!(f.receive(&serde_json::to_vec(&c).unwrap(), &k, t0()).unwrap().status, AckStatus::Applied);
        assert_eq!(f.receive(&wire(10, vec![], None, None), &k, t0()).unwrap().code, Some(RejectCode::WrongHerd));
        // A legacy collar takes no config.
        assert!(fw(Profile::Legacy).apply_config(&config(1, "herd_1", None), &k, t0()).is_err());
    }

    #[test]
    fn a_new_endpoint_falls_back_after_24_hours_of_failures() {
        let k = key().verifying_key();
        let mut f = fw(Profile::V0);
        let mut c: ConfigCommand = serde_json::from_value(config(1, "herd_1", None)).unwrap();
        c.endpoint = Some("https://b.example.com/collar/v1".into());
        op_protocol::sign_config(&mut c, &key());
        f.apply_config(&serde_json::to_value(&c).unwrap(), &k, t0()).unwrap();
        assert_eq!(f.endpoint.current, "https://b.example.com/collar/v1");
        assert!(!f.endpoint.failed(t0() + Duration::hours(23)));
        assert!(f.endpoint.failed(t0() + Duration::hours(24)));
        assert_eq!(f.endpoint.current, "https://a.example.com/collar/v1");
        // One that answers stays.
        let mut e = Endpoint::new("https://a");
        e.switch("https://b", t0());
        e.succeeded();
        assert!(!e.failed(t0() + Duration::days(3)));
        assert_eq!(e.current, "https://b");
    }
}
