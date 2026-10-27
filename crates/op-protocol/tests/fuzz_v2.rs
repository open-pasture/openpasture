//! Hostile input for every v2 decoder (Astra 01 F11, F12): mutations of the
//! published vectors and random bytes, fed to everything a collar, anchor or
//! server reads from outside. Nothing may panic, and nothing may allocate
//! for a size it hasn't checked. Deterministic (a fixed xorshift seed), so a
//! failure reproduces; `OP_FUZZ_ROUNDS` runs longer.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;

use op_protocol::v2::batch::{Batch, Reply};
use op_protocol::v2::cbor;
use op_protocol::v2::command::{self, Trust};
use op_protocol::v2::firmware::{Manifest, TrustUpdate};
use op_protocol::v2::fragment::{Assemblies, Reassembly};
use op_protocol::v2::frame::{Beacon, FragmentFrame, Uplink, accepts, hello_signed};
use op_protocol::v2::identity::{FactoryRecord, Hello};
use op_protocol::v2::record::{Record, decode_records};
use op_protocol::{SigningKey, VerifyingKey};
use serde_json::Value as Json;

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// Every hex string in the published v2 vectors.
fn corpus() -> Vec<Vec<u8>> {
    fn walk(v: &Json, out: &mut Vec<Vec<u8>>) {
        match v {
            Json::String(s) if s.len() >= 4 && s.len() % 2 == 0 && s.bytes().all(|b| b.is_ascii_hexdigit()) => {
                out.push((0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect());
            }
            Json::Array(a) => a.iter().for_each(|x| walk(x, out)),
            Json::Object(o) => o.values().for_each(|x| walk(x, out)),
            _ => {}
        }
    }
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("vectors/v2");
    let mut out = Vec::new();
    for f in ["messages.json", "records.json", "link.json", "identity.json"] {
        let j: Json = serde_json::from_str(&std::fs::read_to_string(dir.join(f)).unwrap()).unwrap();
        walk(&j, &mut out);
    }
    out
}

fn mutate(rng: &mut Rng, seed: &[u8]) -> Vec<u8> {
    let mut b = seed.to_vec();
    for _ in 0..=rng.below(4) {
        match rng.below(8) {
            0 if !b.is_empty() => {
                let i = rng.below(b.len());
                b[i] ^= 1 << rng.below(8);
            }
            1 if !b.is_empty() => {
                let i = rng.below(b.len());
                b[i] = [0x00, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1f, 0x5f, 0x9f, 0xbf, 0xff, 0xf9, 0xfb, 0xc0, 0xd2][rng.below(15)];
            }
            2 => {
                let n = rng.below(b.len() + 1);
                b.truncate(n);
            }
            3 => {
                let i = rng.below(b.len() + 1);
                let fill = rng.next() as u8;
                b.splice(i..i, std::iter::repeat_n(fill, rng.below(9)));
            }
            4 if b.len() > 4 => {
                // A length or count field turned huge.
                let i = rng.below(b.len() - 3);
                b[i..i + 4].copy_from_slice(&[0xff, 0xff, 0xff, 0xff]);
            }
            5 => b.extend((0..rng.below(64)).map(|_| rng.next() as u8)),
            6 if b.len() > 2 => {
                let (i, j) = (rng.below(b.len()), rng.below(b.len()));
                b.swap(i, j);
            }
            _ => b = (0..rng.below(300)).map(|_| rng.next() as u8).collect(),
        }
    }
    b
}

/// Every decoder that reads bytes from outside.
fn feed(b: &[u8]) {
    let key = SigningKey::from_bytes(&[7; 32]).verifying_key();
    let any_key = |_: &[u8; 8]| Some(key);
    let _ = cbor::decode(b);
    let _ = cbor::decode_untagged(b);
    let _ = cbor::item_len(b);
    if let Ok(v) = cbor::decode(b) {
        let _ = Record::from_value(&v);
        if let Ok(e) = cbor::try_encode(&v) {
            assert_eq!(e, b, "a decoded value encodes back to its bytes");
        }
    }
    let _ = decode_records(b);
    if let Ok(r) = command::open(b, any_key) {
        let trust = Trust { server: Some(key), ..Default::default() };
        let _ = trust.authorize(&r);
        let _ = r.command.check_scope(&[0; 8], Some(b"H"), None);
        let _ = r.command.to_json();
    }
    let _ = Manifest::read(b);
    let _ = TrustUpdate::read(b);
    let _ = FactoryRecord::read(b);
    let _ = Hello::open(b);
    let _ = Batch::open(b, |_| Some(key));
    let _ = Batch::open(b, |_| None::<VerifyingKey>);
    let _ = Reply::decode(b);
    if let Ok(up) = Uplink::parse(b) {
        let _ = up.open(&[0; 16], &[0; 8]);
    }
    let _ = Beacon::decode(b, &[[0; 16]]);
    let _ = Beacon::decode_unverified(b);
    let _ = accepts(b, Some(1), true);
    let _ = accepts(b, None, false);
    let _ = hello_signed(b);
    if let Ok(f) = FragmentFrame::decode(b) {
        if let Some(mut r) = Reassembly::new(f.msg, f.count) {
            let _ = r.add(&f);
            let _ = r.need(8);
        }
        let mut a = Assemblies::default();
        let _ = a.add(&f, 0);
    }
}

#[test]
fn no_decoder_panics_on_hostile_input() {
    let rounds: usize = std::env::var("OP_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(30_000);
    let seeds = corpus();
    assert!(seeds.len() > 100, "the corpus is the published vectors");
    let mut rng = Rng(0x0c01_a520_2610_27ff);
    for seed in &seeds {
        let r = catch_unwind(AssertUnwindSafe(|| feed(seed)));
        assert!(r.is_ok(), "panicked on a published vector: {}", seed.iter().map(|b| format!("{b:02x}")).collect::<String>());
    }
    for round in 0..rounds {
        let seed = &seeds[rng.below(seeds.len())];
        let input = mutate(&mut rng, seed);
        let r = catch_unwind(AssertUnwindSafe(|| feed(&input)));
        assert!(r.is_ok(), "round {round}: panicked on {}", input.iter().map(|b| format!("{b:02x}")).collect::<String>());
    }
}

/// The review's reproductions, kept as fixed cases.
#[test]
fn the_reviews_hostile_inputs() {
    // A 65 544-byte uplink: refused as too large, never into CCM.
    let mut huge = vec![0x21, 0, 1, 0, 2, 0, 0, 0, 3, 0];
    huge.resize(10 + 65_536 + 8, 0);
    feed(&huge);
    // A fragment claiming 65 535 parts.
    let f = FragmentFrame { net: 1, dst: 0xffff, msg: 1, idx: 0, count: u16::MAX, data: vec![0; 4] };
    assert!(Reassembly::new(f.msg, f.count).is_none());
    feed(&f.encode());
    // Fix deltas at i64::MAX.
    feed(&[0x87, 0x02, 0x01, 0x18, 0x64, 0x01, 0x01, 0x01, 0x84, 0x18, 0x3c, 0x1b, 0x7f, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00, 0x01]);
}
