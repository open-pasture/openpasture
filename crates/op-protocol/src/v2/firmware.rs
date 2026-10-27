//! Firmware releases (`opencollar/protocol/v2/firmware.md`): the release
//! manifest (message type 65, signed by a release or image key), the rules a
//! collar applies before it installs one and after it boots one, and trust
//! updates (type 68) that change which firmware keys the bootloader trusts.

use ed25519_dalek::SigningKey;
use serde_json::{Value as Json, json};

use super::cbor::{self, IntMap, Value};
use super::cose::{self, Sign1};
use super::crypto::kid;
use super::json::hex;
use super::registry::{DeviceClass, FenceState, MessageType, Reject};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    /// 16: semver.
    pub version: String,
    /// 17: SHA-256 of the whole signed image file.
    pub digest: [u8; 32],
    /// 18: bytes.
    pub size: u64,
    /// 19: device classes it runs on.
    pub classes: Vec<DeviceClass>,
    /// 20: board revisions it runs on, exactly.
    pub hw_revs: Vec<String>,
    /// 21: anti-rollback counter; a collar refuses one below its own.
    pub security_counter: u32,
    /// 22: least battery to start the install, percent.
    pub min_battery: Option<u8>,
    /// 23: kids of firmware keys to stop trusting.
    pub revoke: Vec<[u8; 8]>,
}

impl Manifest {
    pub fn payload(&self) -> Vec<u8> {
        let mut m = IntMap::new();
        m.put(0, Value::uint(MessageType::Manifest.code()))
            .put(16, Value::text(self.version.clone()))
            .put(17, Value::bytes(self.digest.to_vec()))
            .put(18, Value::uint(self.size))
            .put(19, Value::Array(self.classes.iter().map(|c| Value::uint(c.code())).collect()))
            .put(20, Value::Array(self.hw_revs.iter().cloned().map(Value::Text).collect()))
            .put(21, Value::uint(self.security_counter))
            .put_opt(22, self.min_battery.map(Value::uint));
        if !self.revoke.is_empty() {
            m.put(23, Value::Array(self.revoke.iter().map(|k| Value::bytes(k.to_vec())).collect()));
        }
        cbor::encode(&m.into_value())
    }

    pub fn sign(&self, release_key: &SigningKey) -> Vec<u8> {
        cose::sign(&self.payload(), release_key)
    }

    /// Read a signed manifest (check the envelope with [`Sign1::verify`]
    /// against the trusted firmware key named by its kid).
    pub fn read(bytes: &[u8]) -> Result<(Sign1, Self), Reject> {
        let env = Sign1::parse(bytes)?;
        let m = env.payload_map()?;
        let bad = || Reject::BadCbor;
        if m.get(0).and_then(Value::as_u64) != Some(MessageType::Manifest.code()) {
            return Err(bad());
        }
        // Every known field that is present must read: a malformed one is
        // refused, never taken as absent (Astra 01 F14: `min_battery: "90"`
        // was read as no minimum).
        let list = |k| m.get(k).and_then(Value::as_array).ok_or_else(bad);
        let man = Self {
            version: m.get(16).and_then(Value::as_text).filter(|v| (1..=32).contains(&v.len())).ok_or_else(bad)?.to_owned(),
            digest: m.get(17).and_then(Value::as_bytes).and_then(|d| d.try_into().ok()).ok_or_else(bad)?,
            size: m.get(18).and_then(Value::as_u64).ok_or_else(bad)?,
            classes: list(19)?.iter().map(|c| c.as_u64().map(DeviceClass::from_code)).collect::<Option<_>>().ok_or_else(bad)?,
            hw_revs: list(20)?.iter().map(|h| h.as_text().filter(|t| t.len() <= 16).map(str::to_owned)).collect::<Option<_>>().ok_or_else(bad)?,
            security_counter: m.get(21).and_then(Value::as_u32).ok_or_else(bad)?,
            min_battery: m.get(22).map(|b| b.as_u64().filter(|b| *b <= 100).map(|b| b as u8).ok_or_else(bad)).transpose()?,
            revoke: match m.get(23) {
                None => Vec::new(),
                Some(v) => v.as_array().and_then(|a| a.iter().map(|k| k.as_bytes()?.try_into().ok()).collect::<Option<_>>()).ok_or_else(bad)?,
            },
        };
        Ok((env, man))
    }

    pub fn to_json(&self) -> Json {
        let mut o = json!({
            "type": "manifest",
            "version": self.version,
            "digest": hex(&self.digest),
            "size": self.size,
            "classes": self.classes.iter().map(|c| c.to_json()).collect::<Vec<_>>(),
            "hw_revs": self.hw_revs,
            "security_counter": self.security_counter,
        });
        if let Some(b) = self.min_battery {
            o["min_battery"] = json!(b);
        }
        if !self.revoke.is_empty() {
            o["revoke"] = json!(self.revoke.iter().map(|k| hex(k)).collect::<Vec<_>>());
        }
        o
    }
}

/// What a collar knows, from its own state, when it decides whether to
/// install now. Nothing here is taken from a server: the collar reads its
/// own clock, fix, fence, slots and config (Astra 01 F09).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstallState {
    pub class: DeviceClass,
    pub hw_rev: String,
    pub security_counter: u32,
    /// Percent; `None` when unknown (then it doesn't install).
    pub battery: Option<u8>,
    /// GNSS time now; `None` until the collar has trustworthy time.
    pub now: Option<u64>,
    /// GNSS time of its last fix.
    pub last_fix: Option<u64>,
    /// What its fence made of that fix.
    pub fence: Option<FenceState>,
    /// An escape or warning episode is open.
    pub episode: bool,
    /// It holds a collar-scoped boundary (a pen).
    pub pen: bool,
    /// When its fast window ends (config `fast_until`), if one was set.
    pub fast_until: Option<u64>,
    /// `effective_at` of every staged boundary it holds (including ones
    /// waiting for zones or a lease).
    pub staged: Vec<u64>,
}

/// When a collar may install: the command's `effective_at` plus the
/// collar's own random offset within `window_s`, drawn once per command and
/// kept in flash.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct InstallWindow {
    pub effective_at: Option<u64>,
    pub offset_s: u64,
}

impl InstallWindow {
    /// The collar's offset: `random` (any 32 random bits) folded into
    /// `0..window_s`.
    pub fn new(effective_at: Option<u64>, window_s: Option<u32>, random: u32) -> Self {
        Self { effective_at, offset_s: window_s.filter(|w| *w > 0).map_or(0, |w| u64::from(random % w)) }
    }

    /// The earliest GNSS time it may install.
    pub fn opens_at(&self) -> u64 {
        self.effective_at.unwrap_or(0).saturating_add(self.offset_s)
    }
}

/// The collar's own floor for installing, whatever the manifest says.
pub const MIN_BATTERY_FLOOR: u8 = 40;
/// A fix older than this can't say the animal is inside, seconds.
pub const FIX_FRESH_S: u64 = 600;
/// No staged boundary may take effect this soon after an install starts.
pub const MOVE_CLEAR_S: u64 = 3600;

/// Whether to install now, in order: `incompatible` (class or board),
/// `downgrade` (counter below the collar's), then `not_now` when any of these
/// holds, all read from the collar's own state: no GNSS time; before its
/// window opens; battery unknown or under the manifest's minimum or the
/// floor; no fix in the last 10 minutes; the fence not `inside`; an episode
/// open; a pen held; a fast window running; a staged boundary due within the
/// hour (or overdue, still waiting). `not_now` is not a refusal: the collar
/// keeps the update and asks again, and again right before it reboots.
pub fn check_install(m: &Manifest, s: &InstallState, w: &InstallWindow) -> Result<(), Reject> {
    if !m.classes.contains(&s.class) || !m.hw_revs.contains(&s.hw_rev) {
        return Err(Reject::Incompatible);
    }
    if m.security_counter < s.security_counter {
        return Err(Reject::Downgrade);
    }
    let Some(now) = s.now else { return Err(Reject::NotNow) };
    let need = m.min_battery.unwrap_or(0).max(MIN_BATTERY_FLOOR);
    let fresh_fix = s.last_fix.is_some_and(|f| f <= now && now - f <= FIX_FRESH_S);
    let moving = s.fast_until.is_some_and(|f| f > now) || s.staged.iter().any(|e| *e < now.saturating_add(MOVE_CLEAR_S));
    let safe =
        now >= w.opens_at() && s.battery.is_some_and(|b| b >= need) && fresh_fix && s.fence == Some(FenceState::Inside) && !s.episode && !s.pen && !moving;
    if safe { Ok(()) } else { Err(Reject::NotNow) }
}

/// How a test boot of a new image ended (`firmware.md` §On the collar, 6–7).
/// Only local evidence counts: a link outage never condemns an image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TestBoot {
    /// Self-test passed: the active boundary loaded and enforcing, flash and
    /// counters readable, the GNSS receiver answering and a fix taken.
    Passed,
    /// It crashed, the watchdog fired, or the self-test failed.
    Failed,
    /// Everything passed but no fix came in 30 minutes (a barn, a gully).
    NoFix,
}

/// Rollbacks for want of a fix before the image is refused.
pub const NO_FIX_TRIES: u8 = 3;

/// What follows a test boot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterTest {
    /// Mark the image confirmed; ack the command `applied`.
    Confirm,
    /// Reboot into the previous image. `refuse`: report `rolled_back` and
    /// never take that digest again; otherwise report `staged` with
    /// `not_now` and try again later.
    RollBack { refuse: bool },
}

/// `no_fix_before`: how many earlier test boots of this digest ended `NoFix`.
pub fn after_test(t: TestBoot, no_fix_before: u8) -> AfterTest {
    match t {
        TestBoot::Passed => AfterTest::Confirm,
        TestBoot::Failed => AfterTest::RollBack { refuse: true },
        TestBoot::NoFix => AfterTest::RollBack { refuse: no_fix_before + 1 >= NO_FIX_TRIES },
    }
}

// ---------------------------------------------------------------- trust updates

/// A trust update (message type 68), signed by a firmware key the
/// bootloader trusts: firmware keys to stop trusting and keys to trust from
/// now on, under an increasing counter. Carried to a collar in a server's
/// `trust` command (type 14); the bootloader applies it on the first boot
/// after the image running it is confirmed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustUpdate {
    /// 16: above every trust update applied before (anti-rollback).
    pub counter: u32,
    /// 17: kids to stop trusting.
    pub revoke: Vec<[u8; 8]>,
    /// 18: public keys to trust, as OpenCollar release keys.
    pub add: Vec<[u8; 32]>,
}

impl TrustUpdate {
    pub fn payload(&self) -> Vec<u8> {
        let mut m = IntMap::new();
        m.put(0, Value::uint(MessageType::TrustUpdate.code())).put(16, Value::uint(self.counter));
        if !self.revoke.is_empty() {
            m.put(17, Value::Array(self.revoke.iter().map(|k| Value::bytes(k.to_vec())).collect()));
        }
        if !self.add.is_empty() {
            m.put(18, Value::Array(self.add.iter().map(|k| Value::bytes(k.to_vec())).collect()));
        }
        cbor::encode(&m.into_value())
    }

    pub fn sign(&self, key: &SigningKey) -> Vec<u8> {
        cose::sign(&self.payload(), key)
    }

    /// Read a signed update (check the signature with [`Sign1::verify`]).
    pub fn read(bytes: &[u8]) -> Result<(Sign1, Self), Reject> {
        let env = Sign1::parse(bytes)?;
        let m = env.payload_map()?;
        let bad = || Reject::BadCbor;
        if m.get(0).and_then(Value::as_u64) != Some(MessageType::TrustUpdate.code()) {
            return Err(bad());
        }
        let list = |k: u64, n: usize| -> Result<Vec<Vec<u8>>, Reject> {
            match m.get(k) {
                None => Ok(Vec::new()),
                Some(v) => v.as_array().and_then(|a| a.iter().map(|x| x.as_bytes().filter(|b| b.len() == n).map(<[u8]>::to_vec)).collect()).ok_or_else(bad),
            }
        };
        Ok((
            env,
            Self {
                counter: m.get(16).and_then(Value::as_u32).filter(|c| *c >= 1).ok_or_else(bad)?,
                revoke: list(17, 8)?.into_iter().map(|k| k.try_into().unwrap()).collect(),
                add: list(18, 32)?.into_iter().map(|k| k.try_into().unwrap()).collect(),
            },
        ))
    }

    pub fn to_json(&self) -> Json {
        json!({
            "type": "trust_update",
            "counter": self.counter,
            "revoke": self.revoke.iter().map(|k| hex(k)).collect::<Vec<_>>(),
            "add": self.add.iter().map(|k| hex(k)).collect::<Vec<_>>(),
        })
    }
}

/// Who put a key in the trust store.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// An OpenCollar release key: built into the bootloader at manufacture,
    /// or added by a trust update.
    Release,
    /// An owner's or builder's image key, enrolled physically.
    Owner,
}

/// Most keys a bootloader trusts at once (built-in and added).
pub const TRUST_SLOTS: usize = 4;

/// The bootloader's trust store (`firmware.md` §Boot profile): its keys,
/// which are revoked, and the counter of the last trust update. On the
/// nRF52840 it lives in a flash page only the bootloader can write (ACL),
/// two copies with generations like [`super::counter::Journal`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustStore {
    pub keys: Vec<([u8; 32], Origin)>,
    pub revoked: Vec<[u8; 8]>,
    pub counter: u32,
}

impl TrustStore {
    /// The kids it trusts now.
    pub fn trusted(&self) -> Vec<[u8; 8]> {
        self.keys.iter().map(|(k, _)| kid(k)).filter(|k| !self.revoked.contains(k)).collect()
    }

    /// Apply an update whose signature by `signer` already verified, as the
    /// bootloader does: the signer must be trusted (`unknown_kid`), the
    /// counter above the last (`stale`), the keys must fit (`bad_config`),
    /// and at least one release key must remain trusted unless an owner key
    /// is enrolled (`bad_config`): no update can leave a collar with no way
    /// to take a release.
    pub fn apply(&mut self, signer: &[u8; 8], u: &TrustUpdate) -> Result<(), Reject> {
        if !self.trusted().contains(signer) {
            return Err(Reject::UnknownKid);
        }
        if u.counter <= self.counter {
            return Err(Reject::Stale);
        }
        let mut next = self.clone();
        next.revoked.extend(u.revoke.iter().filter(|k| !self.revoked.contains(k)));
        for k in &u.add {
            if !next.keys.iter().any(|(have, _)| have == k) {
                next.keys.push((*k, Origin::Release));
            }
        }
        // Revoked keys give up their slots.
        let live = next.keys.iter().filter(|(k, _)| !next.revoked.contains(&kid(k))).count();
        let release_left = next.keys.iter().any(|(k, o)| *o == Origin::Release && !next.revoked.contains(&kid(k)));
        let owner_left = next.keys.iter().any(|(k, o)| *o == Origin::Owner && !next.revoked.contains(&kid(k)));
        if live > TRUST_SLOTS || !(release_left || owner_left) {
            return Err(Reject::BadConfig);
        }
        next.counter = u.counter;
        *self = next;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest() -> Manifest {
        Manifest {
            version: "2.1.0".into(),
            digest: [3; 32],
            size: 210_000,
            classes: vec![DeviceClass::Node],
            hw_revs: vec!["N1.0".into()],
            security_counter: 2,
            min_battery: Some(50),
            revoke: vec![],
        }
    }

    fn calm() -> InstallState {
        InstallState {
            class: DeviceClass::Node,
            hw_rev: "N1.0".into(),
            security_counter: 2,
            battery: Some(80),
            now: Some(10_000),
            last_fix: Some(9_940),
            fence: Some(FenceState::Inside),
            ..Default::default()
        }
    }

    #[test]
    fn install_rules_in_order() {
        let w = InstallWindow::default();
        let ok = calm();
        check_install(&manifest(), &ok, &w).unwrap();
        assert_eq!(check_install(&manifest(), &InstallState { hw_rev: "N0.9".into(), ..ok.clone() }, &w), Err(Reject::Incompatible));
        assert_eq!(check_install(&manifest(), &InstallState { security_counter: 3, battery: Some(10), ..ok.clone() }, &w), Err(Reject::Downgrade));
        assert_eq!(check_install(&manifest(), &InstallState { battery: Some(49), ..ok.clone() }, &w), Err(Reject::NotNow));
        assert_eq!(
            check_install(&Manifest { min_battery: None, ..manifest() }, &InstallState { battery: Some(39), ..ok.clone() }, &w),
            Err(Reject::NotNow),
            "the floor"
        );
    }

    /// Astra 01 F09: "never during a move" was a `moving: bool` the caller
    /// supplied. Now every reason comes from the collar's own state.
    #[test]
    fn the_collar_decides_from_its_own_state() {
        let w = InstallWindow::default();
        let not_now = |s: InstallState, why: &str| assert_eq!(check_install(&manifest(), &s, &w), Err(Reject::NotNow), "{why}");
        let ok = calm();
        not_now(InstallState { now: None, ..ok.clone() }, "no GNSS time");
        not_now(InstallState { last_fix: Some(9_000), ..ok.clone() }, "the last fix is 16 min old");
        not_now(InstallState { last_fix: None, ..ok.clone() }, "no fix at all");
        not_now(InstallState { fence: Some(FenceState::Warning), ..ok.clone() }, "in the warning zone");
        not_now(InstallState { fence: Some(FenceState::Outside), ..ok.clone() }, "outside");
        not_now(InstallState { fence: Some(FenceState::Unknown), ..ok.clone() }, "fence unknown");
        not_now(InstallState { episode: true, ..ok.clone() }, "an episode open");
        not_now(InstallState { pen: true, ..ok.clone() }, "in a pen");
        not_now(InstallState { fast_until: Some(10_600), ..ok.clone() }, "a fast window running");
        not_now(InstallState { staged: vec![12_000], ..ok.clone() }, "a strip due in 33 minutes");
        not_now(InstallState { staged: vec![9_000], ..ok.clone() }, "a strip overdue, waiting for its lease");
        check_install(&manifest(), &InstallState { staged: vec![20_000], fast_until: Some(9_000), ..ok.clone() }, &w).unwrap();
        let later = InstallWindow::new(Some(9_000), Some(86_400), 5_000);
        assert_eq!(later.opens_at(), 14_000);
        assert_eq!(check_install(&manifest(), &ok, &later), Err(Reject::NotNow), "before its own moment in the window");
        check_install(&manifest(), &InstallState { now: Some(14_000), last_fix: Some(13_990), ..ok }, &later).unwrap();
    }

    /// Astra 01 F09: an image that couldn't hear a beacon in 30 minutes was
    /// refused for good. The link no longer counts; a missing fix is retried.
    #[test]
    fn a_good_image_is_not_condemned_by_an_outage() {
        assert_eq!(after_test(TestBoot::Passed, 0), AfterTest::Confirm);
        assert_eq!(after_test(TestBoot::NoFix, 0), AfterTest::RollBack { refuse: false });
        assert_eq!(after_test(TestBoot::NoFix, NO_FIX_TRIES - 1), AfterTest::RollBack { refuse: true });
        assert_eq!(after_test(TestBoot::Failed, 0), AfterTest::RollBack { refuse: true });
    }

    /// Astra 01 F01/F14: a manifest with `min_battery: "90"` was read as no
    /// minimum.
    #[test]
    fn malformed_manifest_fields_are_refused() {
        let k = SigningKey::from_bytes(&[6; 32]);
        let mut m = IntMap::from_value(&cbor::decode(&manifest().payload()).unwrap()).unwrap();
        m.put(22, Value::text("90"));
        assert_eq!(Manifest::read(&cose::sign(&cbor::encode(&m.clone().into_value()), &k)).unwrap_err(), Reject::BadCbor);
        m.put(22, Value::uint(140u8));
        assert_eq!(Manifest::read(&cose::sign(&cbor::encode(&m.clone().into_value()), &k)).unwrap_err(), Reject::BadCbor);
        m.put(22, Value::uint(90u8)).put(19, Value::Array(vec![Value::uint(1u8), Value::text("node")]));
        assert_eq!(Manifest::read(&cose::sign(&cbor::encode(&m.into_value()), &k)).unwrap_err(), Reject::BadCbor);
    }

    /// Astra 01 F08: the nRF52840 had no way to replace a release key.
    #[test]
    fn trust_updates_replace_and_revoke() {
        let a = SigningKey::from_bytes(&[1; 32]);
        let b = SigningKey::from_bytes(&[2; 32]);
        let c = SigningKey::from_bytes(&[3; 32]);
        let id = |k: &SigningKey| kid(k.verifying_key().as_bytes());
        let mut store =
            TrustStore { keys: vec![(a.verifying_key().to_bytes(), Origin::Release), (b.verifying_key().to_bytes(), Origin::Release)], ..Default::default() };
        // A leaks: the reserve B revokes it and adds C.
        let u = TrustUpdate { counter: 1, revoke: vec![id(&a)], add: vec![c.verifying_key().to_bytes()] };
        let (env, read) = TrustUpdate::read(&u.sign(&b)).unwrap();
        env.verify(&b.verifying_key()).unwrap();
        store.apply(&env.kid, &read).unwrap();
        assert_eq!(store.trusted(), vec![id(&b), id(&c)]);
        assert_eq!(store.apply(&id(&a), &TrustUpdate { counter: 2, ..Default::default() }), Err(Reject::UnknownKid), "the revoked key signs nothing now");
        assert_eq!(store.apply(&id(&b), &TrustUpdate { counter: 1, ..Default::default() }), Err(Reject::Stale), "an old update again");
        // Nothing may leave the collar with no release key and no owner key.
        assert_eq!(store.apply(&id(&b), &TrustUpdate { counter: 3, revoke: vec![id(&b), id(&c)], add: vec![] }), Err(Reject::BadConfig));
        // Nor more keys than the store holds.
        let more = (10..13).map(|i| SigningKey::from_bytes(&[i; 32]).verifying_key().to_bytes()).collect();
        assert_eq!(store.apply(&id(&b), &TrustUpdate { counter: 3, revoke: vec![], add: more }), Err(Reject::BadConfig));
    }

    #[test]
    fn sign_and_read() {
        let k = SigningKey::from_bytes(&[6; 32]);
        let (env, back) = Manifest::read(&manifest().sign(&k)).unwrap();
        env.verify(&k.verifying_key()).unwrap();
        assert_eq!(back, manifest());
    }
}
