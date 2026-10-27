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
use op_protocol::v2::command::{self, Bind, Boundary, Config, FirmwareUpdate, Handover, Member, Membership, Roster, Trust};
use op_protocol::v2::crypto::{frame_key, kid, x25519};
use op_protocol::v2::firmware::{InstallState, Manifest, check_install};
use op_protocol::v2::fragment::{Reassembly, split};
use op_protocol::v2::frame::{Announce, BROADCAST, Beacon, FragmentFrame, Link, Need, Uplink, hello_frame, hello_signed, seal_uplink};
use op_protocol::v2::identity::{Card, FactoryRecord, Hello, bind_proof, check_bind, claim_check, claim_key, normalize_claim_code};
use op_protocol::v2::json::{hex, unhex};
use op_protocol::v2::record::{self, Ack, Cue, DeviceInfo, Episode, FirmwareStatus, Fix, Record, RecordBody, SlotEntry, Status, flags};
use op_protocol::v2::registry::{AckStatus, CueKind, CueMode, DeviceClass, EpisodeOutcome, FenceState, FirmwareState, MessageType, Reject, Role};
use op_protocol::v2::tdma::US915_500;
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

/// Test keys: Ed25519 seeds of 32 consecutive bytes from `start`.
fn key(start: u8) -> SigningKey {
    SigningKey::from_bytes(&core::array::from_fn(|i| start.wrapping_add(i as u8)))
}

const KEYS: [(&str, u8); 8] =
    [("server", 0x00), ("next_server", 0x20), ("owner", 0x40), ("device", 0x60), ("anchor", 0x80), ("maker", 0xa0), ("release", 0xc0), ("new_owner", 0xe0)];

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

/// A file: header fields pretty, one compact case per line.
fn file<C: Serialize>(header: &[(&str, Json)], sections: &[(&str, Vec<C>)]) -> String {
    let mut out = String::from("{\n");
    for (k, v) in header {
        out.push_str(&format!("  \"{k}\": {},\n", serde_json::to_string(v).unwrap()));
    }
    let parts: Vec<String> = sections
        .iter()
        .map(|(name, cases)| {
            let lines: Vec<String> = cases.iter().map(|c| format!("    {}", serde_json::to_string(c).unwrap())).collect();
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

#[derive(Debug, Serialize, serde::Deserialize)]
struct MessageCase {
    name: String,
    /// `claimed` (the default collar in the header) or `unclaimed`.
    state: String,
    hex: String,
    /// The payload's JSON view; `null` when the bytes don't decode.
    json: Json,
    /// `ok`, or the reject code.
    expect: String,
}

/// Re-sign an arbitrary payload (for cases that aren't valid commands).
fn signed(payload: &Value, with: &SigningKey) -> Vec<u8> {
    cose::sign(&cbor::encode(payload), with)
}

fn message_cases() -> Vec<MessageCase> {
    let server = named("server");
    let me = device_id();
    let mut out: Vec<(&str, &str, Vec<u8>)> = Vec::new();
    let mut push = |name: &'static str, state: &'static str, bytes: Vec<u8>| out.push((name, state, bytes));

    push("boundary_herd", "claimed", command(57, Body::Boundary(strip())).sign(&server));
    let mut staged = Boundary::from_lonlat(&rect(0.0, 0.0, 400.0, 400.0), &[rect(150.0, 150.0, 40.0, 40.0)]);
    staged.warn_cm = Some(800);
    staged.hyst_cm = Some(150);
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
    let membership = |herd: &[u8]| Command {
        herd: herd.to_vec(),
        device: Some(me),
        ..command(2, Body::Membership(Membership { short: 12, role: Role::Node, net: NET, plan: Some(1), cycle_s: Some(300), rank: None }))
    };
    push("membership", "claimed", membership(HERD).sign(&server));
    push("membership_moves_the_collar_to_another_herd", "claimed", membership(OTHER_HERD).sign(&server));
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
    let handover = |reason, endpoint: Option<&str>| {
        Body::Handover(Handover {
            new_key: named("next_server").verifying_key().to_bytes(),
            deadline: T0 + 7 * 86_400,
            endpoint: endpoint.map(str::to_owned),
            endpoint_pin: None,
            reason: Some(reason),
        })
    };
    push("handover_rotate", "claimed", command(6, handover(1, None)).sign(&server));
    push("handover_by_the_owner_key", "claimed", command(7, handover(2, Some("https://other.example.org/collar/v2"))).sign(&named("owner")));
    push("confirm_by_the_new_key", "claimed", command(1, Body::Confirm { replaces: kid_of(&server) }).sign(&named("next_server")));
    push("owner_key_replaced", "claimed", command(2, Body::Owner { owner_key: named("new_owner").verifying_key().to_bytes() }).sign(&named("owner")));
    push(
        "firmware",
        "claimed",
        Command {
            effective_at: Some(T0 + 3600),
            ..command(8, Body::Firmware(FirmwareUpdate { manifest: manifest().sign(&named("release")), window_s: Some(86_400), url: None }))
        }
        .sign(&server),
    );
    push("resend", "claimed", Command { device: Some(me), ..command(9, Body::Resend { from: 100, to: 140 }) }.sign(&server));
    push("release", "claimed", Command { device: Some(me), ..command(10, Body::Release) }.sign(&server));
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
                    owner_key: Some(named("owner").verifying_key().to_bytes()),
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
    let mut tampered = command(57, Body::Boundary(strip())).sign(&server);
    let n = tampered.len();
    tampered[n - 70] ^= 0x01;
    push("tampered", "claimed", tampered);
    push("signed_by_an_unknown_key", "claimed", command(57, Body::Boundary(strip())).sign(&named("anchor")));
    push("boundary_signed_by_the_owner_key", "claimed", command(57, Body::Boundary(strip())).sign(&named("owner")));
    push("expired", "claimed", Command { expires_at: Some(T0), ..command(57, Body::Boundary(strip())) }.sign(&server));
    push("another_herd", "claimed", Command { herd: OTHER_HERD.to_vec(), ..command(57, Body::Boundary(strip())) }.sign(&server));
    push("another_collar", "claimed", Command { device: Some([0x13; 8]), ..command(57, Body::Boundary(strip())) }.sign(&server));
    push("confirm_by_the_old_key", "claimed", command(1, Body::Confirm { replaces: kid_of(&server) }).sign(&server));
    let mut payload = cbor::decode(&command(57, Body::Boundary(strip())).payload()).unwrap();
    if let Value::Map(m) = &mut payload {
        m.retain(|(k, _)| k.as_u64() != Some(1));
    }
    push("no_herd", "claimed", signed(&payload, &server));
    // version 57 as a 4-byte integer: valid CBOR, not deterministic.
    let mut long = command(57, Body::Boundary(strip())).payload();
    let at = long.windows(3).position(|w| w == [0x03, 0x18, 57]).unwrap();
    long.splice(at + 1..at + 3, [0x1a, 0, 0, 0, 57]);
    push("payload_not_deterministic", "claimed", cose::sign(&long, &server));
    let untagged = command(57, Body::Boundary(strip())).sign(&server)[1..].to_vec();
    push("untagged", "claimed", untagged);
    // Over 12 288 bytes; an unknown key would otherwise be ignored.
    let mut big = cbor::decode(&command(57, Body::Boundary(strip())).payload()).unwrap();
    if let Value::Map(m) = &mut big {
        m.push((Value::uint(99u8), Value::bytes(vec![0; 12_300])));
    }
    push("too_large", "claimed", signed(&big, &server));
    let bowtie = Boundary::from_lonlat(&[[-93.62, 42.03], [-93.61, 42.04], [-93.61, 42.03], [-93.62, 42.04]], &[]);
    push("self_intersecting", "claimed", command(57, Body::Boundary(bowtie)).sign(&server));
    push(
        "report_interval_too_short",
        "claimed",
        Command { device: Some(me), ..command(4, Body::Config(Config { report_s: 5, ..Default::default() })) }.sign(&server),
    );
    push(
        "short_address_zero",
        "claimed",
        Command {
            device: Some(me),
            ..command(2, Body::Membership(Membership { short: 0, role: Role::Node, net: NET, plan: None, cycle_s: None, rank: None }))
        }
        .sign(&server),
    );

    out.into_iter()
        .map(|(name, state, bytes)| {
            let json = cose::Sign1::parse(&bytes)
                .ok()
                .and_then(|s| s.payload_map().ok())
                .and_then(|m| Command::from_payload(&m).ok())
                .map_or(Json::Null, |c| c.to_json());
            MessageCase { name: name.into(), state: state.into(), hex: hex(&bytes), json, expect: outcome(collar_check(&bytes, state)) }
        })
        .collect()
}

fn outcome(r: Result<(), Reject>) -> String {
    r.map_or_else(|e| e.name().to_string(), |()| "ok".into())
}

/// The collar's whole check of a command, in the spec's order.
fn collar_check(bytes: &[u8], state: &str) -> Result<(), Reject> {
    let me = device_id();
    let claimed = state == "claimed";
    let trust = if claimed {
        Trust {
            server: Some(named("server").verifying_key()),
            pending: Some(named("next_server").verifying_key()),
            owner: Some(named("owner").verifying_key()),
        }
    } else {
        Trust::default()
    };
    let r = command::open(bytes, |k| trust.key_for(k))?;
    trust.authorize(&r)?;
    let c = &r.command;
    if let Body::Bind(b) = &c.body {
        check_bind(b, &c.herd, &me, &NONCE, CLAIM_CODE)?;
    }
    c.check_scope(&me, claimed.then_some(HERD), Some(T0 + 60))?;
    match &c.body {
        Body::Boundary(b) => b.check(&CollarLimits::V0),
        Body::Config(x) => x.check(false),
        Body::Membership(m) => m.check(),
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
        image_keys: vec![kid_of(&named("release"))],
        test: Some(IntMap(vec![(1, Value::text("fixture-1")), (2, Value::Bool(true)), (3, Value::bytes(vec![0xab; 32])), (4, Value::uint(T0 - 30 * 86_400))])),
        caps: vec![],
    }
}

fn hello() -> Hello {
    Hello {
        class: DeviceClass::Node,
        fw: "2.0.0".into(),
        nonce: Some(NONCE),
        caps: vec!["holes".into(), "slots".into(), "episodes".into()],
        device_key: named("device").verifying_key().to_bytes(),
    }
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
            "device",
            rec(
                4331,
                t,
                RecordBody::Device(DeviceInfo {
                    class: Some(DeviceClass::Node),
                    fw: Some("2.0.0".into()),
                    caps: vec!["holes".into(), "slots".into(), "collar_id".into(), "cue_mode".into(), "episodes".into(), "config".into()],
                    limits: Some(CollarLimits::V0),
                    hw_rev: Some("N1.0".into()),
                    server_kid: Some(kid_of(&named("server"))),
                    owner_kid: Some(kid_of(&named("owner"))),
                    versions: vec![(MessageType::Boundary, 57), (MessageType::Config, 4), (MessageType::Membership, 2)],
                    herd: Some(HERD.to_vec()),
                    short: Some(12),
                    boot: Some("mcuboot 2.1.0".into()),
                    security_counter: Some(2),
                    image_keys: vec![kid_of(&named("release"))],
                    ..Default::default()
                }),
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
                4332,
                t,
                RecordBody::Slots(vec![
                    SlotEntry { version: 57, status: AckStatus::Applied, effective_at: None },
                    SlotEntry { version: 58, status: AckStatus::Received, effective_at: Some(T0 + 3600) },
                ]),
            ),
        ),
        (
            "anchor",
            rec(
                4333,
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
            rec(4334, t, RecordBody::Firmware(FirmwareStatus { state: FirmwareState::Staged, version: "2.1.0".into(), digest: Some([0x5a; 8]), code: None })),
        ),
        (
            "firmware_deferred",
            rec(
                4335,
                t,
                RecordBody::Firmware(FirmwareStatus { state: FirmwareState::Staged, version: "2.1.0".into(), digest: None, code: Some(Reject::NotNow) }),
            ),
        ),
    ]
}

fn record_cases() -> Vec<RecordCase> {
    let mut out: Vec<RecordCase> = records()
        .into_iter()
        .map(|(name, r)| RecordCase { name: name.into(), hex: hex(&r.encode()), json: r.to_json(), expect: "ok".into(), same_bytes: true })
        .collect();
    let raw = |name: &str, v: Value| {
        let bytes = cbor::encode(&v);
        let r = Record::from_value(&v);
        let same_bytes = r.as_ref().is_ok_and(|r| r.encode() == bytes);
        RecordCase { name: name.into(), hex: hex(&bytes), json: r.as_ref().map_or(Json::Null, Record::to_json), expect: outcome(r.map(|_| ())), same_bytes }
    };
    let ints = |v: &[i64]| Value::Array(v.iter().map(|x| Value::int(*x)).collect());
    out.push(raw(
        "unknown_record_type_is_kept",
        Value::Array(vec![Value::uint(42u8), Value::uint(4400u16), Value::uint(T0), Value::text("from a newer collar")]),
    ));
    out.push(raw("unknown_cue_kind_is_kept", ints(&[3, 4401, T0 as i64, 9, 2, -150, 0, 57])));
    out.push(raw("unknown_outcome_is_kept", ints(&[4, 4402, T0 as i64, 12, 57, 0, 3, 2, 40, 7])));
    out.push(raw("extra_fields_are_ignored", ints(&[5, 4403, T0 as i64, 1, 57, 2, 0, 0, 99])));
    out.push(raw("status_too_short", ints(&[1, 4404, T0 as i64, -936_200_123])));
    out.push(raw("seq_not_a_number", Value::Array(vec![Value::uint(1u8), Value::text("x"), Value::uint(T0)])));
    // Non-deterministic bytes are refused before any record is read.
    let mut long = cbor::encode(&ints(&[6, 1, T0 as i64]));
    long.splice(2..3, [0x18, 0x01]);
    out.push(RecordCase {
        name: "not_deterministic".into(),
        hex: hex(&long),
        json: Json::Null,
        expect: outcome(cbor::decode(&long).map(|_| ()).map_err(|_| Reject::BadCbor)),
        same_bytes: false,
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
        "state": b.state, "next": b.next,
    })
}

fn link_file() -> String {
    let dev = named("device");
    let server = named("server");
    let shared = x25519(&dev, &server.verifying_key());
    let fkey = frame_key(&shared, &device_id(), &kid_of(&server));
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
    let uplinks = vec![
        up("status_only", 77, &Link::default(), &status_only),
        up("three_records_and_a_link_section", 78, &link, &several),
        up("standby_anchor_states_itself", 79, &standby, &[pick("anchor")]),
    ];

    let beacon = Beacon {
        net: NET,
        anchor: 1,
        minute: (T0 / 60) as u32,
        bver: Some(57),
        pending: vec![12],
        msgs: vec![Announce { msg: 0x1a2b_3c4d, count: 2, dst: BROADCAST, message_type: 1 }],
        heard: Beacon::heard_from([0, 3, 11]),
        state: Some([76, 3, 1]),
        next: Some(2),
    };
    let beacons = vec![json!({"name": "beacon", "json": beacon_json(&beacon), "hex": hex(&beacon.encode())}), {
        let quiet = Beacon { net: NET, anchor: 1, minute: (T0 / 60) as u32 + 1, ..Default::default() };
        json!({"name": "quiet_minute", "json": beacon_json(&quiet), "hex": hex(&quiet.encode())})
    }];

    // A herd boundary in fragments of 64 bytes, received out of order with one lost.
    let msg = command(57, Body::Boundary(Boundary::from_lonlat(&rect(0.0, 0.0, 400.0, 120.0), &[rect(150.0, 30.0, 40.0, 40.0)]))).sign(&server);
    let frags = split(&msg, 64, NET, BROADCAST);
    let mut r = Reassembly::new(frags[0].msg, frags[0].count);
    r.add(&frags[2]);
    r.add(&frags[0]);
    let need = r.need(8).unwrap();
    let fragments = vec![json!({
        "name": "boundary_in_64_byte_fragments",
        "message": hex(&msg), "max_data": 64, "msg": frags[0].msg,
        "frames": frags.iter().map(|f| hex(&f.encode())).collect::<Vec<_>>(),
        "received": [2, 0], "need": {"from": need.0, "missing": hex(&need.1)},
    })];

    let p = &US915_500;
    let mut slots = Vec::new();
    for (short, cycle, fast) in
        [(1u16, 5u32, false), (12, 5, false), (300, 5, false), (301, 5, false), (612, 5, false), (12, 5, true), (301, 5, true), (12, 1, false)]
    {
        let s = p.slot(short).unwrap();
        let minutes: Vec<u32> = (0..10).map(|i| (T0 / 60) as u32 + i).collect();
        slots.push(json!({
            "short": short, "cycle_min": cycle, "fast": fast, "k": s.k, "g": s.g, "start_ms": p.slot_start_ms(s.k),
            "minutes": minutes, "sends": minutes.iter().map(|m| p.sends(short, *m, cycle, fast)).collect::<Vec<_>>(),
        }));
    }
    let airtime: Vec<Json> = [(7u8, 125u32, 20usize), (8, 500, 44), (8, 500, 128), (8, 500, 222), (10, 125, 50)]
        .iter()
        .map(|(sf, bw, bytes)| json!({"sf": sf, "bw_khz": bw, "bytes": bytes, "us": op_protocol::v2::tdma::Plan { sf: *sf, bw_khz: *bw, ..*p }.airtime_us(*bytes)}))
        .collect();

    let hello_bytes = hello().sign(&named("device"));
    let hellos = vec![json!({"name": "hello", "hex": hex(&hello_frame(NET, &hello_bytes)), "signed": hex(&hello_bytes)})];

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
        (
            "plan",
            json!({"id": p.id, "slots": p.slots, "slot_ms": p.slot_ms, "uplink_ms": p.uplink_ms, "max_uplink": p.max_uplink, "max_downlink": p.max_downlink}),
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
        ],
    )
}

// ---------------------------------------------------------------- identity

fn identity_file() -> String {
    let me = device_id();
    let ck = claim_key(CLAIM_CODE, &me);
    let farm = named("next_server");
    let codes: Vec<Json> = ["7Q4K-9M2X-H8D3-TV5W", "7q4k 9m2x h8d3 tv5w", "IL0O-0000-0000-0000", "7Q4K9M2XH8D3TV5", "7Q4K9M2XH8D3TV5U"]
        .iter()
        .map(|c| json!({"input": c, "normalized": normalize_claim_code(c)}))
        .collect();
    let card = Card { serial: "OCN-2611-00123".into(), device_key: named("device").verifying_key().to_bytes(), claim_code: CLAIM_CODE.into() };
    let install = |name: &str, s: InstallState| {
        json!({"name": name, "class": s.class.name(), "hw_rev": s.hw_rev, "security_counter": s.security_counter,
               "battery": s.battery, "moving": s.moving, "escaping": s.escaping, "expect": outcome(check_install(&manifest(), &s))})
    };
    let ok = InstallState { class: DeviceClass::Node, hw_rev: "N1.0".into(), security_counter: 2, battery: Some(80), moving: false, escaping: false };
    let installs = vec![
        install("installs", ok.clone()),
        install("other_board", InstallState { hw_rev: "N0.9".into(), ..ok.clone() }),
        install("station_class", InstallState { class: DeviceClass::Station, ..ok.clone() }),
        install("older_than_running", InstallState { security_counter: 3, ..ok.clone() }),
        install("battery_under_the_manifest_minimum", InstallState { battery: Some(49), ..ok.clone() }),
        install("battery_unknown", InstallState { battery: None, ..ok.clone() }),
        install("during_a_move", InstallState { moving: true, ..ok.clone() }),
        install("during_an_escape", InstallState { escaping: true, ..ok }),
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
    ];
    file(&header, &[("claim_codes", codes), ("install", installs)])
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
    ];
    vec![
        ("messages.json", {
            let docs: Vec<Json> = document_cases().iter().map(|d| serde_json::to_value(d).unwrap()).collect();
            let msgs: Vec<Json> = message_cases().iter().map(|m| serde_json::to_value(m).unwrap()).collect();
            file(&header, &[("commands", msgs), ("documents", docs)])
        }),
        ("records.json", file::<RecordCase>(&[], &[("records", record_cases())])),
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
    assert!(cases.len() >= 30);
    for c in &cases {
        let b = unhex(&c.hex).unwrap();
        assert_eq!(outcome(collar_check(&b, &c.state)), c.expect, "{}", c.name);
        let decoded = cose::Sign1::parse(&b).ok().and_then(|s| s.payload_map().ok()).and_then(|m| Command::from_payload(&m).ok());
        assert_eq!(decoded.as_ref().map_or(Json::Null, Command::to_json), c.json, "{}", c.name);
        // Re-encoding a decoded payload gives back the bytes signed (a
        // refused one may carry keys this version ignores).
        if let Some(cmd) = decoded.filter(|_| c.expect == "ok") {
            assert_eq!(cmd.payload(), cose::Sign1::parse(&b).unwrap().payload, "{}", c.name);
        }
    }
    for t in MessageType::ALL.iter().filter(|t| t.is_command()) {
        assert!(cases.iter().any(|c| c.json["type"] == t.name()), "no case for {t}");
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
        let got = cbor::decode(&b).map_err(|_| Reject::BadCbor).and_then(|v| Record::from_value(&v));
        assert_eq!(outcome(got.as_ref().map(|_| ()).map_err(|e| *e)), c.expect, "{}", c.name);
        if let Ok(r) = got {
            assert_eq!(r.to_json(), c.json, "{}", c.name);
            assert_eq!(hex(&r.encode()) == c.hex, c.same_bytes, "{} re-encodes byte for byte", c.name);
        }
    }
    for t in op_protocol::v2::registry::RecordType::ALL {
        assert!(cases.iter().any(|c| c.json["type"] == t.name()), "no case for {t}");
    }
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
        let up = Uplink::parse(&bytes(&u["hex"])).unwrap();
        assert_eq!((up.net, up.src, u64::from(up.fcnt)), (NET, 12, u["fcnt"].as_u64().unwrap()));
        assert_eq!(link_json(&up.link), u["link"]);
        let recs: Vec<Json> = up.open(&key, &dev_id).unwrap().into_iter().map(|r| r.unwrap().to_json()).collect();
        assert_eq!(Json::Array(recs), u["records"], "{}", u["name"]);
    }
    for b in f["beacons"].as_array().unwrap() {
        assert_eq!(beacon_json(&Beacon::decode(&bytes(&b["hex"])).unwrap()), b["json"]);
    }
    for g in f["fragments"].as_array().unwrap() {
        let frames: Vec<FragmentFrame> = g["frames"].as_array().unwrap().iter().map(|h| FragmentFrame::decode(&bytes(h)).unwrap()).collect();
        let mut r = Reassembly::new(frames[0].msg, frames[0].count);
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
        assert_eq!((u64::from(slot.k), u64::from(slot.g)), (s["k"].as_u64().unwrap(), s["g"].as_u64().unwrap()));
        assert_eq!(u64::from(p.slot_start_ms(slot.k)), s["start_ms"].as_u64().unwrap());
        for (m, want) in s["minutes"].as_array().unwrap().iter().zip(s["sends"].as_array().unwrap()) {
            assert_eq!(
                p.sends(short, m.as_u64().unwrap() as u32, s["cycle_min"].as_u64().unwrap() as u32, s["fast"].as_bool().unwrap()),
                want.as_bool().unwrap()
            );
        }
    }
    for a in f["airtime"].as_array().unwrap() {
        let plan = op_protocol::v2::tdma::Plan { sf: a["sf"].as_u64().unwrap() as u8, bw_khz: a["bw_khz"].as_u64().unwrap() as u32, ..*p };
        assert_eq!(plan.airtime_us(a["bytes"].as_u64().unwrap() as usize), a["us"].as_u64().unwrap());
    }
    for h in f["hellos"].as_array().unwrap() {
        let frame = bytes(&h["hex"]);
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
        let s = InstallState {
            class: DeviceClass::from_name(c["class"].as_str().unwrap()).unwrap(),
            hw_rev: c["hw_rev"].as_str().unwrap().into(),
            security_counter: c["security_counter"].as_u64().unwrap() as u32,
            battery: c["battery"].as_u64().map(|b| b as u8),
            moving: c["moving"].as_bool().unwrap(),
            escaping: c["escaping"].as_bool().unwrap(),
        };
        assert_eq!(outcome(check_install(&m, &s)), c["expect"], "{}", c["name"]);
    }
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

/// The records a typical node sends in a day, sealed in frames: the numbers
/// behind the data budget in `opencollar/protocol/v2/README.md`.
#[test]
fn data_budget() {
    let recs = records();
    let pick = |n: &str| recs.iter().find(|(name, _)| *name == n).unwrap().1.clone();
    let key = [1u8; 16];
    let frame = |rs: &[Record]| seal_uplink(NET, 12, 1, &Link::default(), rs, &key, &device_id()).len();
    let status = frame(&[pick("status")]);
    // Ten fixes a minute apart, a typical block that fits one frame.
    let fixes: Vec<Fix> =
        (0..10).map(|i| Fix { t: T0 + 60 * i, lon: -936_200_000 + (i as i32 * 37) % 900, lat: 420_300_000 - (i as i32 * 53) % 700, acc_dm: 18 }).collect();
    let block = frame(&[Record { seq: 70_000, t: T0, body: RecordBody::Fixes(fixes) }]);
    let health = frame(&[pick("health")]);
    let device = frame(&[pick("device")]);
    assert!(status <= 48, "status frame {status} B");
    assert!(block <= US915_500.max_uplink, "fix block frame {block} B");
    let per_day = |n: usize, b: usize| n * b;
    // Status every 5 min, health every 6 h, device info daily.
    let node = per_day(288, status) + per_day(4, health) + device;
    let node_tracked = node + per_day(144, block);
    eprintln!("status {status} B, fix block {block} B, health {health} B, device {device} B; node {node} B/day, with track every 10 min {node_tracked} B/day");
    assert!(node < 20_000 && node_tracked < 60_000);
    let _ = record::encode_records(&[]);
}
