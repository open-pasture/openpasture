//! Protocol v2 test vectors: every message as the exact bytes (hex), its JSON
//! view and its signature by a published test key; records; frames,
//! fragments and airtime slots; claims, cards and firmware rules. The files
//! are `vectors/v2/*.json`; `vectors/README.md` says how to run them.
//!
//! `vectors_are_current` rebuilds the files and fails if they differ from
//! what is committed (`OP_WRITE_VECTORS=1 cargo test -p op-protocol --test
//! vectors_v2` rewrites them). The other tests read the committed files and
//! check this implementation against every case. `copy_matches_opencollar`
//! fails when `vectors/` here differs from `opencollar/protocol/vectors/`,
//! the published source of truth, whenever that checkout sits next to this one.

use std::path::{Path, PathBuf};

use op_geo::CollarLimits;
use op_protocol::v2::batch::{Batch, Downlink, Relayed, Reply, Rx};
use op_protocol::v2::cbor::{self, IntMap, Value};
use op_protocol::v2::command::{self, Bind, Boundary, Config, FirmwareUpdate, Handover, Member, Membership, Roster, Trust, Versions};
use op_protocol::v2::counter::{Counter, Journal, ReplayWindow};
use op_protocol::v2::crypto::{ccm_seal, frame_key, kid, wrap_key, x25519};
use op_protocol::v2::firmware::{AfterTest, InstallState, InstallWindow, Manifest, Origin, TestBoot, TrustStore, TrustUpdate, after_test, check_install};
use op_protocol::v2::fragment::{Reassembly, split};
use op_protocol::v2::frame::{
    Announce, BROADCAST, Beacon, FragmentFrame, Link, Need, UNCLAIMED, UPLINK_OVERHEAD, Uplink, hello_frame, hello_signed, nonce, seal_uplink,
    unwrap_beacon_key, wrap_beacon_key,
};
use op_protocol::v2::identity::{Card, FactoryRecord, Hello, bind_proof, check_bind, claim_check, claim_key, herd_from_v1, normalize_claim_code};
use op_protocol::v2::json::{hex, unhex};
use op_protocol::v2::policy::{Activation, DEFAULT_ZONE_LIMITS, Lease, PolicyState, Zones, overlaps};
use op_protocol::v2::record::{Ack, Cue, DeviceInfo, Episode, FirmwareStatus, Fix, MAX_RECORD_BYTES, Record, RecordBody, SlotEntry, Slots, Status, flags};
use op_protocol::v2::registry::{AckStatus, CueKind, CueMode, DeviceClass, EpisodeOutcome, FenceState, FirmwareState, MessageType, Reject, Role};
use op_protocol::v2::tdma::{Plan, Schedule, US915_500};
use op_protocol::v2::{Body, Command, cose};
use op_protocol::{SigningKey, VerifyingKey};
use serde::Serialize;
use serde_json::{Value as Json, json};

/// 2026-09-27T12:00:00Z
const T0: u64 = 1_790_510_400;
const HERD: &[u8] = b"HERD0001";
const OTHER_HERD: &[u8] = b"HERD0002";
const CLAIM_CODE: &str = "7Q4K9M2XH8D3TV5W";
const NONCE: [u8; 8] = *b"NONCE001";
const NET: u16 = 0x0a01;
/// The test herd's beacon key.
const BEACON_KEY: [u8; 16] = *b"beacon-key-herd1";
/// When the test collar was claimed (its bind's `issued_at`).
const CLAIMED_AT: u64 = T0 - 86_400;
/// The policy epoch the claimed test collar holds.
const EPOCH: u32 = 3;

/// Test keys: Ed25519 seeds of 32 consecutive bytes from `start`.
fn key(start: u8) -> SigningKey {
    SigningKey::from_bytes(&core::array::from_fn(|i| start.wrapping_add(i as u8)))
}

const KEYS: [(&str, u8); 10] = [
    ("server", 0x00),
    ("next_server", 0x20),
    ("owner", 0x40),
    ("device", 0x60),
    ("device2", 0x70),
    ("anchor", 0x80),
    ("maker", 0xa0),
    ("release", 0xc0),
    ("release2", 0xd0),
    ("new_owner", 0xe0),
];

fn named(name: &str) -> SigningKey {
    key(KEYS.iter().find(|(n, _)| *n == name).unwrap().1)
}

fn kid_of(k: &SigningKey) -> [u8; 8] {
    kid(k.verifying_key().as_bytes())
}

fn device_id() -> [u8; 8] {
    kid_of(&named("device"))
}

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("vectors/v2")
}

/// The same value with every object's keys in sorted order, so a file's
/// bytes don't depend on whether serde_json keeps insertion order (a
/// workspace build enables `preserve_order`; Astra 03 P1-12).
fn canon(v: Json) -> Json {
    match v {
        Json::Object(o) => {
            let mut entries: Vec<(String, Json)> = o.into_iter().collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            Json::Object(entries.into_iter().map(|(k, v)| (k, canon(v))).collect())
        }
        Json::Array(a) => Json::Array(a.into_iter().map(canon).collect()),
        other => other,
    }
}

fn compact<T: Serialize>(v: &T) -> String {
    serde_json::to_string(&canon(serde_json::to_value(v).unwrap())).unwrap()
}

/// A file: header fields pretty, one compact case per line.
fn file<C: Serialize>(header: &[(&str, Json)], sections: &[(&str, Vec<C>)]) -> String {
    let mut out = String::from("{\n");
    for (k, v) in header {
        out.push_str(&format!("  \"{k}\": {},\n", compact(v)));
    }
    let parts: Vec<String> = sections
        .iter()
        .map(|(name, cases)| {
            let lines: Vec<String> = cases.iter().map(|c| format!("    {}", compact(c))).collect();
            format!("  \"{name}\": [\n{}\n  ]", lines.join(",\n"))
        })
        .collect();
    out.push_str(&parts.join(",\n"));
    out.push_str("\n}\n");
    out
}

fn keys_header() -> Json {
    Json::Object(
        KEYS.iter()
            .map(|(name, start)| {
                let k = key(*start);
                (name.to_string(), json!({"seed": hex(&k.to_bytes()), "public": hex(k.verifying_key().as_bytes()), "kid": hex(&kid_of(&k))}))
            })
            .collect(),
    )
}

// ---------------------------------------------------------------- messages

/// A rectangle `w` × `h` metres, south-west corner `x`, `y` metres from Ames.
fn rect(x: f64, y: f64, w: f64, h: f64) -> Vec<[f64; 2]> {
    let p = op_geo::Projection::new([-93.62, 42.03]);
    vec![p.offset(x, y), p.offset(x + w, y), p.offset(x + w, y + h), p.offset(x, y + h)]
}

fn command(version: u32, body: Body) -> Command {
    Command { herd: HERD.to_vec(), device: None, version, issued_at: T0, expires_at: None, reference: None, effective_at: None, body }
}

fn strip() -> Boundary {
    Boundary::from_lonlat(&rect(0.0, 0.0, 400.0, 120.0), &[])
}

/// The permanent no-go zones the claimed test collar holds: a washed-out
/// creek crossing 500 m east of the strips.
fn zones() -> Zones {
    Zones { zones: vec![Boundary::from_lonlat(&rect(500.0, 0.0, 60.0, 40.0), &[]).outer], epoch: EPOCH }
}

fn lease() -> Lease {
    Lease { epoch: EPOCH, until: T0 + 7_200, digest: zones().digest() }
}

fn manifest() -> Manifest {
    Manifest {
        version: "2.1.0".into(),
        digest: core::array::from_fn(|i| i as u8 ^ 0x5a),
        size: 212_480,
        classes: vec![DeviceClass::Node, DeviceClass::Anchor],
        hw_revs: vec!["N1.0".into(), "A1.0".into()],
        security_counter: 2,
        min_battery: Some(50),
        revoke: vec![],
    }
}

/// The release key leaked: the reserve revokes it and adds a new reserve.
fn trust_update() -> TrustUpdate {
    TrustUpdate { counter: 1, revoke: vec![kid_of(&named("release"))], add: vec![named("new_owner").verifying_key().to_bytes()] }
}

/// The handover the claimed test collar holds pending (`next_server`).
fn handover_rotate() -> Vec<u8> {
    command(
        6,
        Body::Handover(Handover {
            new_key: named("next_server").verifying_key().to_bytes(),
            deadline: T0 + 7 * 86_400,
            endpoint: None,
            endpoint_pin: None,
            reason: Some(1),
            new_owner: None,
        }),
    )
    .sign(&named("server"))
}

/// The test collar's wrap key (the server side computes the same).
fn device_wrap_key() -> [u8; 16] {
    wrap_key(&x25519(&named("server"), &named("device").verifying_key()), &device_id(), &kid_of(&named("server")))
}

#[derive(Debug, Serialize, serde::Deserialize)]
struct MessageCase {
    name: String,
    /// `claimed` (the default collar in the header) or `unclaimed`.
    state: String,
    /// The collar has GNSS time (`now` in the header). False: no clock yet.
    clock: bool,
    hex: String,
    /// The payload's JSON view; `null` when the bytes don't decode.
    json: Json,
    /// `ok`, or the reject code.
    expect: String,
    /// What the CDDL schema says of it: `accept`, `reject`, or `skip` when
    /// the question isn't the schema's (canonical bytes, size, tags).
    schema: String,
}

/// Re-sign an arbitrary payload (for cases that aren't valid commands).
fn signed(payload: &Value, with: &SigningKey) -> Vec<u8> {
    cose::sign(&cbor::encode(payload), with)
}

/// A COSE_Sign1 with its parts replaced, re-signed or not.
fn envelope(protected: Value, unprotected: Value, payload: &[u8], key: &SigningKey) -> Vec<u8> {
    let p = cbor::encode(&protected);
    let tbs = cbor::encode(&Value::Array(vec![
        Value::text("Signature1"),
        Value::bytes(p.clone()),
        Value::bytes(b"opencollar/2".to_vec()),
        Value::bytes(payload.to_vec()),
    ]));
    let sig = ed25519_dalek::Signer::sign(key, &tbs);
    cbor::encode(&Value::Tag(
        18,
        Box::new(Value::Array(vec![Value::bytes(p), unprotected, Value::bytes(payload.to_vec()), Value::bytes(sig.to_bytes().to_vec())])),
    ))
}

fn protected_for(key: &SigningKey, crit: bool) -> Value {
    let mut h = IntMap::new();
    h.put(1, Value::int(-8)).put(4, Value::bytes(kid_of(key).to_vec()));
    if crit {
        h.put(2, Value::Array(vec![Value::uint(99u8)]));
    }
    h.into_value()
}

fn with_key(payload: &[u8], key: u64, v: Value) -> Value {
    let mut m = cbor::decode(payload).unwrap();
    if let Value::Map(entries) = &mut m {
        entries.retain(|(k, _)| k.as_u64() != Some(key));
        entries.push((Value::uint(key), v));
    }
    m
}

fn without_key(payload: &[u8], key: u64) -> Value {
    let mut m = cbor::decode(payload).unwrap();
    if let Value::Map(entries) = &mut m {
        entries.retain(|(k, _)| k.as_u64() != Some(key));
    }
    m
}

fn message_cases() -> Vec<MessageCase> {
    let server = named("server");
    let owner = named("owner");
    let me = device_id();
    // (name, state, clock, bytes, schema)
    let mut out: Vec<(&str, &str, bool, Vec<u8>, &str)> = Vec::new();
    let mut push = |name: &'static str, state: &'static str, bytes: Vec<u8>| out.push((name, state, true, bytes, "accept"));

    push("boundary_herd", "claimed", command(57, Body::Boundary(strip())).sign(&server));
    let mut staged = Boundary::from_lonlat(&rect(0.0, 0.0, 400.0, 400.0), &[rect(150.0, 150.0, 40.0, 40.0)]);
    staged.warn_cm = Some(800);
    staged.hyst_cm = Some(150);
    staged.policy = Some(EPOCH);
    push(
        "boundary_staged_with_a_hole",
        "claimed",
        Command { effective_at: Some(T0 + 3600), reference: Some(b"bnd-58".to_vec()), ..command(58, Body::Boundary(staged)) }.sign(&server),
    );
    let pen = Boundary { cue_mode: Some(CueMode::Track), ..Boundary::from_lonlat(&rect(-60.0, 0.0, 50.0, 50.0), &[]) };
    push(
        "boundary_pen_for_one_collar",
        "claimed",
        Command { device: Some(me), expires_at: Some(T0 + 86_400), ..command(59, Body::Boundary(pen)) }.sign(&server),
    );
    let over_creek = Boundary { policy: Some(EPOCH), ..Boundary::from_lonlat(&rect(450.0, 0.0, 200.0, 120.0), &[]) };
    push("boundary_over_a_no_go_zone", "claimed", command(60, Body::Boundary(over_creek)).sign(&server));
    push("zones", "claimed", command(11, Body::Zones(zones())).sign(&server));
    push("lease", "claimed", command(12, Body::Lease(lease())).sign(&server));
    push(
        "config",
        "claimed",
        Command {
            device: Some(me),
            ..command(
                4,
                Body::Config(Config {
                    report_s: 300,
                    fix_s: Some(60),
                    fast_report_s: Some(60),
                    fast_fix_s: Some(10),
                    fast_until: Some(T0 + 1800),
                    track_s: Some(1800),
                    ..Default::default()
                }),
            )
        }
        .sign(&server),
    );
    push(
        "config_anchor_endpoint",
        "claimed",
        Command {
            device: Some(me),
            ..command(
                5,
                Body::Config(Config {
                    report_s: 60,
                    endpoint: Some("https://farm.example.com/collar/v2".into()),
                    endpoint_pin: Some([0x77; 32]),
                    anchor_hold_s: Some(3600),
                    ..Default::default()
                }),
            )
        }
        .sign(&server),
    );
    let member = |short, groups| Membership { short, role: Role::Node, net: NET, plan: Some(1), cycle_s: Some(300), rank: None, groups, beacon_key: None };
    let membership = |herd: &[u8], m: Membership| Command { herd: herd.to_vec(), device: Some(me), ..command(2, Body::Membership(m)) };
    push("membership", "claimed", membership(HERD, member(12, None)).sign(&server));
    push("membership_moves_the_collar_to_another_herd", "claimed", membership(OTHER_HERD, member(12, None)).sign(&server));
    let wrapped = wrap_beacon_key(&device_wrap_key(), &me, 3, HERD, NET, &BEACON_KEY);
    push(
        "membership_with_groups_and_the_beacon_key",
        "claimed",
        Command { version: 3, ..membership(HERD, Membership { beacon_key: Some(wrapped), ..member(312, Some(2)) }) }.sign(&server),
    );
    push(
        "roster",
        "claimed",
        command(
            3,
            Body::Roster(Roster {
                members: vec![
                    Member { short: 1, device: kid_of(&named("anchor")), role: Role::Anchor },
                    Member { short: 12, device: me, role: Role::Node },
                    Member { short: 13, device: [0x13; 8], role: Role::Node },
                ],
                revoked: vec![[0xee; 8]],
            }),
        )
        .sign(&server),
    );
    let handover = |reason, endpoint: Option<&str>, new_owner: Option<[u8; 32]>| {
        Body::Handover(Handover {
            new_key: named("next_server").verifying_key().to_bytes(),
            deadline: T0 + 7 * 86_400,
            endpoint: endpoint.map(str::to_owned),
            endpoint_pin: None,
            reason: Some(reason),
            new_owner,
        })
    };
    push("handover_rotate", "claimed", handover_rotate());
    let sale = handover(2, Some("https://other.example.org/collar/v2"), Some(named("new_owner").verifying_key().to_bytes()));
    push("transfer_by_the_owner_key", "claimed", command(7, sale.clone()).sign(&owner));
    let pending = command::digest(&handover_rotate());
    push("confirm_by_the_new_key", "claimed", command(1, Body::Confirm { replaces: kid_of(&server), handover: pending }).sign(&named("next_server")));
    push("owner_key_replaced", "claimed", command(2, Body::Owner { owner_key: named("new_owner").verifying_key().to_bytes() }).sign(&owner));
    push(
        "firmware",
        "claimed",
        Command {
            effective_at: Some(T0 + 3600),
            ..command(8, Body::Firmware(FirmwareUpdate { manifest: manifest().sign(&named("release")), window_s: Some(86_400), url: None }))
        }
        .sign(&server),
    );
    push("trust", "claimed", command(13, Body::Trust { update: trust_update().sign(&named("release2")) }).sign(&server));
    push("resend", "claimed", Command { device: Some(me), ..command(9, Body::Resend { from: 100, to: 140 }) }.sign(&server));
    push("release_by_the_owner_key", "claimed", Command { device: Some(me), ..command(10, Body::Release) }.sign(&owner));
    push(
        "unknown_type_is_unsupported",
        "claimed",
        command(1, Body::Other { message_type: 40, fields: IntMap(vec![(16, Value::text("later"))]) }).sign(&server),
    );

    // Claims.
    let farm = named("next_server");
    let bind = |proof: [u8; 16]| {
        Command {
            device: Some(me),
            ..command(
                1,
                Body::Bind(Bind {
                    server_key: farm.verifying_key().to_bytes(),
                    proof,
                    nonce: NONCE,
                    endpoint: None,
                    owner_key: Some(owner.verifying_key().to_bytes()),
                    endpoint_pin: None,
                }),
            )
        }
        .sign(&farm)
    };
    let good_proof = bind_proof(&claim_key(CLAIM_CODE, &me), &me, &NONCE, farm.verifying_key().as_bytes(), HERD);
    let wrong_proof = bind_proof(&claim_key("0000000000000000", &me), &me, &NONCE, farm.verifying_key().as_bytes(), HERD);
    push("bind", "unclaimed", bind(good_proof));
    push("bind_with_the_wrong_claim_code", "unclaimed", bind(wrong_proof));
    push("bind_when_already_claimed", "claimed", bind(good_proof));

    // Refusals.
    push("signed_by_an_unknown_key", "claimed", command(57, Body::Boundary(strip())).sign(&named("anchor")));
    push("boundary_signed_by_the_owner_key", "claimed", command(57, Body::Boundary(strip())).sign(&owner));
    push("release_by_the_server_key_with_an_owner_enrolled", "claimed", Command { device: Some(me), ..command(10, Body::Release) }.sign(&server));
    push("transfer_by_the_server_key", "claimed", command(7, sale).sign(&server));
    push("expired", "claimed", Command { expires_at: Some(T0), ..command(57, Body::Boundary(strip())) }.sign(&server));
    push("another_herd", "claimed", Command { herd: OTHER_HERD.to_vec(), ..command(57, Body::Boundary(strip())) }.sign(&server));
    push("another_collar", "claimed", Command { device: Some([0x13; 8]), ..command(57, Body::Boundary(strip())) }.sign(&server));
    push("confirm_by_the_old_key", "claimed", command(1, Body::Confirm { replaces: kid_of(&server), handover: pending }).sign(&server));
    push("confirm_for_another_handover", "claimed", command(1, Body::Confirm { replaces: kid_of(&server), handover: [0x11; 8] }).sign(&named("next_server")));
    push("issued_before_the_claim", "claimed", Command { issued_at: CLAIMED_AT - 60, ..command(70, Body::Boundary(strip())) }.sign(&server));
    push("membership_on_an_unknown_plan", "claimed", membership(HERD, Membership { plan: Some(9), ..member(12, None) }).sign(&server));
    push("membership_short_past_its_groups", "claimed", membership(HERD, member(312, None)).sign(&server));
    push(
        "handover_deadline_past_30_days",
        "claimed",
        command(
            6,
            Body::Handover(Handover {
                deadline: T0 + 31 * 86_400,
                ..match handover(1, None, None) {
                    Body::Handover(h) => h,
                    _ => unreachable!(),
                }
            }),
        )
        .sign(&server),
    );
    push("self_intersecting", "claimed", {
        let bowtie = Boundary::from_lonlat(&[[-93.62, 42.03], [-93.61, 42.04], [-93.61, 42.03], [-93.62, 42.04]], &[]);
        command(57, Body::Boundary(bowtie)).sign(&server)
    });
    let mut out_clock = out;
    let mut push =
        |name: &'static str, state: &'static str, clock: bool, bytes: Vec<u8>, schema: &'static str| out_clock.push((name, state, clock, bytes, schema));
    push(
        "expiring_command_without_a_clock",
        "claimed",
        false,
        Command { expires_at: Some(T0 + 600), ..command(57, Body::Boundary(strip())) }.sign(&server),
        "accept",
    );
    // A flipped payload bit: what the schema makes of it is beside the point.
    let mut tampered = command(57, Body::Boundary(strip())).sign(&server);
    let n = tampered.len();
    tampered[n - 70] ^= 0x01;
    push("tampered", "claimed", true, tampered, "skip");

    // Shape refusals: the schema refuses these too.
    let boundary_payload = command(57, Body::Boundary(strip())).payload();
    push("no_herd", "claimed", true, signed(&without_key(&boundary_payload, 1), &server), "reject");
    let config_payload = Command { device: Some(me), ..command(4, Body::Config(Config { report_s: 300, ..Default::default() })) }.payload();
    push("config_without_a_device", "claimed", true, signed(&without_key(&config_payload, 2), &server), "reject");
    let membership_payload = membership(HERD, member(12, None)).payload();
    push("membership_without_a_device", "claimed", true, signed(&without_key(&membership_payload, 2), &server), "reject");
    let release_payload = Command { device: Some(me), ..command(10, Body::Release) }.payload();
    push("release_without_a_device", "claimed", true, signed(&without_key(&release_payload, 2), &owner), "reject");
    push(
        "payload_key_not_a_number",
        "claimed",
        true,
        {
            let mut m = cbor::decode(&boundary_payload).unwrap();
            if let Value::Map(e) = &mut m {
                e.push((Value::Bool(false), Value::uint(2u8)));
            }
            signed(&m, &server)
        },
        "reject",
    );
    push(
        "unprotected_header_not_empty",
        "claimed",
        true,
        envelope(
            protected_for(&server, false),
            Value::Map(vec![(Value::uint(2u8), Value::Array(vec![Value::uint(99u8)])), (Value::uint(99u8), Value::Bool(true))]),
            &boundary_payload,
            &server,
        ),
        "reject",
    );
    push("crit_in_the_protected_header", "claimed", true, envelope(protected_for(&server, true), Value::Map(vec![]), &boundary_payload, &server), "reject");
    // version 57 as a 4-byte integer: valid CBOR, not deterministic.
    let mut long = boundary_payload.clone();
    let at = long.windows(3).position(|w| w == [0x03, 0x18, 57]).unwrap();
    long.splice(at + 1..at + 3, [0x1a, 0, 0, 0, 57]);
    push("payload_not_deterministic", "claimed", true, cose::sign(&long, &server), "skip");
    push("payload_with_a_tag", "claimed", true, signed(&with_key(&boundary_payload, 99, Value::Tag(1, Box::new(Value::uint(0u8)))), &server), "skip");
    let untagged = command(57, Body::Boundary(strip())).sign(&server)[1..].to_vec();
    push("untagged", "claimed", true, untagged, "reject");
    // Over 12 288 bytes; an unknown key would otherwise be ignored.
    push("too_large", "claimed", true, signed(&with_key(&boundary_payload, 99, Value::bytes(vec![0; 12_300])), &server), "skip");
    push(
        "report_interval_too_short",
        "claimed",
        true,
        Command { device: Some(me), ..command(4, Body::Config(Config { report_s: 5, ..Default::default() })) }.sign(&server),
        "reject",
    );
    push("short_address_zero", "claimed", true, membership(HERD, member(0, None)).sign(&server), "reject");

    out_clock
        .into_iter()
        .map(|(name, state, clock, bytes, schema)| {
            let json = cose::Sign1::parse(&bytes)
                .ok()
                .and_then(|s| s.payload_map().ok())
                .and_then(|m| Command::from_payload(&m).ok())
                .map_or(Json::Null, |c| c.to_json());
            MessageCase {
                name: name.into(),
                state: state.into(),
                clock,
                hex: hex(&bytes),
                json,
                expect: outcome(collar_check(&bytes, state, clock)),
                schema: schema.into(),
            }
        })
        .collect()
}

fn outcome(r: Result<(), Reject>) -> String {
    r.map_or_else(|e| e.name().to_string(), |()| "ok".into())
}

/// The claimed test collar's keys.
fn claimed_trust() -> Trust {
    Trust {
        server: Some(named("server").verifying_key()),
        pending: Some(named("next_server").verifying_key()),
        pending_handover: Some(command::digest(&handover_rotate())),
        owner: Some(named("owner").verifying_key()),
    }
}

/// The firmware keys its bootloader trusts.
fn trust_store() -> TrustStore {
    TrustStore {
        keys: vec![(named("release").verifying_key().to_bytes(), Origin::Release), (named("release2").verifying_key().to_bytes(), Origin::Release)],
        ..Default::default()
    }
}

/// The collar's whole check of a command, in the spec's order
/// (`messages.md` §4.3): envelope, key, signature, payload, who may sign,
/// claim, scope, versions, the type's own rules, then zones and lease.
fn collar_check(bytes: &[u8], state: &str, clock: bool) -> Result<(), Reject> {
    let me = device_id();
    let claimed = state == "claimed";
    let trust = if claimed { claimed_trust() } else { Trust::default() };
    let now = clock.then_some(T0 + 60);
    let r = command::open(bytes, |k| trust.key_for(k))?;
    trust.authorize(&r)?;
    let c = &r.command;
    if let Body::Bind(b) = &c.body {
        check_bind(b, &c.herd, &me, &NONCE, CLAIM_CODE)?;
    }
    c.check_scope(&me, claimed.then_some(HERD), now)?;
    if claimed && !matches!(c.body, Body::Boundary(_)) {
        Versions { held: Default::default(), claimed_at: CLAIMED_AT }.check(&r)?;
    } else if claimed && c.issued_at < CLAIMED_AT {
        return Err(Reject::Stale);
    }
    match &c.body {
        Body::Boundary(b) => {
            b.check(&CollarLimits::V0)?;
            let policy = PolicyState { zones: Some(zones()), lease: Some(lease()) };
            match policy.may_activate(c, now) {
                Activation::Reject(code) => Err(code),
                _ => Ok(()),
            }
        }
        Body::Config(x) => x.check(false),
        Body::Membership(m) => m.check(),
        Body::Handover(h) => h.check(c.issued_at),
        Body::Zones(z) => z.check(DEFAULT_ZONE_LIMITS),
        Body::Trust { update } => {
            let (env, _) = TrustUpdate::read(update)?;
            let key = trust_store()
                .keys
                .iter()
                .map(|(k, _)| VerifyingKey::from_bytes(k).unwrap())
                .find(|k| kid(k.as_bytes()) == env.kid)
                .ok_or(Reject::UnknownKid)?;
            env.verify(&key)
        }
        Body::Other { .. } => Err(Reject::Unsupported),
        _ => Ok(()),
    }
}

#[derive(Debug, Serialize, serde::Deserialize)]
struct DocumentCase {
    name: String,
    signer: String,
    hex: String,
    json: Json,
}

fn factory_record() -> FactoryRecord {
    let dev = named("device");
    FactoryRecord {
        serial: "OCN-2611-00123".into(),
        class: DeviceClass::Node,
        hw_rev: "N1.0".into(),
        device_key: dev.verifying_key().to_bytes(),
        fw: "2.0.0".into(),
        made_at: T0 - 30 * 86_400,
        maker: "OpenCollar".into(),
        claim_check: Some(claim_check(&claim_key(CLAIM_CODE, &device_id()))),
        radio: Some(IntMap(vec![(1, Value::text("US915")), (2, Value::bytes(vec![0xd4, 0x12, 0x34, 0x56, 0x78, 0x9a]))])),
        // Both OpenCollar release keys, the second the offline reserve.
        image_keys: vec![kid_of(&named("release")), kid_of(&named("release2"))],
        test: Some(IntMap(vec![(1, Value::text("fixture-1")), (2, Value::Bool(true)), (3, Value::bytes(vec![0xab; 32])), (4, Value::uint(T0 - 30 * 86_400))])),
        caps: vec![],
    }
}

fn hello_of(device: &str, nonce: [u8; 8]) -> Hello {
    Hello {
        class: DeviceClass::Node,
        fw: "2.0.0".into(),
        nonce: Some(nonce),
        caps: vec!["holes".into(), "slots".into(), "episodes".into()],
        device_key: named(device).verifying_key().to_bytes(),
    }
}

fn hello() -> Hello {
    hello_of("device", NONCE)
}

fn document_cases() -> Vec<DocumentCase> {
    let doc = |name: &str, signer: &str, bytes: Vec<u8>, json: Json| DocumentCase { name: name.into(), signer: signer.into(), hex: hex(&bytes), json };
    let fr = factory_record();
    let sim = FactoryRecord {
        serial: "SIM-0001".into(),
        maker: "openpasture collar-sim".into(),
        caps: vec!["sim".into()],
        claim_check: None,
        radio: None,
        test: None,
        ..factory_record()
    };
    vec![
        doc("factory_record", "maker", fr.sign(&named("maker")), fr.to_json()),
        doc("factory_record_simulated", "maker", sim.sign(&named("maker")), sim.to_json()),
        doc("manifest", "release", manifest().sign(&named("release")), manifest().to_json()),
        doc("hello", "device", hello().sign(&named("device")), hello().to_json()),
        doc("trust_update", "release2", trust_update().sign(&named("release2")), trust_update().to_json()),
    ]
}

// ---------------------------------------------------------------- records

#[derive(Debug, Serialize, serde::Deserialize)]
struct RecordCase {
    name: String,
    hex: String,
    /// The JSON view; `null` when the record is refused.
    json: Json,
    expect: String,
    /// Encoding the decoded record gives back these bytes (false when it
    /// carried fields this version ignores).
    same_bytes: bool,
    /// What the CDDL sender schema says: `accept`, `reject`, or `skip`
    /// (canonical bytes, tags: not the schema's question).
    schema: String,
}

/// The test collar's device snapshot, too big for one frame whole.
fn device_snapshot() -> DeviceInfo {
    DeviceInfo {
        class: Some(DeviceClass::Node),
        fw: Some("2.0.0".into()),
        caps: ["holes", "slots", "collar_id", "cue_mode", "episodes", "config", "v2", "firmware", "handover", "owner", "zones"].map(String::from).to_vec(),
        limits: Some(CollarLimits::V0),
        hw_rev: Some("N1.0".into()),
        server_kid: Some(kid_of(&named("server"))),
        owner_kid: Some(kid_of(&named("owner"))),
        versions: vec![(MessageType::Boundary, 57), (MessageType::Config, 4), (MessageType::Membership, 2), (MessageType::Zones, 11)],
        herd: Some(HERD.to_vec()),
        short: Some(12),
        boot: Some("mcuboot 2.1.0".into()),
        security_counter: Some(2),
        image_keys: vec![kid_of(&named("release")), kid_of(&named("release2"))],
        plans: vec![1],
        zone_limits: Some(DEFAULT_ZONE_LIMITS),
        ..Default::default()
    }
}

fn records() -> Vec<(&'static str, Record)> {
    let t = T0 + 120;
    let rec = |seq, t, body| Record { seq, t, body };
    let status =
        Status { lon: -936_200_123, lat: 420_301_456, acc_dm: 18, state: Some(FenceState::Inside), bver: 57, battery: Some(81), flags: Some(flags::ARMED) };
    vec![
        ("status", rec(4321, t, RecordBody::Status(status.clone()))),
        (
            "status_stale_no_battery",
            rec(
                4322,
                t + 300,
                RecordBody::Status(Status { flags: Some(flags::STALE | flags::FAST), battery: None, state: Some(FenceState::Warning), ..status.clone() }),
            ),
        ),
        ("status_without_flags", rec(4323, t + 600, RecordBody::Status(Status { flags: None, ..status }))),
        (
            "fixes",
            rec(
                4324,
                t,
                RecordBody::Fixes(vec![
                    Fix { t, lon: -936_200_123, lat: 420_301_456, acc_dm: 18 },
                    Fix { t: t + 60, lon: -936_199_204, lat: 420_301_911, acc_dm: 21 },
                    Fix { t: t + 120, lon: -936_200_390, lat: 420_301_020, acc_dm: 15 },
                ]),
            ),
        ),
        ("cue_warn", rec(4325, t, RecordBody::Cue(Cue { kind: CueKind::Warn, level: 2, margin_cm: 140, ring: 0, bver: 57, dur_ms: Some(300) }))),
        ("cue_outside_in_a_hole", rec(4326, t + 1, RecordBody::Cue(Cue { kind: CueKind::Outside, level: 4, margin_cm: -60, ring: 2, bver: 57, dur_ms: None }))),
        (
            "episode",
            rec(
                4327,
                t,
                RecordBody::Episode(Episode { dur_s: 9, bver: 57, ring: 0, cues: 4, max_level: 3, min_margin_cm: 80, outcome: EpisodeOutcome::TurnedBack }),
            ),
        ),
        (
            "ack_applied",
            rec(
                4328,
                t,
                RecordBody::Ack(Ack { message_type: MessageType::Boundary, version: 57, status: AckStatus::Applied, code: None, digest: Some([0x0b; 8]) }),
            ),
        ),
        (
            "ack_rejected",
            rec(
                4329,
                t,
                RecordBody::Ack(Ack {
                    message_type: MessageType::Boundary,
                    version: 58,
                    status: AckStatus::Rejected,
                    code: Some(Reject::HoleTooClose),
                    digest: None,
                }),
            ),
        ),
        (
            "health",
            rec(
                4330,
                t,
                RecordBody::Health(IntMap(vec![
                    (1, Value::uint(120u8)),
                    (2, Value::uint(118u8)),
                    (3, Value::uint(9u8)),
                    (7, Value::uint(3310u16)),
                    (8, Value::Bool(true)),
                    (9, Value::int(215)),
                    (10, Value::uint(86_400u32)),
                    (11, Value::text("power_on")),
                    (15, IntMap(vec![(1, Value::int(-97)), (2, Value::int(6)), (3, Value::uint(1438u16)), (4, Value::uint(2u8))]).into_value()),
                ])),
            ),
        ),
        (
            "device_simulated_unclaimed",
            rec(
                0,
                t,
                RecordBody::Device(DeviceInfo {
                    class: Some(DeviceClass::Node),
                    fw: Some("2.0.0".into()),
                    caps: vec!["sim".into()],
                    claim_nonce: Some(NONCE),
                    ..Default::default()
                }),
            ),
        ),
        (
            "slots",
            rec(
                4333,
                t,
                RecordBody::Slots(Slots {
                    entries: vec![
                        SlotEntry { version: 57, status: AckStatus::Applied, effective_at: None },
                        SlotEntry { version: 58, status: AckStatus::Received, effective_at: Some(T0 + 3600) },
                    ],
                    part: None,
                }),
            ),
        ),
        (
            "slots_part_2_of_2",
            rec(
                4334,
                t,
                RecordBody::Slots(Slots {
                    entries: vec![SlotEntry { version: 61, status: AckStatus::Received, effective_at: Some(T0 + 7200) }],
                    part: Some((1, 2)),
                }),
            ),
        ),
        (
            "anchor",
            rec(
                4335,
                t,
                RecordBody::Anchor(IntMap(vec![
                    (1, Value::Bool(true)),
                    (2, Value::uint(76u8)),
                    (3, Value::int(-104)),
                    (4, Value::uint(248u8)),
                    (5, Value::uint(1u8)),
                ])),
            ),
        ),
        (
            "firmware",
            rec(4336, t, RecordBody::Firmware(FirmwareStatus { state: FirmwareState::Staged, version: "2.1.0".into(), digest: Some([0x5a; 8]), code: None })),
        ),
        (
            "firmware_deferred",
            rec(
                4337,
                t,
                RecordBody::Firmware(FirmwareStatus { state: FirmwareState::Staged, version: "2.1.0".into(), digest: None, code: Some(Reject::NotNow) }),
            ),
        ),
    ]
}

/// The device snapshot's parts, as records made at one `t`.
fn device_parts() -> Vec<(String, Record)> {
    let parts = device_snapshot().split(MAX_RECORD_BYTES);
    let n = parts.len();
    parts
        .into_iter()
        .enumerate()
        .map(|(i, d)| (format!("device_part_{}_of_{n}", i + 1), Record { seq: 4320 + i as u32, t: T0 + 120, body: RecordBody::Device(d) }))
        .collect()
}

fn record_cases() -> Vec<RecordCase> {
    let mut out: Vec<RecordCase> = records()
        .into_iter()
        .map(|(n, r)| (n.to_string(), r))
        .chain(device_parts())
        .map(|(name, r)| RecordCase { name, hex: hex(&r.encode()), json: r.to_json(), expect: "ok".into(), same_bytes: true, schema: "accept".into() })
        .collect();
    let raw_bytes = |name: &str, bytes: Vec<u8>, schema: &str| {
        let r = cbor::decode_untagged(&bytes).map_err(|_| Reject::BadCbor).and_then(|v| Record::from_value(&v));
        let same_bytes = r.as_ref().is_ok_and(|r| r.encode() == bytes);
        RecordCase {
            name: name.into(),
            hex: hex(&bytes),
            json: r.as_ref().map_or(Json::Null, Record::to_json),
            expect: outcome(r.map(|_| ())),
            same_bytes,
            schema: schema.into(),
        }
    };
    let raw = |name: &str, v: Value, schema: &str| raw_bytes(name, cbor::encode(&v), schema);
    let ints = |v: &[i64]| Value::Array(v.iter().map(|x| Value::int(*x)).collect());
    let t = T0 as i64;
    out.push(raw(
        "unknown_record_type_is_kept",
        Value::Array(vec![Value::uint(42u8), Value::uint(4400u16), Value::uint(T0), Value::text("from a newer collar")]),
        "accept",
    ));
    out.push(raw("unknown_cue_kind_is_kept", ints(&[3, 4401, t, 9, 2, -150, 0, 57]), "accept"));
    out.push(raw("unknown_outcome_is_kept", ints(&[4, 4402, t, 12, 57, 0, 3, 2, 40, 7]), "accept"));
    // A reader ignores elements past the ones it knows; a v2.0 sender never sends them.
    out.push(raw("extra_fields_are_ignored", ints(&[5, 4403, t, 1, 57, 2, 0, 0, 99]), "reject"));
    out.push(raw("status_too_short", ints(&[1, 4404, t, -936_200_123]), "reject"));
    out.push(raw("seq_not_a_number", Value::Array(vec![Value::uint(1u8), Value::text("x"), Value::uint(T0)]), "reject"));
    out.push(raw("battery_out_of_range", ints(&[1, 4405, t, -936_200_123, 420_301_456, 18, 1, 57, 255]), "reject"));
    out.push(raw("cue_level_out_of_range", ints(&[3, 4406, t, 1, 255, -150, 0, 57]), "reject"));
    out.push(raw(
        "state_not_a_number",
        Value::Array(vec![
            Value::uint(1u8),
            Value::uint(4407u16),
            Value::uint(T0),
            Value::int(1),
            Value::int(1),
            Value::uint(18u8),
            Value::text("inside"),
            Value::uint(57u8),
        ]),
        "reject",
    ));
    out.push(raw(
        "map_key_not_a_number",
        Value::Array(vec![
            Value::uint(6u8),
            Value::uint(4408u16),
            Value::uint(T0),
            Value::Map(vec![(Value::uint(0u8), Value::uint(1u8)), (Value::Bool(false), Value::uint(2u8))]),
        ]),
        "reject",
    ));
    out.push(raw(
        "fix_delta_overflows",
        Value::Array(vec![
            Value::uint(2u8),
            Value::uint(4409u16),
            Value::uint(T0),
            Value::int(1),
            Value::int(1),
            Value::uint(18u8),
            Value::Array(vec![Value::uint(60u8), Value::int(i64::MAX), Value::int(0), Value::uint(18u8)]),
        ]),
        "accept",
    ));
    // index < count is past what the schema can say.
    out.push(raw(
        "part_index_past_its_count",
        Value::Array(vec![Value::uint(8u8), Value::uint(4410u16), Value::uint(T0), Value::Array(vec![]), ints(&[2, 2])]),
        "accept",
    ));
    out.push(raw_bytes("tag_in_a_record", [&[0x84u8, 0x06, 0x19, 0x11, 0x3b][..], &cbor::encode(&Value::uint(T0)), &[0xc1, 0xa0]].concat(), "reject"));
    out.push(raw_bytes(
        "float_in_a_record",
        [&[0x88u8, 0x01, 0x19, 0x11, 0x3c][..], &cbor::encode(&Value::uint(T0)), &[0x01, 0x01, 0xf9, 0x3e, 0x00, 0x01, 0x18, 0x39]].concat(),
        "reject",
    ));
    // Non-deterministic bytes are refused before the record is read.
    let mut long = cbor::encode(&ints(&[6, 1, t]));
    long.splice(2..3, [0x18, 0x01]);
    out.push(RecordCase {
        name: "not_deterministic".into(),
        hex: hex(&long),
        json: Json::Null,
        expect: outcome(cbor::decode(&long).map(|_| ()).map_err(|_| Reject::BadCbor)),
        same_bytes: false,
        schema: "skip".into(),
    });
    out
}

// ---------------------------------------------------------------- link

fn link_json(link: &Link) -> Json {
    json!({
        "got": link.got,
        "need": link.need.iter().map(|n| json!({"msg": n.msg, "from": n.from, "missing": hex(&n.missing)})).collect::<Vec<_>>(),
        "anchor": link.anchor,
    })
}

fn beacon_json(b: &Beacon) -> Json {
    json!({
        "net": b.net, "anchor": b.anchor, "minute": b.minute, "bver": b.bver, "pending": b.pending,
        "msgs": b.msgs.iter().map(|a| json!({"msg": a.msg, "count": a.count, "dst": a.dst, "type": a.message_type})).collect::<Vec<_>>(),
        "heard": b.heard.as_ref().map(|(f, bits)| json!({"first": f, "bits": hex(bits)})),
        "state": b.state, "handon": b.handon.map(|(s, m)| json!({"short": s, "minute": m})), "term": b.term,
    })
}

fn test_frame_key() -> [u8; 16] {
    let server = named("server");
    frame_key(&x25519(&named("device"), &server.verifying_key()), &device_id(), &kid_of(&server))
}

/// An uplink frame around any plaintext (to carry records a sender
/// shouldn't make).
fn seal_raw(fcnt: u32, plaintext: &[u8], key: &[u8; 16]) -> Vec<u8> {
    let mut out = vec![0x21];
    out.extend(NET.to_be_bytes());
    out.extend(12u16.to_be_bytes());
    out.extend(fcnt.to_be_bytes());
    out.push(0);
    let sealed = ccm_seal(key, &nonce(&device_id(), fcnt), &out, plaintext);
    out.extend(sealed);
    out
}

fn link_file() -> String {
    let dev = named("device");
    let server = named("server");
    let shared = x25519(&dev, &server.verifying_key());
    let fkey = test_frame_key();
    let recs = records();
    let pick = |n: &str| recs.iter().find(|(name, _)| *name == n).unwrap().1.clone();
    let status_only = vec![pick("status")];
    let several = vec![pick("status_stale_no_battery"), pick("cue_warn"), pick("ack_applied")];
    let link = Link { got: vec![0x1a2b_3c4d], need: vec![Need { msg: 0x5e6f_7081, from: 1, missing: vec![0b101] }], anchor: None };
    let up = |name: &str, fcnt: u32, link: &Link, records: &[Record]| {
        json!({
            "name": name, "net": NET, "src": 12, "fcnt": fcnt, "link": link_json(link),
            "records": records.iter().map(Record::to_json).collect::<Vec<_>>(),
            "hex": hex(&seal_uplink(NET, 12, fcnt, link, records, &fkey, &device_id())),
        })
    };
    let standby = Link { anchor: Some([76, 3]), ..Link::default() };
    let mut uplinks = vec![
        up("status_only", 77, &Link::default(), &status_only),
        up("three_records_and_a_link_section", 78, &link, &several),
        up("standby_anchor_states_itself", 79, &standby, &[pick("anchor")]),
    ];
    for (i, (name, r)) in device_parts().into_iter().enumerate() {
        uplinks.push(up(&name, 83 + i as u32, &Link::default(), &[r]));
    }
    // A frame whose middle record has a float: its neighbours survive.
    let mut plain = vec![0x83];
    plain.extend(pick("status").encode());
    plain.extend([0x83, 0x01, 0x19, 0x11, 0x3d, 0xf9, 0x3c, 0x00]);
    plain.extend(pick("cue_warn").encode());
    uplinks.push(json!({
        "name": "one_malformed_record_among_valid", "net": NET, "src": 12, "fcnt": 82, "link": link_json(&Link::default()),
        "records": [pick("status").to_json(), Json::Null, pick("cue_warn").to_json()],
        "hex": hex(&seal_raw(82, &plain, &fkey)),
    }));

    let beacon = Beacon {
        net: NET,
        anchor: 1,
        minute: (T0 / 60) as u32,
        bver: Some(57),
        pending: vec![12],
        msgs: vec![Announce { msg: 0x1a2b_3c4d, count: 2, dst: BROADCAST, message_type: 1 }],
        heard: Beacon::heard_from([0, 3, 11]),
        state: Some([76, 3, 1]),
        handon: Some((2, (T0 / 60) as u32 + 5)),
        term: 7,
    };
    let quiet = Beacon { net: NET, anchor: 1, minute: (T0 / 60) as u32 + 1, term: 7, ..Default::default() };
    let b = |name: &str, bytes: Vec<u8>, json: Json, expect: &str| json!({"name": name, "json": json, "hex": hex(&bytes), "expect": expect});
    let beacons = vec![
        b("beacon", beacon.encode(&BEACON_KEY), beacon_json(&beacon), "ok"),
        b("quiet_minute", quiet.encode(&BEACON_KEY), beacon_json(&quiet), "ok"),
        b("forged_with_another_key", Beacon { heard: Beacon::heard_from(0..300), ..beacon.clone() }.encode(b"not-the-herd-key"), Json::Null, "bad_sig"),
    ];

    // A herd boundary in fragments of 64 bytes, received out of order with one lost.
    let msg = command(57, Body::Boundary(Boundary::from_lonlat(&rect(0.0, 0.0, 400.0, 120.0), &[rect(150.0, 30.0, 40.0, 40.0)]))).sign(&server);
    let frags = split(&msg, 64, NET, BROADCAST);
    let mut r = Reassembly::new(frags[0].msg, frags[0].count).unwrap();
    r.add(&frags[2]);
    r.add(&frags[0]);
    let need = r.need(8).unwrap();
    let hostile = FragmentFrame { net: NET, dst: BROADCAST, msg: 0x0bad_f00d, idx: 0, count: u16::MAX, data: vec![0; 8] };
    let fragments = vec![
        json!({
            "name": "boundary_in_64_byte_fragments",
            "message": hex(&msg), "max_data": 64, "msg": frags[0].msg,
            "frames": frags.iter().map(|f| hex(&f.encode())).collect::<Vec<_>>(),
            "received": [2, 0], "need": {"from": need.0, "missing": hex(&need.1)}, "expect": "ok",
        }),
        json!({
            "name": "count_past_a_command", "message": Json::Null, "max_data": 8, "msg": hostile.msg,
            "frames": [hex(&hostile.encode())], "received": [], "need": Json::Null, "expect": "refused",
        }),
    ];

    let p = &US915_500;
    let mut slots = Vec::new();
    for (short, cycle, groups, fast) in [
        (1u16, 5u32, 1u32, false),
        (12, 5, 1, false),
        (300, 5, 1, false),
        (12, 5, 1, true),
        (301, 5, 2, false),
        (1, 5, 2, true),
        (301, 5, 2, true),
        (612, 5, 3, true),
        (1200, 5, 5, true),
        (12, 1, 1, false),
    ] {
        let s = p.slot(short).unwrap();
        let sched = Schedule::new(Some(cycle * 60), Some(groups)).unwrap();
        let minutes: Vec<u32> = (0..10).map(|i| (T0 / 60) as u32 + i).collect();
        slots.push(json!({
            "short": short, "cycle_min": cycle, "groups": groups, "fast": fast, "k": s.k, "g": s.g, "start_ms": p.slot_start_ms(s.k),
            "minutes": minutes, "sends": minutes.iter().map(|m| p.sends(short, *m, sched, fast)).collect::<Vec<_>>(),
        }));
    }
    let airtime: Vec<Json> = [(7u8, 125u32, 20usize), (8, 500, 44), (8, 500, 128), (8, 500, 222), (10, 125, 50)]
        .iter()
        .map(|(sf, bw, bytes)| json!({"sf": sf, "bw_khz": bw, "bytes": bytes, "us": Plan { sf: *sf, bw_khz: *bw, ..*p }.airtime_us(*bytes)}))
        .collect();

    let hello_bytes = hello().sign(&dev);
    let hellos = vec![json!({"name": "hello", "net": 0, "hex": hex(&hello_frame(0, &hello_bytes)), "signed": hex(&hello_bytes)})];

    let anchor = named("anchor");
    let batch = Batch {
        seq: 9,
        sent_at: T0 + 125,
        herd: Some(HERD.to_vec()),
        have: 3,
        frames: vec![
            Relayed {
                frame: seal_uplink(NET, 12, 77, &Link::default(), &status_only, &fkey, &device_id()),
                rx: Some(Rx { at: T0 + 121, rssi_dbm: -97, snr_db: 6 }),
            },
            Relayed { frame: seal_uplink(NET, 12, 78, &link, &several, &fkey, &device_id()), rx: Some(Rx { at: T0 + 122, rssi_dbm: -101, snr_db: 4 }) },
        ],
    };
    let reply = Reply {
        ack: 9,
        down: vec![
            Downlink { id: 4, to: None, message: msg.clone() },
            Downlink {
                id: 5,
                to: Some(device_id()),
                message: Command { device: Some(device_id()), ..command(4, Body::Config(Config { report_s: 300, ..Default::default() })) }.sign(&server),
            },
        ],
        next_s: Some(60),
        time: Some(T0 + 126),
        drop: vec![2],
        next_anchor: Some(2),
    };
    let batches = vec![json!({
        "name": "batch_and_reply", "signer": "anchor", "hex": hex(&batch.sign(&anchor)),
        "frames": batch.frames.iter().map(|f| hex(&f.frame)).collect::<Vec<_>>(),
        "reply": hex(&reply.encode()),
    })];

    let header = [
        ("keys", keys_header()),
        (
            "frame_key",
            json!({"device": "device", "server": "server", "device_id": hex(&device_id()), "server_kid": hex(&kid_of(&server)), "shared": hex(&shared), "key": hex(&fkey)}),
        ),
        ("beacon_key", json!(hex(&BEACON_KEY))),
        (
            "plan",
            json!({"id": p.id, "slots": p.slots, "slot_ms": p.slot_ms, "uplink_ms": p.uplink_ms, "max_uplink": p.max_uplink, "max_downlink": p.max_downlink, "max_record": MAX_RECORD_BYTES}),
        ),
    ];
    file(
        &header,
        &[
            ("uplinks", uplinks),
            ("beacons", beacons),
            ("fragments", fragments),
            ("slots", slots),
            ("airtime", airtime),
            ("hellos", hellos),
            ("batches", batches),
            ("joins", joins()),
            ("replay", replay_cases()),
        ],
    )
}

/// Two unclaimed collars join one herd over LoRa at once (`lora.md` §6):
/// hellos with net 0, relayed in a batch; the server's binds and
/// memberships, each to its device; the anchor sends them as fragments to
/// dst 0; each collar keeps only what names it, then sends its first
/// sealed status with `seq` 0 and `fcnt` 0.
fn joins() -> Vec<Json> {
    let farm = named("next_server");
    let anchor = named("anchor");
    let mut out = Vec::new();
    let mut downs = Vec::new();
    let mut frames = Vec::new();
    for (i, (name, nonce, short)) in [("device", NONCE, 12u16), ("device2", *b"NONCE002", 13)].into_iter().enumerate() {
        let dk = named(name);
        let id = kid_of(&dk);
        let h = hello_of(name, nonce).sign(&dk);
        frames.push(Relayed { frame: hello_frame(0, &h), rx: Some(Rx { at: T0 + 56 + i as u64, rssi_dbm: -95, snr_db: 7 }) });
        let proof = bind_proof(&claim_key(CLAIM_CODE, &id), &id, &nonce, farm.verifying_key().as_bytes(), HERD);
        let bind = Command {
            device: Some(id),
            ..command(1, Body::Bind(Bind { server_key: farm.verifying_key().to_bytes(), proof, nonce, endpoint: None, owner_key: None, endpoint_pin: None }))
        }
        .sign(&farm);
        let shared = x25519(&farm, &dk.verifying_key());
        let wrapped = wrap_beacon_key(&wrap_key(&shared, &id, &kid_of(&farm)), &id, 1, HERD, NET, &BEACON_KEY);
        let membership = Command {
            device: Some(id),
            ..command(
                1,
                Body::Membership(Membership {
                    short,
                    role: Role::Node,
                    net: NET,
                    plan: Some(1),
                    cycle_s: Some(300),
                    rank: None,
                    groups: None,
                    beacon_key: Some(wrapped),
                }),
            )
        }
        .sign(&farm);
        downs.push((id, bind, membership, short, dk));
    }
    let batch = Batch { seq: 10, sent_at: T0 + 60, herd: Some(HERD.to_vec()), have: 5, frames };
    let reply = Reply {
        ack: 10,
        down: downs
            .iter()
            .enumerate()
            .flat_map(|(i, (id, bind, membership, ..))| {
                [
                    Downlink { id: 6 + 2 * i as u64, to: Some(*id), message: bind.clone() },
                    Downlink { id: 7 + 2 * i as u64, to: Some(*id), message: membership.clone() },
                ]
            })
            .collect(),
        ..Default::default()
    };
    // Every message to a joiner goes out as fragments to dst 0.
    let air: Vec<FragmentFrame> = reply.down.iter().flat_map(|d| split(&d.message, 209, NET, UNCLAIMED)).collect();
    for (id, bind, membership, short, dk) in &downs {
        let fkey = frame_key(&x25519(dk, &farm.verifying_key()), id, &kid_of(&farm));
        let status = Record {
            seq: 0,
            t: T0 + 180,
            body: RecordBody::Status(Status {
                lon: -936_200_123,
                lat: 420_301_456,
                acc_dm: 18,
                state: Some(FenceState::Unknown),
                bver: 0,
                battery: Some(90),
                flags: None,
            }),
        };
        let mut counter = Counter::boot(None);
        let fcnt = counter.take(|_| true).unwrap();
        // What it keeps of the fragments it heard: messages naming it.
        let mut kept = Vec::new();
        let mut bound = false;
        let mut asm = op_protocol::v2::fragment::Assemblies::default();
        for f in &air {
            if let Some(m) = asm.add(f, T0 + 61) {
                // Unclaimed, it can open only binds; once bound, its server's commands.
                let c = command::open(&m, |k| (bound && *k == kid_of(&farm)).then(|| farm.verifying_key())).map(|r| r.command);
                if let Some(c) = c.ok().filter(|c| c.device == Some(*id)) {
                    bound |= matches!(c.body, Body::Bind(_));
                    kept.push(c.body.message_type().name());
                }
            }
        }
        out.push(json!({
            "device": hex(id), "short": short,
            "bind": hex(bind), "membership": hex(membership),
            "keeps": kept,
            "first_status": hex(&seal_uplink(NET, *short, fcnt, &Link::default(), &[status], &fkey, id)),
        }));
    }
    vec![json!({
        "name": "two_collars_join_at_once",
        "batch": hex(&batch.sign(&anchor)), "reply": hex(&reply.encode()),
        "air": air.iter().map(|f| hex(&f.encode())).collect::<Vec<_>>(),
        "collars": out,
    })]
}

/// A server's replay window for one device's frames.
fn replay_cases() -> Vec<Json> {
    let mut w = ReplayWindow::default();
    let fcnts = [100u32, 100, 90, 90, 300, 100, 299, 173, 172];
    let accepted: Vec<bool> = fcnts.iter().map(|f| w.accept(*f)).collect();
    vec![json!({"name": "late_and_repeated_frames", "fcnt": fcnts, "accepted": accepted})]
}

// ---------------------------------------------------------------- identity

fn install_json(s: &InstallState) -> Json {
    json!({
        "class": s.class.to_json(), "hw_rev": s.hw_rev, "security_counter": s.security_counter, "battery": s.battery,
        "now": s.now, "last_fix": s.last_fix, "fence": s.fence.map(|f| f.to_json()), "episode": s.episode, "pen": s.pen,
        "fast_until": s.fast_until, "staged": s.staged,
    })
}

fn install_from(j: &Json) -> InstallState {
    let opt_u64 = |k: &str| j[k].as_u64();
    InstallState {
        class: DeviceClass::from_name(j["class"].as_str().unwrap()).unwrap(),
        hw_rev: j["hw_rev"].as_str().unwrap().into(),
        security_counter: j["security_counter"].as_u64().unwrap() as u32,
        battery: j["battery"].as_u64().map(|b| b as u8),
        now: opt_u64("now"),
        last_fix: opt_u64("last_fix"),
        fence: j["fence"].as_str().and_then(FenceState::from_name),
        episode: j["episode"].as_bool().unwrap(),
        pen: j["pen"].as_bool().unwrap(),
        fast_until: opt_u64("fast_until"),
        staged: j["staged"].as_array().unwrap().iter().map(|t| t.as_u64().unwrap()).collect(),
    }
}

fn identity_file() -> String {
    let me = device_id();
    let ck = claim_key(CLAIM_CODE, &me);
    let farm = named("next_server");
    let codes: Vec<Json> = ["7Q4K-9M2X-H8D3-TV5W", "7q4k 9m2x h8d3 tv5w", "IL0O-0000-0000-0000", "7Q4K9M2XH8D3TV5", "7Q4K9M2XH8D3TV5U"]
        .iter()
        .map(|c| json!({"input": c, "normalized": normalize_claim_code(c)}))
        .collect();
    let card = Card { serial: "OCN-2611-00123".into(), device_key: named("device").verifying_key().to_bytes(), claim_code: CLAIM_CODE.into() };

    let now = T0 + 7_200;
    let install = |name: &str, s: InstallState, w: InstallWindow| json!({"name": name, "state": install_json(&s), "window": {"effective_at": w.effective_at, "offset_s": w.offset_s}, "expect": outcome(check_install(&manifest(), &s, &w))});
    let ok = InstallState {
        class: DeviceClass::Node,
        hw_rev: "N1.0".into(),
        security_counter: 2,
        battery: Some(80),
        now: Some(now),
        last_fix: Some(now - 60),
        fence: Some(FenceState::Inside),
        ..Default::default()
    };
    let open_window = InstallWindow::new(Some(T0 + 3600), Some(86_400), 1_800);
    let w = open_window;
    let installs = vec![
        install("installs", ok.clone(), w),
        install("other_board", InstallState { hw_rev: "N0.9".into(), ..ok.clone() }, w),
        install("station_class", InstallState { class: DeviceClass::Station, ..ok.clone() }, w),
        install("older_than_running", InstallState { security_counter: 3, ..ok.clone() }, w),
        install("battery_under_the_manifest_minimum", InstallState { battery: Some(49), ..ok.clone() }, w),
        install("battery_unknown", InstallState { battery: None, ..ok.clone() }, w),
        install("no_gnss_time", InstallState { now: None, ..ok.clone() }, w),
        install("before_its_moment_in_the_window", ok.clone(), InstallWindow::new(Some(T0 + 3600), Some(86_400), 40_000)),
        install("fix_too_old", InstallState { last_fix: Some(now - 900), ..ok.clone() }, w),
        install("in_the_warning_zone", InstallState { fence: Some(FenceState::Warning), ..ok.clone() }, w),
        install("outside", InstallState { fence: Some(FenceState::Outside), ..ok.clone() }, w),
        install("in_an_episode", InstallState { episode: true, ..ok.clone() }, w),
        install("in_a_pen", InstallState { pen: true, ..ok.clone() }, w),
        install("fast_window_running", InstallState { fast_until: Some(now + 600), ..ok.clone() }, w),
        install("a_strip_due_within_the_hour", InstallState { staged: vec![now + 1_800], ..ok.clone() }, w),
        install("a_strip_overdue_waiting_for_its_lease", InstallState { staged: vec![now - 600], ..ok.clone() }, w),
        install("a_strip_due_tomorrow", InstallState { staged: vec![now + 86_400], ..ok }, w),
    ];
    let tests: Vec<Json> = [(TestBoot::Passed, 0u8), (TestBoot::Failed, 0), (TestBoot::NoFix, 0), (TestBoot::NoFix, 2)]
        .iter()
        .map(|(t, before)| {
            let r = after_test(*t, *before);
            json!({"test": format!("{t:?}"), "no_fix_before": before, "then": match r {
                AfterTest::Confirm => json!("confirm"),
                AfterTest::RollBack { refuse } => json!({"roll_back": true, "refuse": refuse}),
            }})
        })
        .collect();

    // Trust updates applied in turn to the factory store.
    let mut store = trust_store();
    let store_json = |s: &TrustStore| json!({"trusted": s.trusted().iter().map(|k| hex(k)).collect::<Vec<_>>(), "counter": s.counter});
    let mut trust = Vec::new();
    let steps: [(&str, &str, TrustUpdate); 4] = [
        ("reserve_revokes_the_leaked_key_and_adds_a_new_reserve", "release2", trust_update()),
        ("the_revoked_key_signs_nothing", "release", TrustUpdate { counter: 2, ..Default::default() }),
        ("an_old_update_again", "release2", trust_update()),
        (
            "would_leave_no_release_key",
            "release2",
            TrustUpdate { counter: 2, revoke: vec![kid_of(&named("release2")), kid_of(&named("new_owner"))], add: vec![] },
        ),
    ];
    for (name, signer, u) in steps {
        let before = store_json(&store);
        let bytes = u.sign(&named(signer));
        let (env, read) = TrustUpdate::read(&bytes).unwrap();
        let r = store.apply(&env.kid, &read);
        trust.push(json!({"name": name, "signer": signer, "hex": hex(&bytes), "before": before, "expect": outcome(r), "after": store_json(&store)}));
    }

    // Counter reservations as they sit in flash.
    let mut journal = Journal::default();
    let mut counters = Vec::new();
    for value in [64u32, 128, 192] {
        let (slot, data) = journal.next_write(value);
        journal.copies[slot] = data;
        counters.push(json!({"name": format!("reserved_{value}"), "copies": [hex(&journal.copies[0]), hex(&journal.copies[1])], "durable": journal.read(), "resume_at": Counter::boot(journal.read()).peek()}));
    }
    let mut torn = journal.clone();
    let (slot, data) = torn.next_write(256);
    torn.copies[slot][..6].copy_from_slice(&data[..6]);
    counters.push(json!({"name": "write_of_256_torn_by_a_power_cut", "copies": [hex(&torn.copies[0]), hex(&torn.copies[1])], "durable": torn.read(), "resume_at": Counter::boot(torn.read()).peek()}));

    // Zones, overlaps and activation.
    let z = zones();
    let ring = |x, y, w, h| Boundary::from_lonlat(&rect(x, y, w, h), &[]).outer;
    let overlap_cases: Vec<Json> = [
        ("apart", ring(0.0, 0.0, 400.0, 120.0), vec![], false),
        ("sharing_an_edge", ring(440.0, 0.0, 60.0, 40.0), vec![], false),
        ("crossing", ring(450.0, 0.0, 200.0, 120.0), vec![], true),
        ("holding_the_whole_zone", ring(400.0, -50.0, 300.0, 200.0), vec![], true),
        ("zone_carved_out_as_a_hole", ring(400.0, -50.0, 300.0, 200.0), vec![z.zones[0].clone()], false),
    ]
    .into_iter()
    .map(|(name, outer, holes, want)| {
        assert_eq!(overlaps(&outer, &holes, &z.zones[0]), want, "{name}");
        json!({"name": name, "outer": outer, "holes": holes, "zone": z.zones[0], "overlaps": want})
    })
    .collect();
    let activation = |name: &str, c: Command, state: &PolicyState, now: Option<u64>| {
        let a = state.may_activate(&c, now);
        json!({"name": name, "boundary": hex(&c.sign(&named("server"))), "held_epoch": state.epoch(), "lease": state.lease.map(|l| json!({"epoch": l.epoch, "until": l.until, "digest": hex(&l.digest)})), "now": now,
               "then": match a { Activation::Activate => json!("activate"), Activation::Wait(w) => json!(format!("wait_{w:?}").to_lowercase()), Activation::Reject(r) => json!(r.name()) }})
    };
    let staged = |outer, policy| Command {
        effective_at: Some(T0 + 86_400),
        ..command(80, Body::Boundary(Boundary { outer, policy: Some(policy), ..Default::default() }))
    };
    let online = PolicyState { zones: Some(z.clone()), lease: Some(Lease { until: T0 + 90_000, ..lease() }) };
    let offline = PolicyState {
        zones: Some(Zones { zones: vec![], epoch: 2 }),
        lease: Some(Lease { epoch: 2, until: T0 + 7_200, digest: Zones { zones: vec![], epoch: 2 }.digest() }),
    };
    let tomorrow = Some(T0 + 86_400);
    let activations = vec![
        activation("clear_strip_under_a_current_lease", staged(ring(0.0, 0.0, 400.0, 120.0), EPOCH), &online, tomorrow),
        activation("strip_over_a_zone_it_holds", staged(ring(450.0, 0.0, 200.0, 120.0), 2), &online, tomorrow),
        activation("offline_lease_ran_out", staged(ring(450.0, 0.0, 200.0, 120.0), 2), &offline, tomorrow),
        activation("names_an_epoch_it_lacks", staged(ring(0.0, 0.0, 400.0, 120.0), EPOCH), &offline, tomorrow),
        activation("no_clock", staged(ring(0.0, 0.0, 400.0, 120.0), EPOCH), &online, None),
        activation(
            "immediate_pen_needs_no_lease",
            command(81, Body::Boundary(Boundary { outer: ring(-60.0, 0.0, 50.0, 50.0), policy: Some(EPOCH), ..Default::default() })),
            &PolicyState { zones: Some(z.clone()), lease: None },
            None,
        ),
    ];

    let header = [
        ("keys", keys_header()),
        ("device_id", json!(hex(&me))),
        (
            "claim",
            json!({
                "code": CLAIM_CODE, "device_id": hex(&me), "claim_key": hex(&ck), "claim_check": hex(&claim_check(&ck)),
                "nonce": hex(&NONCE), "server_key": hex(farm.verifying_key().as_bytes()), "herd": hex(HERD),
                "proof": hex(&bind_proof(&ck, &me, &NONCE, farm.verifying_key().as_bytes(), HERD)),
            }),
        ),
        ("card", json!({"text": card.text(), "serial": card.serial, "device_key": hex(&card.device_key), "claim_code": card.claim_code})),
        (
            "manifest",
            json!({"hex": hex(&manifest().sign(&named("release"))), "json": manifest().to_json(), "min_battery_floor": op_protocol::v2::firmware::MIN_BATTERY_FLOOR}),
        ),
        ("trust_store", json!({"keys": ["release", "release2"], "slots": op_protocol::v2::firmware::TRUST_SLOTS})),
        ("zones", json!({"hex": hex(&command(11, Body::Zones(z.clone())).sign(&named("server"))), "epoch": z.epoch, "digest": hex(&z.digest())})),
        ("v1_herd", json!({"v1": "herd_01JB3K9M2XH8D3TV5W7Q4K9M2X", "v2": hex(&herd_from_v1("herd_01JB3K9M2XH8D3TV5W7Q4K9M2X"))})),
    ];
    file(
        &header,
        &[
            ("claim_codes", codes),
            ("install", installs),
            ("test_boot", tests),
            ("trust_updates", trust),
            ("counters", counters),
            ("overlaps", overlap_cases),
            ("activation", activations),
        ],
    )
}

// ---------------------------------------------------------------- files

fn render() -> Vec<(&'static str, String)> {
    let header = [
        ("keys", keys_header()),
        ("device_id", json!(hex(&device_id()))),
        ("herd", json!(hex(HERD))),
        ("now", json!(T0 + 60)),
        ("nonce", json!(hex(&NONCE))),
        ("claim_code", json!(CLAIM_CODE)),
        ("limits", json!(CollarLimits::V0)),
        ("claimed_at", json!(CLAIMED_AT)),
        ("pending_handover", json!(hex(&command::digest(&handover_rotate())))),
        ("zones", json!(hex(&command(11, Body::Zones(zones())).sign(&named("server"))))),
        ("lease", json!(hex(&command(12, Body::Lease(lease())).sign(&named("server"))))),
        ("image_keys", json!(["release", "release2"])),
    ];
    vec![
        ("messages.json", {
            let docs: Vec<Json> = document_cases().iter().map(|d| serde_json::to_value(d).unwrap()).collect();
            let msgs: Vec<Json> = message_cases().iter().map(|m| serde_json::to_value(m).unwrap()).collect();
            file(&header, &[("commands", msgs), ("documents", docs)])
        }),
        ("records.json", file::<RecordCase>(&[("max_record", json!(MAX_RECORD_BYTES))], &[("records", record_cases())])),
        ("link.json", link_file()),
        ("identity.json", identity_file()),
    ]
}

fn writing() -> bool {
    std::env::var("OP_WRITE_VECTORS").is_ok_and(|v| v == "1")
}

#[test]
fn vectors_are_current() {
    for (name, text) in render() {
        let path = dir().join(name);
        if writing() {
            std::fs::create_dir_all(dir()).unwrap();
            std::fs::write(&path, &text).unwrap();
        } else {
            let on_disk = std::fs::read_to_string(&path).unwrap_or_default();
            assert!(on_disk == text, "v2/{name} is out of date: run OP_WRITE_VECTORS=1 cargo test -p op-protocol --test vectors_v2");
        }
    }
}

/// A file renders the same bytes whatever serde_json's map order (Astra 03
/// P1-12: the workspace build enables `preserve_order`, and the files
/// differed byte for byte though equal as JSON).
#[test]
fn rendering_doesnt_depend_on_map_order() {
    let a = json!({"b": 1, "a": {"d": 2, "c": [ {"f": 3, "e": 4} ]}});
    assert_eq!(compact(&a), r#"{"a":{"c":[{"e":4,"f":3}],"d":2},"b":1}"#);
}

fn read(name: &str) -> Json {
    let text = if writing() {
        render().into_iter().find(|(n, _)| *n == name).unwrap().1
    } else {
        std::fs::read_to_string(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
    };
    serde_json::from_str(&text).unwrap()
}

fn bytes(v: &Json) -> Vec<u8> {
    unhex(v.as_str().unwrap()).unwrap()
}

#[test]
fn commands_agree() {
    let f = read("messages.json");
    let cases: Vec<MessageCase> = serde_json::from_value(f["commands"].clone()).unwrap();
    assert!(cases.len() >= 50);
    for c in &cases {
        let b = unhex(&c.hex).unwrap();
        assert_eq!(outcome(collar_check(&b, &c.state, c.clock)), c.expect, "{}", c.name);
        let decoded = cose::Sign1::parse(&b).ok().and_then(|s| s.payload_map().ok()).and_then(|m| Command::from_payload(&m).ok());
        assert_eq!(decoded.as_ref().map_or(Json::Null, Command::to_json), c.json, "{}", c.name);
        // Re-encoding a decoded payload gives back the bytes signed (a
        // refused one may carry keys this version ignores).
        if let Some(cmd) = decoded.filter(|_| c.expect == "ok") {
            assert_eq!(cmd.payload(), cose::Sign1::parse(&b).unwrap().payload, "{}", c.name);
        }
    }
    for t in MessageType::ALL.iter().filter(|t| t.is_command()) {
        assert!(cases.iter().any(|c| c.json["type"] == t.name() && c.expect == "ok"), "no accepted case for {t}");
    }
}

#[test]
fn documents_agree() {
    let f = read("messages.json");
    let key_of = |name: &str| named(name).verifying_key();
    for d in f["documents"].as_array().unwrap() {
        let b = bytes(&d["hex"]);
        let signer: VerifyingKey = key_of(d["signer"].as_str().unwrap());
        let env = cose::Sign1::parse(&b).unwrap();
        env.verify(&signer).unwrap();
        let json = match d["json"]["type"].as_str().unwrap() {
            "factory" => FactoryRecord::read(&b).unwrap().1.to_json(),
            "manifest" => Manifest::read(&b).unwrap().1.to_json(),
            "trust_update" => TrustUpdate::read(&b).unwrap().1.to_json(),
            "hello" => {
                let (id, h) = Hello::open(&b).unwrap();
                assert_eq!(id, kid(&h.device_key));
                h.to_json()
            }
            other => panic!("{other}"),
        };
        assert_eq!(json, d["json"], "{}", d["name"]);
    }
}

#[test]
fn records_agree() {
    let f = read("records.json");
    let cases: Vec<RecordCase> = serde_json::from_value(f["records"].clone()).unwrap();
    for c in &cases {
        let b = unhex(&c.hex).unwrap();
        let got = cbor::decode_untagged(&b).map_err(|_| Reject::BadCbor).and_then(|v| Record::from_value(&v));
        assert_eq!(outcome(got.as_ref().map(|_| ()).map_err(|e| *e)), c.expect, "{}", c.name);
        if let Ok(r) = got {
            assert_eq!(r.to_json(), c.json, "{}", c.name);
            assert_eq!(hex(&r.encode()) == c.hex, c.same_bytes, "{} re-encodes byte for byte", c.name);
            // Astra 01 F02: every record a sender makes fits one frame.
            if c.same_bytes {
                assert!(b.len() <= MAX_RECORD_BYTES, "{}: {} bytes", c.name, b.len());
            }
        }
    }
    for t in op_protocol::v2::registry::RecordType::ALL {
        assert!(cases.iter().any(|c| c.json["type"] == t.name()), "no case for {t}");
    }
    // The device snapshot's parts merge back into it.
    let parts: Vec<DeviceInfo> = cases
        .iter()
        .filter(|c| c.name.starts_with("device_part_"))
        .map(|c| match Record::from_value(&cbor::decode(&unhex(&c.hex).unwrap()).unwrap()).unwrap().body {
            RecordBody::Device(d) => d,
            _ => panic!(),
        })
        .collect();
    assert_eq!(DeviceInfo::merge(&parts), Some(device_snapshot()));
}

#[test]
fn link_agrees() {
    let f = read("link.json");
    let fk = &f["frame_key"];
    let dev_id: [u8; 8] = bytes(&fk["device_id"]).try_into().unwrap();
    let shared: [u8; 32] = x25519(&named("server"), &named("device").verifying_key());
    assert_eq!(hex(&shared), fk["shared"], "the server side agrees");
    let key: [u8; 16] = bytes(&fk["key"]).try_into().unwrap();
    assert_eq!(frame_key(&shared, &dev_id, &kid_of(&named("server"))), key);
    for u in f["uplinks"].as_array().unwrap() {
        let frame = bytes(&u["hex"]);
        assert!(frame.len() <= US915_500.max_uplink, "{} is {} bytes", u["name"], frame.len());
        let up = Uplink::parse(&frame).unwrap();
        assert_eq!((up.net, up.src, u64::from(up.fcnt)), (NET, 12, u["fcnt"].as_u64().unwrap()));
        assert_eq!(link_json(&up.link), u["link"]);
        let recs: Vec<Json> = up.open(&key, &dev_id).unwrap().into_iter().map(|r| r.map_or(Json::Null, |r| r.to_json())).collect();
        assert_eq!(Json::Array(recs), u["records"], "{}", u["name"]);
    }
    let beacon_key: [u8; 16] = bytes(&f["beacon_key"]).try_into().unwrap();
    for b in f["beacons"].as_array().unwrap() {
        let got = Beacon::decode(&bytes(&b["hex"]), &[beacon_key]);
        assert_eq!(outcome(got.as_ref().map(|_| ()).map_err(|e| *e)), b["expect"], "{}", b["name"]);
        if let Ok(beacon) = got {
            assert_eq!(beacon_json(&beacon), b["json"]);
        }
    }
    for g in f["fragments"].as_array().unwrap() {
        let frames: Vec<FragmentFrame> = g["frames"].as_array().unwrap().iter().map(|h| FragmentFrame::decode(&bytes(h)).unwrap()).collect();
        let Some(mut r) = Reassembly::new(frames[0].msg, frames[0].count) else {
            assert_eq!(g["expect"], "refused", "{}", g["name"]);
            continue;
        };
        for i in g["received"].as_array().unwrap() {
            r.add(&frames[i.as_u64().unwrap() as usize]);
        }
        let need = r.need(8).unwrap();
        assert_eq!(json!({"from": need.0, "missing": hex(&need.1)}), g["need"]);
        for f in &frames {
            r.add(f);
        }
        assert_eq!(hex(&r.message().unwrap()), g["message"]);
    }
    let p = &US915_500;
    for s in f["slots"].as_array().unwrap() {
        let short = s["short"].as_u64().unwrap() as u16;
        let slot = p.slot(short).unwrap();
        let sched = Schedule::new(Some(s["cycle_min"].as_u64().unwrap() as u32 * 60), Some(s["groups"].as_u64().unwrap() as u32)).unwrap();
        assert_eq!((u64::from(slot.k), u64::from(slot.g)), (s["k"].as_u64().unwrap(), s["g"].as_u64().unwrap()));
        assert_eq!(u64::from(p.slot_start_ms(slot.k)), s["start_ms"].as_u64().unwrap());
        for (m, want) in s["minutes"].as_array().unwrap().iter().zip(s["sends"].as_array().unwrap()) {
            assert_eq!(p.sends(short, m.as_u64().unwrap() as u32, sched, s["fast"].as_bool().unwrap()), want.as_bool().unwrap());
        }
    }
    for a in f["airtime"].as_array().unwrap() {
        let plan = Plan { sf: a["sf"].as_u64().unwrap() as u8, bw_khz: a["bw_khz"].as_u64().unwrap() as u32, ..*p };
        assert_eq!(plan.airtime_us(a["bytes"].as_u64().unwrap() as usize), a["us"].as_u64().unwrap());
    }
    for h in f["hellos"].as_array().unwrap() {
        let frame = bytes(&h["hex"]);
        assert!(op_protocol::v2::frame::accepts(&frame, Some(NET), true), "an anchor takes a net-0 hello");
        let (id, _) = Hello::open(hello_signed(&frame).unwrap()).unwrap();
        assert_eq!(id, dev_id);
    }
    for b in f["batches"].as_array().unwrap() {
        let anchor = named("anchor").verifying_key();
        let (who, batch) = Batch::open(&bytes(&b["hex"]), |k| (*k == kid(anchor.as_bytes())).then_some(anchor)).unwrap();
        assert_eq!(who, kid(anchor.as_bytes()));
        let frames: Vec<Json> = batch.frames.iter().map(|f| json!(hex(&f.frame))).collect();
        assert_eq!(Json::Array(frames), b["frames"]);
        Reply::decode(&bytes(&b["reply"])).unwrap();
    }
    for j in f["joins"].as_array().unwrap() {
        let farm = named("next_server");
        for c in j["collars"].as_array().unwrap() {
            let id: [u8; 8] = bytes(&c["device"]).try_into().unwrap();
            let dk = if id == device_id() { named("device") } else { named("device2") };
            // The beacon key unwraps for this collar alone.
            let m = command::open(&bytes(&c["membership"]), |_| Some(farm.verifying_key())).unwrap();
            let Body::Membership(mem) = &m.command.body else { panic!() };
            let wk = wrap_key(&x25519(&dk, &farm.verifying_key()), &id, &kid_of(&farm));
            assert_eq!(unwrap_beacon_key(&wk, &id, m.command.version, HERD, NET, &mem.beacon_key.unwrap()), Some(BEACON_KEY));
            assert_eq!(c["keeps"], json!(["bind", "membership"]));
            let fkey = frame_key(&x25519(&dk, &farm.verifying_key()), &id, &kid_of(&farm));
            let up = Uplink::parse(&bytes(&c["first_status"])).unwrap();
            assert_eq!(up.fcnt, 0);
            assert!(up.open(&fkey, &id).unwrap()[0].as_ref().is_ok_and(|r| r.seq == 0));
        }
    }
    for r in f["replay"].as_array().unwrap() {
        let mut w = ReplayWindow::default();
        let got: Vec<bool> = r["fcnt"].as_array().unwrap().iter().map(|f| w.accept(f.as_u64().unwrap() as u32)).collect();
        assert_eq!(json!(got), r["accepted"]);
    }
}

#[test]
fn identity_agrees() {
    let f = read("identity.json");
    for c in f["claim_codes"].as_array().unwrap() {
        assert_eq!(json!(normalize_claim_code(c["input"].as_str().unwrap())), c["normalized"]);
    }
    let cl = &f["claim"];
    let me: [u8; 8] = bytes(&cl["device_id"]).try_into().unwrap();
    let ck = claim_key(cl["code"].as_str().unwrap(), &me);
    assert_eq!(hex(&ck), cl["claim_key"]);
    assert_eq!(hex(&claim_check(&ck)), cl["claim_check"]);
    let server_key: [u8; 32] = bytes(&cl["server_key"]).try_into().unwrap();
    let nonce: [u8; 8] = bytes(&cl["nonce"]).try_into().unwrap();
    assert_eq!(hex(&bind_proof(&ck, &me, &nonce, &server_key, &bytes(&cl["herd"]))), cl["proof"]);
    let card = Card::parse(f["card"]["text"].as_str().unwrap()).unwrap();
    assert_eq!(hex(&card.device_key), f["card"]["device_key"]);
    assert_eq!(hex(&kid(&card.device_key)), f["device_id"]);
    let (_, m) = Manifest::read(&bytes(&f["manifest"]["hex"])).unwrap();
    for c in f["install"].as_array().unwrap() {
        let w = InstallWindow { effective_at: c["window"]["effective_at"].as_u64(), offset_s: c["window"]["offset_s"].as_u64().unwrap() };
        assert_eq!(outcome(check_install(&m, &install_from(&c["state"]), &w)), c["expect"], "{}", c["name"]);
    }
    let mut store = trust_store();
    for t in f["trust_updates"].as_array().unwrap() {
        let (env, u) = TrustUpdate::read(&bytes(&t["hex"])).unwrap();
        env.verify(&named(t["signer"].as_str().unwrap()).verifying_key()).unwrap();
        assert_eq!(outcome(store.apply(&env.kid, &u)), t["expect"], "{}", t["name"]);
        assert_eq!(json!(store.trusted().iter().map(|k| hex(k)).collect::<Vec<_>>()), t["after"]["trusted"]);
    }
    for c in f["counters"].as_array().unwrap() {
        let copies: Vec<[u8; 12]> = c["copies"].as_array().unwrap().iter().map(|h| bytes(h).try_into().unwrap()).collect();
        let j = Journal { copies: [copies[0], copies[1]] };
        assert_eq!(json!(j.read()), c["durable"], "{}", c["name"]);
        assert_eq!(json!(Counter::boot(j.read()).peek()), c["resume_at"]);
    }
    let zr = command::open(&bytes(&f["zones"]["hex"]), |_| Some(named("server").verifying_key())).unwrap();
    let Body::Zones(z) = zr.command.body else { panic!() };
    assert_eq!(hex(&z.digest()), f["zones"]["digest"]);
    for o in f["overlaps"].as_array().unwrap() {
        let ring = |v: &Json| -> Vec<[i32; 2]> { serde_json::from_value(v.clone()).unwrap() };
        let holes: Vec<Vec<[i32; 2]>> = serde_json::from_value(o["holes"].clone()).unwrap();
        assert_eq!(json!(overlaps(&ring(&o["outer"]), &holes, &ring(&o["zone"]))), o["overlaps"], "{}", o["name"]);
    }
    for a in f["activation"].as_array().unwrap() {
        let c = command::open(&bytes(&a["boundary"]), |_| Some(named("server").verifying_key())).unwrap().command;
        let held = a["held_epoch"].as_u64().unwrap() as u32;
        let zones = if held == z.epoch { z.clone() } else { Zones { zones: vec![], epoch: held } };
        let lease = a["lease"].as_object().map(|l| Lease {
            epoch: l["epoch"].as_u64().unwrap() as u32,
            until: l["until"].as_u64().unwrap(),
            digest: bytes(&l["digest"]).try_into().unwrap(),
        });
        let then = match (PolicyState { zones: Some(zones), lease }).may_activate(&c, a["now"].as_u64()) {
            Activation::Activate => json!("activate"),
            Activation::Wait(w) => json!(format!("wait_{w:?}").to_lowercase()),
            Activation::Reject(r) => json!(r.name()),
        };
        assert_eq!(then, a["then"], "{}", a["name"]);
    }
    assert_eq!(hex(&herd_from_v1(f["v1_herd"]["v1"].as_str().unwrap())), f["v1_herd"]["v2"]);
}

/// Every file under `vectors/`, relative paths, sorted.
fn tree(root: &Path) -> Vec<PathBuf> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
        for e in std::fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(root, &p, out);
            } else {
                out.push(p.strip_prefix(root).unwrap().to_path_buf());
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out.sort();
    out
}

#[test]
fn copy_matches_opencollar() {
    let ours = Path::new(env!("CARGO_MANIFEST_DIR")).join("vectors");
    let theirs = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../opencollar/protocol/vectors");
    if !theirs.is_dir() {
        eprintln!("no opencollar checkout next to this one; skipping the copy check");
        return;
    }
    let (a, b) = (tree(&ours), tree(&theirs));
    assert_eq!(a, b, "crates/op-protocol/vectors and opencollar/protocol/vectors hold different files");
    for rel in a {
        let same = std::fs::read(ours.join(&rel)).unwrap() == std::fs::read(theirs.join(&rel)).unwrap();
        assert!(same, "{} differs from opencollar/protocol/vectors: copy it over (vectors/README.md)", rel.display());
    }
}

// ---------------------------------------------------------------- budget

/// Radio figures assumed for energy: SX1262 datasheet typicals at 3.3 V,
/// **not measured** on our board (`v2/README.md` §Data budget).
const TX_MA: f64 = 118.0; // +22 dBm PA setting, which +20 dBm conducted needs with module losses
const RX_MA: f64 = 5.3; // LoRa receive, 500 kHz, DC-DC
const VOLTS: f64 = 3.3;
/// Receive guard around each expected beacon, ms (clock error both sides).
const BEACON_GUARD_MS: f64 = 20.0;

/// The numbers behind the data budget in `opencollar/protocol/v2/README.md`:
/// what a node sends and hears in a day, its radio energy under the stated
/// assumptions, and what an anchor carries over LTE for a herd of 250.
/// Every quantity counts all the traffic named, not status alone (Astra 01
/// F02, F17).
#[test]
fn data_budget() {
    let p = &US915_500;
    let recs = records();
    let pick = |n: &str| recs.iter().find(|(name, _)| *name == n).unwrap().1.clone();
    let key = [1u8; 16];
    let frame = |rs: &[Record]| seal_uplink(NET, 12, 1, &Link::default(), rs, &key, &device_id()).len();
    let air_ms = |bytes: usize| p.airtime_us(bytes) as f64 / 1000.0;

    // Every frame a node sends fits a slot.
    let status = frame(&[pick("status")]);
    let fixes: Vec<Fix> =
        (0..10).map(|i| Fix { t: T0 + 60 * i, lon: -936_200_000 + (i as i32 * 37) % 900, lat: 420_300_000 - (i as i32 * 53) % 700, acc_dm: 18 }).collect();
    let block = frame(&[Record { seq: 70_000, t: T0, body: RecordBody::Fixes(fixes) }]);
    let health = frame(&[pick("health")]);
    let device: Vec<usize> = device_parts().iter().map(|(_, r)| frame(std::slice::from_ref(r))).collect();
    let cue = frame(&[pick("cue_warn")]);
    let ack = frame(&[pick("ack_applied")]);
    for (name, b) in
        [("status", status), ("fix block", block), ("health", health), ("cue", cue), ("ack", ack)].into_iter().chain(device.iter().map(|d| ("device part", *d)))
    {
        assert!(b <= p.max_uplink, "{name} frame {b} B over {}", p.max_uplink);
        assert!(air_ms(b) + 2.0 * f64::from(p.guard_ms) <= f64::from(p.slot_ms), "{name} frame doesn't fit its slot");
    }
    assert!(status <= 48, "status frame {status} B");
    assert_eq!(UPLINK_OVERHEAD + 1 + MAX_RECORD_BYTES, p.max_uplink, "the record budget is the frame less its overhead");

    // A node's day: status every 5 min, health every 6 h, the device
    // snapshot daily, 4 cues and 6 acks, and 2 % of frames sent again.
    let sent: Vec<(usize, usize)> = vec![(288, status), (4, health), (4, cue), (6, ack)].into_iter().chain(device.iter().map(|d| (1, *d))).collect();
    let frames_day: f64 = sent.iter().map(|(n, _)| *n as f64).sum::<f64>() * 1.02;
    let bytes_day: f64 = sent.iter().map(|(n, b)| (n * b) as f64).sum::<f64>() * 1.02;
    let tx_s: f64 = sent.iter().map(|(n, b)| *n as f64 * air_ms(*b)).sum::<f64>() * 1.02 / 1000.0;
    let tracked_extra_s = 144.0 * air_ms(block) / 1000.0;
    // Listening: a beacon every minute that acknowledges ~50 of 250 nodes'
    // frames, with guard; plus one 150-byte strip fragment a day and a
    // lease (one fragment) every 15 minutes.
    let ack_beacon = Beacon {
        net: NET,
        anchor: 1,
        minute: 1,
        bver: Some(57),
        heard: Beacon::heard_from((0..300).step_by(6)),
        state: Some([76, 3, 1]),
        term: 7,
        ..Default::default()
    }
    .encode(&BEACON_KEY)
    .len();
    let quiet_beacon = Beacon { net: NET, anchor: 1, minute: 1, term: 7, ..Default::default() }.encode(&BEACON_KEY).len();
    let rx_beacons_s = 1440.0 * (air_ms(ack_beacon) + BEACON_GUARD_MS) / 1000.0;
    let lease_frag = 13 + command(12, Body::Lease(lease())).sign(&named("server")).len();
    let rx_down_s = (96.0 * (air_ms(lease_frag) + BEACON_GUARD_MS) + (air_ms(13 + 160) + BEACON_GUARD_MS)) / 1000.0;
    let rx_s = rx_beacons_s + rx_down_s;
    let joules = (tx_s * TX_MA + rx_s * RX_MA) / 1000.0 * VOLTS;
    let joules_tracked = joules + tracked_extra_s * TX_MA / 1000.0 * VOLTS;
    eprintln!(
        "node: frames status {status} B, fix block {block} B, health {health} B, device {device:?} B, cue {cue} B, ack {ack} B; beacons quiet {quiet_beacon} B, acking {ack_beacon} B, lease fragment {lease_frag} B"
    );
    eprintln!(
        "node/day: {frames_day:.0} frames, {bytes_day:.0} B, TX {tx_s:.1} s, RX {rx_s:.1} s (beacons {rx_beacons_s:.1} s), radio {joules:.1} J ({:.3} mW); with track every 10 min +{tracked_extra_s:.1} s TX, {joules_tracked:.1} J",
        joules / 86.4
    );

    // An anchor carrying 250 nodes: every frame above, relayed with radio
    // fields in a signed batch every 5 minutes, plus 2 KB of TLS/TCP/HTTP
    // per exchange and the replies (the herd's downlinks).
    let anchor = named("anchor");
    let relayed = |f: &[u8]| Relayed { frame: f.to_vec(), rx: Some(Rx { at: T0, rssi_dbm: -101, snr_db: 4 }) };
    let status_frame = seal_uplink(NET, 12, 70_000, &Link::default(), &[pick("status")], &key, &device_id());
    let per_batch = Batch { seq: 70_000, sent_at: T0, herd: Some(HERD.to_vec()), have: 1_000, frames: vec![relayed(&status_frame); 250] }.sign(&anchor).len();
    let extra_bytes: f64 = 250.0 * ((4 * (health + 10)) + 4 * (cue + 10) + 6 * (ack + 10) + device.iter().map(|d| d + 10).sum::<usize>()) as f64;
    let lte_day = 288.0 * (per_batch as f64 + 2048.0 + 200.0) + extra_bytes;
    eprintln!(
        "anchor/day for 250 nodes: batch of 250 status frames {per_batch} B; LTE {:.2} MB (status {:.2} MB + health, device, cues, acks {:.2} MB); per anchor {:.2}–{:.2} MB with 8–5 anchors",
        lte_day / 1e6,
        288.0 * (per_batch as f64 + 2248.0) / 1e6,
        extra_bytes / 1e6,
        lte_day / 8e6,
        lte_day / 5e6
    );
    assert!(bytes_day < 20_000.0 && joules < 30.0);
}
