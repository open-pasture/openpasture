//! Protocol v1 test vectors (field-ready contract §3.10), shared with the
//! firmware: EF copies `tests/vectors/*.json` byte-identical into
//! `opencollar/firmware/tests/host/vectors/`.
//!
//! Every expected result below is written by hand (codes, validity, acks),
//! except geofence margins, which come from a plain reference computation
//! and are spot-checked against analytic values. The `vectors_are_current`
//! test rebuilds the files and fails if they differ from what is committed;
//! `OP_WRITE_VECTORS=1 cargo test -p op-protocol --test vectors` rewrites
//! them. The other tests read the committed files and check this
//! implementation against every case.
//!
//! Signing key: Ed25519 seed bytes `00 01 02 … 1f`. See `vectors/README.md`
//! for the file formats.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use op_geo::projection::{Projection, round7};
use op_geo::{Geofence, GeofenceConfig, LonLat, Polygon};
use op_protocol::*;
use serde::{Deserialize, Serialize};

const T0: &str = "2026-09-27T12:00:00Z";
/// Ames, Iowa: the contract's live-check farm.
const AMES: LonLat = [-93.62, 42.03];

fn seed() -> [u8; 32] {
    core::array::from_fn(|i| i as u8)
}

fn key() -> SigningKey {
    SigningKey::from_bytes(&seed())
}

fn seed_hex() -> String {
    seed().iter().map(|b| format!("{b:02x}")).collect()
}

fn t(s: &str) -> DateTime<Utc> {
    wire_time::parse(s).unwrap()
}

fn t0() -> DateTime<Utc> {
    t(T0)
}

fn dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/vectors")
}

/// A file: header fields pretty, one compact case per line.
fn file<C: Serialize>(header: &[(&str, String)], cases: &[C]) -> String {
    let mut out = String::from("{\n");
    for (k, v) in header {
        out.push_str(&format!("  \"{k}\": {v},\n"));
    }
    out.push_str("  \"cases\": [\n");
    let lines: Vec<String> = cases.iter().map(|c| format!("    {}", serde_json::to_string(c).unwrap())).collect();
    out.push_str(&lines.join(",\n"));
    out.push_str("\n  ]\n}\n");
    out
}

fn json<T: Serialize>(v: &T) -> String {
    serde_json::to_string(v).unwrap()
}

// ---------------------------------------------------------------- commands

#[derive(Debug, Serialize, Deserialize)]
struct WireCase {
    name: String,
    wire: String,
    valid: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    /// For valid cases: the canonical bytes the signature covers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    canonical: Option<String>,
}

#[derive(Debug, Deserialize)]
struct WireFile {
    public_key: String,
    #[serde(default)]
    collar_id: Option<String>,
    #[serde(default)]
    held_version: Option<u32>,
    cases: Vec<WireCase>,
}

/// Sign an object given as ordered `(key, raw JSON value)` pairs, the way the
/// server does (canonical: sorted keys, values as written), and write it in
/// that key order with `sig` appended.
fn signed_text(pairs: &[(&str, String)], with: &SigningKey) -> String {
    let mut sorted: Vec<&(&str, String)> = pairs.iter().collect();
    sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
    let canonical = format!("{{{}}}", sorted.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect::<Vec<_>>().join(","));
    let sig = sign_bytes(canonical.as_bytes(), with);
    format!("{{{},\"sig\":\"{sig}\"}}", pairs.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect::<Vec<_>>().join(","))
}

fn sign_bytes(bytes: &[u8], with: &SigningKey) -> String {
    use base64::Engine;
    use ed25519_dalek::Signer;
    base64::engine::general_purpose::STANDARD.encode(with.sign(bytes).to_bytes())
}

/// The command's fields as ordered pairs (no `sig`), values as serde writes them.
fn pairs_of(cmd: &BoundaryCommand) -> Vec<(&'static str, String)> {
    let mut out = vec![("command_id", json(&cmd.command_id))];
    if let Some(h) = &cmd.herd_id {
        out.push(("herd_id", json(h)));
    }
    if let Some(c) = &cmd.collar_id {
        out.push(("collar_id", json(c)));
    }
    out.push(("version", json(&cmd.version)));
    if let Some(e) = &cmd.effective_at {
        out.push(("effective_at", json(&wire_time::format(e))));
    }
    out.push(("boundary", json(&cmd.boundary)));
    if !cmd.holes.is_empty() {
        out.push(("holes", json(&cmd.holes)));
    }
    if cmd.cue_mode != CueMode::Audio {
        out.push(("cue_mode", json(&cmd.cue_mode)));
    }
    if let Some(w) = cmd.warn_m {
        out.push(("warn_m", json(&w)));
    }
    if let Some(h) = cmd.hysteresis_m {
        out.push(("hysteresis_m", json(&h)));
    }
    out
}

fn offset(e: f64, n: f64) -> LonLat {
    let p = Projection::new(AMES).offset(e, n);
    [round7(p[0]), round7(p[1])]
}

fn rect_m(x: f64, y: f64, w: f64, h: f64) -> Vec<LonLat> {
    vec![offset(x, y), offset(x + w, y), offset(x + w, y + h), offset(x, y + h)]
}

/// The golden V0 command (same as the unit test's, from the pre-v1 code).
fn v0_command() -> BoundaryCommand {
    let poly = Polygon::from_ring(vec![[-92.41, 38.12], [-92.40, 38.12], [-92.40, 38.13], [-92.41, 38.13]]);
    let mut cmd = BoundaryCommand::from_polygon("bnd_01J8V0GOLDEN", 57, &poly, Some(t(T0))).unwrap();
    cmd.herd_id = Some("herd_01J7GOLDEN".into());
    cmd.warn_m = Some(5.0);
    cmd.hysteresis_m = Some(1.0);
    sign_command(&mut cmd, &key());
    cmd
}

fn holes_command() -> BoundaryCommand {
    let poly = Polygon::from_rings(rect_m(0.0, 0.0, 400.0, 400.0), [rect_m(150.0, 150.0, 40.0, 40.0), rect_m(250.0, 100.0, 30.0, 60.0)]);
    let mut cmd = BoundaryCommand::from_shape("bnd_01J9HOLES", 58, &poly, Some(t("2026-09-27T12:30:00Z"))).unwrap();
    cmd.herd_id = Some("herd_01J7GOLDEN".into());
    cmd.collar_id = Some("col_01J9SIMCOLLAR".into());
    cmd.cue_mode = CueMode::Track;
    cmd.warn_m = Some(5.0);
    cmd.hysteresis_m = Some(1.0);
    sign_command(&mut cmd, &key());
    cmd
}

fn valid(name: &str, wire: String) -> WireCase {
    let canonical = String::from_utf8(canonical_wire(wire.as_bytes()).expect("valid case scans").bytes).unwrap();
    WireCase { name: name.into(), wire, valid: true, code: None, canonical: Some(canonical) }
}

fn invalid(name: &str, wire: String, code: RejectCode) -> WireCase {
    WireCase { name: name.into(), wire, valid: false, code: Some(code.as_str().into()), canonical: None }
}

fn command_cases() -> Vec<WireCase> {
    let k = key();
    let v0 = v0_command();
    let v0_wire = json(&v0);
    let holes = holes_command();
    let holes_wire = json(&holes);
    let hp = pairs_of(&holes);
    let body = |p: &[(&str, String)]| p.iter().map(|(k, v)| format!("\"{k}\":{v}")).collect::<Vec<_>>().join(",");

    let mut bare = BoundaryCommand::from_polygon("bnd_x", 3, &Polygon::from_ring(v0.boundary.clone()), None).unwrap();
    sign_command(&mut bare, &k);
    let mut scoped = v0.clone();
    scoped.collar_id = Some("col_01J9SIMCOLLAR".into());
    sign_command(&mut scoped, &k);
    let mut tiny = BoundaryCommand::from_shape("bnd_exp", 4, &Polygon::from_ring(vec![[1e-7, 0.0], [0.001, 0.0], [0.001, 0.001], [0.0, 0.001]]), None).unwrap();
    tiny.hysteresis_m = Some(0.0);
    sign_command(&mut tiny, &k);
    assert!(json(&tiny).contains("1e-7"));

    let mut shuffled = hp.clone();
    shuffled.reverse();
    let shuffled = {
        let signed = signed_text(&shuffled, &k);
        // Move sig to the front.
        let sig_at = signed.rfind(",\"sig\"").unwrap();
        format!("{{{},{}}}", &signed[sig_at + 1..signed.len() - 1], &signed[1..sig_at])
    };
    let spaced = {
        let mut s = String::from(" {\n\t");
        let signed = signed_text(&hp, &k);
        let v: Vec<(String, String)> = split_pairs(&signed);
        s.push_str(&v.iter().map(|(k, v)| format!("\"{k}\" :  {}", v.replace(',', " , ").replace('[', "[ "))).collect::<Vec<_>>().join(" ,\r\n\t"));
        s.push_str("\n}\n");
        s
    };

    let mut extra = hp.clone();
    extra.push(("note", json(&"signed and ignored")));
    let mut sixteen = hp.clone();
    for (i, name) in ["k1", "k2", "k3", "k4", "k5"].into_iter().enumerate() {
        sixteen.push((name, json(&i)));
    }
    assert_eq!(sixteen.len() + 1, 16);
    let mut seventeen = sixteen.clone();
    seventeen.push(("k6", json(&6)));

    let other = SigningKey::from_bytes(&core::array::from_fn(|i| 0x20 + i as u8));
    let without = |name: &str| {
        let v: Vec<(String, String)> = split_pairs(&holes_wire);
        format!("{{{}}}", v.iter().filter(|(k, _)| k != name).map(|(k, v)| format!("\"{k}\":{v}")).collect::<Vec<_>>().join(","))
    };
    let nested = {
        let mut p = hp.clone();
        p.push(("x", "{\"a\":1}".into()));
        signed_text(&p, &k)
    };
    let nested_in_array = {
        let mut p = hp.clone();
        p.push(("x", "[{\"a\":1}]".into()));
        signed_text(&p, &k)
    };
    let deep = {
        let mut p = hp.clone();
        p.push(("x", "[[[[[1]]]]]".into()));
        signed_text(&p, &k)
    };
    let pad_to = |target: usize| {
        let make = |n: usize| {
            let mut p = hp.clone();
            p.push(("pad", json(&"x".repeat(n))));
            signed_text(&p, &k)
        };
        let n = target - make(0).len();
        let w = make(n);
        assert_eq!(w.len(), target);
        w
    };
    let long_id = {
        let mut p = hp.clone();
        p[0] = ("command_id", json(&"b".repeat(65)));
        signed_text(&p, &k)
    };
    let bad_types = {
        let mut p = hp.clone();
        let at = p.iter().position(|(k, _)| *k == "version").unwrap();
        p[at].1 = json(&"58");
        signed_text(&p, &k)
    };
    let control = {
        let mut p = hp.clone();
        p.push(("note", "\"two\nlines\"".into()));
        signed_text(&p, &k)
    };

    vec![
        valid("v0_compact", v0_wire.clone()),
        valid("v0_pretty", serde_json::to_string_pretty(&v0).unwrap()),
        valid("v0_no_herd_no_margins", json(&bare)),
        valid("holes_collar_id_track", holes_wire.clone()),
        valid("collar_id", json(&scoped)),
        valid("shuffled_keys_sig_first", shuffled),
        valid("whitespace_everywhere", spaced),
        valid("exponent_1e-7", json(&tiny)),
        valid("unknown_key_signed", signed_text(&extra, &k)),
        valid("sixteen_keys", signed_text(&sixteen, &k)),
        valid("exactly_12288_bytes", pad_to(MAX_COMMAND_BYTES)),
        invalid("seventeen_keys", signed_text(&seventeen, &k), RejectCode::BadJson),
        invalid("oversize_12289_bytes", pad_to(MAX_COMMAND_BYTES + 1), RejectCode::TooLarge),
        invalid("tampered_version", holes_wire.replacen("\"version\":58", "\"version\":59", 1), RejectCode::BadSig),
        invalid("tampered_coordinate", holes_wire.replacen("-93.62", "-93.63", 1), RejectCode::BadSig),
        invalid("tampered_cue_mode", holes_wire.replacen("\"track\"", "\"audio\"", 1), RejectCode::BadSig),
        invalid("stripped_holes", without("holes"), RejectCode::BadSig),
        invalid("stripped_herd_id", without("herd_id"), RejectCode::BadSig),
        invalid("stripped_collar_id", without("collar_id"), RejectCode::BadSig),
        invalid("missing_sig", without("sig"), RejectCode::BadSig),
        invalid("sig_not_base64", format!("{{{},\"sig\":\"not base64!\"}}", body(&hp)), RejectCode::BadSig),
        invalid(
            "sig_63_bytes",
            format!("{{{},\"sig\":\"{}\"}}", body(&hp), {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.encode([7u8; 63])
            }),
            RejectCode::BadSig,
        ),
        invalid("sig_not_a_string", format!("{{{},\"sig\":12}}", body(&hp)), RejectCode::BadSig),
        invalid("signed_by_another_key", signed_text(&hp, &other), RejectCode::BadSig),
        invalid("duplicate_key_same_value", holes_wire.replacen("\"version\":58", "\"version\":58,\"version\":58", 1), RejectCode::BadJson),
        invalid("duplicate_key_first_differs", holes_wire.replacen('{', "{\"version\":99,", 1), RejectCode::BadJson),
        invalid("nested_object", nested, RejectCode::BadJson),
        invalid("object_in_array", nested_in_array, RejectCode::BadJson),
        invalid("arrays_too_deep", deep, RejectCode::BadJson),
        invalid("escaped_key", holes_wire.replacen("\"version\"", "\"\\u0076ersion\"", 1), RejectCode::BadJson),
        invalid("raw_control_character", control, RejectCode::BadJson),
        invalid("not_an_object", "[1,2]".into(), RejectCode::BadJson),
        invalid("trailing_text", format!("{holes_wire} x"), RejectCode::BadJson),
        invalid("truncated", holes_wire[..holes_wire.len() / 2].to_string(), RejectCode::BadJson),
        invalid("version_as_string", bad_types, RejectCode::BadJson),
        invalid("command_id_65_bytes", long_id, RejectCode::BadJson),
    ]
}

/// Top-level `(key, value)` pairs of a flat object as written, by the crate's own scanner rules.
fn split_pairs(text: &str) -> Vec<(String, String)> {
    let v: serde_json::Value = serde_json::from_str(text).unwrap();
    let mut out = Vec::new();
    // Recover the written order from the text itself.
    let obj = v.as_object().unwrap();
    let mut keys: Vec<(&String, usize)> = obj.keys().map(|k| (k, text.find(&format!("\"{k}\":")).unwrap())).collect();
    keys.sort_by_key(|x| x.1);
    for (k, _) in keys {
        out.push((k.clone(), serde_json::to_string(&obj[k]).unwrap()));
    }
    out
}

// ---------------------------------------------------------------- config

const SIM_COLLAR: &str = "col_01J9SIMCOLLAR";
const HELD_CONFIG: u32 = 2;

fn config_pairs(version: u32) -> Vec<(&'static str, String)> {
    vec![
        ("command_id", json(&format!("cfg_01J9V{version}"))),
        ("collar_id", json(&SIM_COLLAR)),
        ("version", json(&version)),
        ("herd_id", json(&"herd_01J7GOLDEN")),
        ("endpoint", json(&"https://farm.example.com/collar/v1")),
        ("report_s", json(&60)),
        ("poll_s", json(&60)),
        ("fast_report_s", json(&10)),
        ("fast_poll_s", json(&10)),
        ("fast_until", json(&"2026-09-27T13:10:00Z")),
    ]
}

fn with(mut p: Vec<(&'static str, String)>, key: &'static str, value: Option<String>) -> Vec<(&'static str, String)> {
    match (p.iter().position(|(k, _)| *k == key), value) {
        (Some(i), Some(v)) => p[i].1 = v,
        (Some(i), None) => {
            p.remove(i);
        }
        (None, Some(v)) => p.push((key, v)),
        (None, None) => {}
    }
    p
}

fn config_cases() -> Vec<WireCase> {
    let k = key();
    let full = config_pairs(3);
    let s = |p: &[(&str, String)]| signed_text(p, &k);
    let minimal: Vec<(&str, String)> =
        full.iter().filter(|(k, _)| matches!(*k, "command_id" | "collar_id" | "version" | "report_s" | "poll_s")).cloned().collect();
    let pretty = {
        let wire = s(&full);
        wire.replace(",\"", ",\n  \"").replacen('{', "{\n  ", 1).replace("\"}", "\"\n}")
    };
    let full_wire = s(&full);
    vec![
        valid("valid_full", full_wire.clone()),
        valid("valid_minimal", s(&minimal)),
        valid("valid_pretty", pretty),
        valid("valid_intervals_10_and_3600", s(&with(with(full.clone(), "report_s", Some(json(&10))), "poll_s", Some(json(&3600))))),
        invalid("missing_collar_id", s(&with(full.clone(), "collar_id", None)), RejectCode::BadJson),
        invalid("wrong_collar", s(&with(full.clone(), "collar_id", Some(json(&"col_01J9OTHER")))), RejectCode::WrongCollar),
        invalid("stale_equal", s(&config_pairs(HELD_CONFIG)), RejectCode::Stale),
        invalid("stale_lower", s(&config_pairs(1)), RejectCode::Stale),
        invalid("report_s_9", s(&with(full.clone(), "report_s", Some(json(&9)))), RejectCode::BadConfig),
        invalid("poll_s_3601", s(&with(full.clone(), "poll_s", Some(json(&3601)))), RejectCode::BadConfig),
        invalid("fast_poll_s_5", s(&with(full.clone(), "fast_poll_s", Some(json(&5)))), RejectCode::BadConfig),
        invalid("fast_until_without_fast_poll_s", s(&with(full.clone(), "fast_poll_s", None)), RejectCode::BadConfig),
        invalid("http_endpoint", s(&with(full.clone(), "endpoint", Some(json(&"http://farm.example.com/collar/v1")))), RejectCode::BadConfig),
        invalid("herd_id_65_bytes", s(&with(full.clone(), "herd_id", Some(json(&"h".repeat(65))))), RejectCode::BadJson),
        invalid("duplicate_keys", full_wire.replacen("\"version\":3", "\"version\":3,\"version\":3", 1), RejectCode::BadJson),
        invalid("nested_object", s(&with(full.clone(), "x", Some("{\"a\":1}".into()))), RejectCode::BadJson),
        invalid("tampered_herd_id", full_wire.replacen("herd_01J7GOLDEN", "herd_01J7OTHER", 1), RejectCode::BadSig),
    ]
}

// ---------------------------------------------------------------- shapes

#[derive(Debug, Serialize, Deserialize)]
struct ShapeCase {
    name: String,
    boundary: Vec<LonLat>,
    #[serde(default)]
    holes: Vec<Vec<LonLat>>,
    warn_m: f64,
    hysteresis_m: f64,
    limits: CollarLimits,
    ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<String>,
}

fn circle_m(cx: f64, cy: f64, r: f64, n: usize) -> Vec<LonLat> {
    (0..n)
        .map(|i| {
            let a = std::f64::consts::TAU * i as f64 / n as f64;
            offset(cx + r * a.cos(), cy + r * a.sin())
        })
        .collect()
}

/// A 400 m square field whose first vertex is AMES, so the collar's
/// projection matches these metre offsets.
fn field() -> Vec<LonLat> {
    rect_m(0.0, 0.0, 400.0, 400.0)
}

/// `n` holes of `k` vertices on a grid inside `field()`.
fn grid_holes(n: usize, k: usize) -> Vec<Vec<LonLat>> {
    (0..n).map(|i| circle_m(60.0 + 70.0 * (i % 5) as f64, 60.0 + 70.0 * (i / 5) as f64, 12.0, k)).collect()
}

fn shape_cases() -> Vec<ShapeCase> {
    let v0 = CollarLimits::V0;
    let legacy = CollarLimits::LEGACY;
    let case = |name: &str, boundary: Vec<LonLat>, holes: Vec<Vec<LonLat>>, warn_m: f64, limits: CollarLimits, code: Option<RejectCode>| ShapeCase {
        name: name.into(),
        boundary,
        holes,
        warn_m,
        hysteresis_m: 1.0,
        limits,
        ok: code.is_none(),
        code: code.map(|c| c.as_str().into()),
    };
    let rev = |mut r: Vec<LonLat>| {
        r.reverse();
        r
    };
    let hole = rect_m(180.0, 180.0, 40.0, 40.0);
    let big_circle = |n: usize| circle_m(200.0, 200.0, 190.0, n);
    let total_385 = ring_holes(&[32, 32, 32, 32, 32, 32, 32, 30, 3]);
    use RejectCode::*;
    let mut out = vec![
        case("ok_square_ccw", field(), vec![], 5.0, v0, None),
        case("ok_square_cw", rev(field()), vec![], 5.0, v0, None),
        case("ok_hole_ccw_in_ccw", field(), vec![hole.clone()], 5.0, v0, None),
        case("ok_hole_cw_in_cw", rev(field()), vec![rev(hole.clone())], 5.0, v0, None),
        case("ok_closed_rings", [field(), vec![field()[0]]].concat(), vec![[hole.clone(), vec![hole[0]]].concat()], 5.0, v0, None),
        case("bad_margins_negative_warn", field(), vec![], -0.5, v0, Some(BadMargins)),
        case("ok_margins_1000", field(), vec![], 1000.0, v0, None),
        case("bad_margins_1000_1", field(), vec![], 1000.1, v0, Some(BadMargins)),
        case("too_many_holes_legacy", field(), vec![hole.clone()], 5.0, legacy, Some(TooManyHoles)),
        case("ok_16_holes", field(), grid_holes(16, 4), 5.0, v0, None),
        case("too_many_holes_17", field(), grid_holes(17, 4), 5.0, v0, Some(TooManyHoles)),
        case("out_of_range_lon", vec![[180.0000001, 0.0], [179.9, 0.0], [179.9, 0.1]], vec![], 5.0, v0, Some(OutOfRange)),
        case("out_of_range_lat", vec![[0.0, -90.0000001], [0.1, -89.9], [0.2, -89.9]], vec![], 5.0, v0, Some(OutOfRange)),
        case("ok_on_the_antimeridian", vec![[179.999, 0.0], [180.0, 0.0], [180.0, 0.001]], vec![], 5.0, v0, None),
        case("too_few_vertices_2", field()[..2].to_vec(), vec![], 5.0, v0, Some(TooFewVertices)),
        case("too_few_vertices_after_duplicates", vec![field()[0], field()[1], field()[1], field()[0]], vec![], 5.0, v0, Some(TooFewVertices)),
        case("too_few_vertices_hole", field(), vec![hole[..2].to_vec()], 5.0, v0, Some(TooFewVertices)),
        case("ok_outer_128", big_circle(128), vec![], 5.0, v0, None),
        case("too_many_vertices_outer_129", big_circle(129), vec![], 5.0, v0, Some(TooManyVertices)),
        case("ok_legacy_64", big_circle(64), vec![], 5.0, legacy, None),
        case("too_many_vertices_legacy_65", big_circle(65), vec![], 5.0, legacy, Some(TooManyVertices)),
        case("ok_hole_32", field(), vec![circle_m(200.0, 200.0, 40.0, 32)], 5.0, v0, None),
        case("too_many_vertices_hole_33", field(), vec![circle_m(200.0, 200.0, 40.0, 33)], 5.0, v0, Some(TooManyVertices)),
        case("ok_total_384", big_circle(128), ring_holes(&[32; 8]), 5.0, v0, None),
        case("too_many_vertices_total_385", big_circle(128), total_385, 5.0, v0, Some(TooManyVertices)),
        case(
            "self_intersecting_bowtie",
            vec![offset(0.0, 0.0), offset(100.0, 100.0), offset(100.0, 0.0), offset(0.0, 100.0)],
            vec![],
            5.0,
            v0,
            Some(SelfIntersecting),
        ),
        case(
            "self_intersecting_touches_itself",
            vec![
                offset(0.0, 0.0),
                offset(50.0, 0.0),
                offset(50.0, 50.0),
                offset(100.0, 50.0),
                offset(100.0, 100.0),
                offset(50.0, 100.0),
                offset(50.0, 50.0),
                offset(0.0, 50.0),
            ],
            vec![],
            5.0,
            v0,
            Some(SelfIntersecting),
        ),
        case(
            "self_intersecting_folds_back",
            vec![offset(0.0, 0.0), offset(100.0, 0.0), offset(100.0, 100.0), offset(100.0, 50.0), offset(0.0, 100.0)],
            vec![],
            5.0,
            v0,
            Some(SelfIntersecting),
        ),
        case(
            "self_intersecting_hole",
            field(),
            vec![vec![offset(150.0, 150.0), offset(250.0, 250.0), offset(250.0, 150.0), offset(150.0, 250.0)]],
            5.0,
            v0,
            Some(SelfIntersecting),
        ),
        case("rings_cross_hole_over_edge", field(), vec![rect_m(380.0, 180.0, 40.0, 40.0)], 5.0, v0, Some(RingsCross)),
        case("rings_cross_hole_touches_edge", field(), vec![vec![offset(400.0, 200.0), offset(350.0, 180.0), offset(350.0, 220.0)]], 5.0, v0, Some(RingsCross)),
        case("rings_cross_two_holes", field(), vec![rect_m(100.0, 100.0, 60.0, 60.0), rect_m(130.0, 130.0, 60.0, 60.0)], 5.0, v0, Some(RingsCross)),
        case("hole_outside", field(), vec![rect_m(500.0, 180.0, 40.0, 40.0)], 5.0, v0, Some(HoleOutside)),
        case("holes_overlap_nested", field(), vec![rect_m(100.0, 100.0, 200.0, 200.0), rect_m(180.0, 180.0, 40.0, 40.0)], 5.0, v0, Some(HolesOverlap)),
        case("zero_area_0_98_m2", vec![offset(0.0, 0.0), offset(1.4, 0.0), offset(0.0, 1.4)], vec![], 5.0, v0, Some(ZeroArea)),
        case("ok_area_1_28_m2", vec![offset(0.0, 0.0), offset(1.6, 0.0), offset(0.0, 1.6)], vec![], 5.0, v0, None),
        case("hole_too_small_99_m2", field(), vec![rect_m(180.0, 180.0, 9.9, 10.0)], 5.0, v0, Some(HoleTooSmall)),
        case("ok_hole_101_m2", field(), vec![rect_m(180.0, 180.0, 10.1, 10.0)], 5.0, v0, None),
        case("ok_gap_to_edge_12_1", field(), vec![rect_m(12.1, 180.0, 20.0, 20.0)], 5.0, v0, None),
        case("hole_too_close_to_edge_11_9", field(), vec![rect_m(11.9, 180.0, 20.0, 20.0)], 5.0, v0, Some(HoleTooClose)),
        case("ok_gap_between_holes_12_1", field(), vec![rect_m(100.0, 180.0, 20.0, 20.0), rect_m(132.1, 180.0, 20.0, 20.0)], 5.0, v0, None),
        case(
            "hole_too_close_between_holes_11_9",
            field(),
            vec![rect_m(100.0, 180.0, 20.0, 20.0), rect_m(131.9, 180.0, 20.0, 20.0)],
            5.0,
            v0,
            Some(HoleTooClose),
        ),
        case("ok_gap_22_1_at_warn_10", field(), vec![rect_m(100.0, 180.0, 20.0, 20.0), rect_m(142.1, 180.0, 20.0, 20.0)], 10.0, v0, None),
        case("hole_too_close_21_9_at_warn_10", field(), vec![rect_m(100.0, 180.0, 20.0, 20.0), rect_m(141.9, 180.0, 20.0, 20.0)], 10.0, v0, Some(HoleTooClose)),
    ];
    // Southern and western hemispheres, and a margin rule that masks later rules.
    let south: Vec<LonLat> = vec![[172.6, -43.5], [172.605, -43.5], [172.605, -43.4964], [172.6, -43.4964]];
    out.push(case("ok_southern_hemisphere", south.clone(), vec![], 5.0, v0, None));
    out.push(case("bad_margins_before_everything", vec![[200.0, 0.0]], vec![], -1.0, v0, Some(BadMargins)));
    out
}

/// Holes of 12 m radius with these vertex counts, evenly on a 110 m circle
/// about the middle of `field()` (inside `big_circle`).
fn ring_holes(counts: &[usize]) -> Vec<Vec<LonLat>> {
    let n = counts.len() as f64;
    counts
        .iter()
        .enumerate()
        .map(|(i, k)| {
            let a = std::f64::consts::TAU * i as f64 / n;
            circle_m(200.0 + 110.0 * a.cos(), 200.0 + 110.0 * a.sin(), 12.0, *k)
        })
        .collect()
}

// ---------------------------------------------------------------- geofence

#[derive(Debug, Serialize, Deserialize)]
struct GeoCase {
    name: String,
    boundary: Vec<LonLat>,
    #[serde(default)]
    holes: Vec<Vec<LonLat>>,
    point: LonLat,
    margin_m: f64,
    inside: bool,
    nearest_ring: usize,
}

/// Reference margin: every edge of every ring, no shortcuts, the edge taken
/// from vertex i-1 to i as the firmware loops.
fn reference(boundary: &[LonLat], holes: &[Vec<LonLat>], point: LonLat) -> (f64, bool, usize) {
    let proj = Projection::new(boundary[0]);
    let q = proj.forward(point);
    let rings: Vec<Vec<[f64; 2]>> = std::iter::once(boundary).chain(holes.iter().map(Vec::as_slice)).map(|r| proj.forward_ring(r)).collect();
    let mut inside = op_geo::point_in_ring(q, &rings[0]);
    let mut best = (f64::INFINITY, 0);
    for (k, r) in rings.iter().enumerate() {
        if k > 0 && op_geo::point_in_ring(q, r) {
            inside = false;
        }
        let n = r.len();
        let d = (0..n).map(|i| op_geo::ring::distance_to_segment(q, r[i], r[(i + n - 1) % n])).fold(f64::INFINITY, f64::min);
        if d < best.0 {
            best = (d, k);
        }
    }
    (if inside { best.0 } else { -best.0 }, inside, best.1)
}

fn geofence_cases() -> Vec<GeoCase> {
    let square = rect_m(0.0, 0.0, 100.0, 100.0);
    let middle = rect_m(40.0, 40.0, 20.0, 20.0);
    let l_hole = vec![offset(40.0, 40.0), offset(80.0, 40.0), offset(80.0, 60.0), offset(60.0, 60.0), offset(60.0, 80.0), offset(40.0, 80.0)];
    let west = rect_m(10.0, 40.0, 20.0, 20.0);
    let east = rect_m(70.0, 40.0, 20.0, 20.0);
    let concave_outer = vec![offset(0.0, 0.0), offset(100.0, 0.0), offset(100.0, 50.0), offset(50.0, 50.0), offset(50.0, 100.0), offset(0.0, 100.0)];
    let mut out = Vec::new();
    // (name, boundary, holes, point in metres, analytic margin)
    let specs: Vec<(&str, Vec<LonLat>, Vec<Vec<LonLat>>, [f64; 2], f64)> = vec![
        ("no_holes_centre", square.clone(), vec![], [50.0, 50.0], 50.0),
        ("no_holes_near_edge", square.clone(), vec![], [50.0, 3.0], 3.0),
        ("no_holes_outside", square.clone(), vec![], [50.0, -10.0], -10.0),
        ("no_holes_off_corner", square.clone(), vec![], [110.0, 110.0], -(200.0f64).sqrt()),
        ("concave_outer_notch", concave_outer.clone(), vec![], [75.0, 75.0], -25.0),
        ("concave_outer_inside", concave_outer, vec![], [25.0, 75.0], 25.0),
        ("in_the_hole", square.clone(), vec![middle.clone()], [50.0, 50.0], -10.0),
        ("between_hole_and_edge_nearer_hole", square.clone(), vec![middle.clone()], [50.0, 30.0], 10.0),
        ("between_hole_and_edge_nearer_edge", square.clone(), vec![middle.clone()], [50.0, 5.0], 5.0),
        ("outside_with_a_hole", square.clone(), vec![middle.clone()], [50.0, -3.0], -3.0),
        ("off_the_hole_corner", square.clone(), vec![middle.clone()], [35.0, 35.0], 50.0f64.sqrt()),
        ("hole_edge_warning_band", square.clone(), vec![middle.clone()], [50.0, 37.0], 3.0),
        ("concave_hole_notch", square.clone(), vec![l_hole.clone()], [70.0, 70.0], 10.0),
        ("concave_hole_inside_l", square.clone(), vec![l_hole], [50.0, 70.0], -10.0),
        ("nearest_second_hole", square.clone(), vec![west.clone(), east.clone()], [66.0, 50.0], 4.0),
        ("nearest_first_hole", square.clone(), vec![west.clone(), east.clone()], [34.0, 50.0], 4.0),
        ("nearest_outer_with_holes", square.clone(), vec![west.clone(), east.clone()], [50.0, 97.0], 3.0),
        ("inside_second_hole", square, vec![west, east], [80.0, 50.0], -10.0),
    ];
    for (name, boundary, holes, at, analytic) in specs {
        let point = offset(at[0], at[1]);
        let (margin_m, inside, nearest_ring) = reference(&boundary, &holes, point);
        assert!((margin_m - analytic).abs() < 0.02, "{name}: reference {margin_m} vs analytic {analytic}");
        out.push(GeoCase { name: name.into(), boundary, holes, point, margin_m: (margin_m * 1e6).round() / 1e6, inside, nearest_ring });
    }
    // Southern hemisphere, east of Greenwich.
    let south: Vec<LonLat> = vec![[172.6, -43.5], [172.605, -43.5], [172.605, -43.4964], [172.6, -43.4964]];
    let south_hole: Vec<LonLat> = vec![[172.602, -43.499], [172.603, -43.499], [172.603, -43.498], [172.602, -43.498]];
    for (name, point) in [("southern_in_hole", [172.6025, -43.4985]), ("southern_inside", [172.6045, -43.4985])] {
        let (margin_m, inside, nearest_ring) = reference(&south, std::slice::from_ref(&south_hole), point);
        out.push(GeoCase {
            name: name.into(),
            boundary: south.clone(),
            holes: vec![south_hole.clone()],
            point,
            margin_m: (margin_m * 1e6).round() / 1e6,
            inside,
            nearest_ring,
        });
    }
    out
}

// ---------------------------------------------------------------- slots

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct SlotStep {
    op: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    version: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    effective_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    herd_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    collar_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    vertices: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    now: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct ExpAck {
    version: u32,
    status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Expect {
    acks: Vec<ExpAck>,
    active: Option<u32>,
    have: u32,
    free: usize,
    free_bytes: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize)]
struct SlotCase {
    name: String,
    limits: CollarLimits,
    herd_id: String,
    collar_id: String,
    steps: Vec<SlotStep>,
    expect: Vec<Expect>,
}

const SLOT_HERD: &str = "herd_01J7SLOTS";
const SLOT_COLLAR: &str = "col_01J9SLOTS";

fn at(s: i64) -> String {
    wire_time::format(&(t0() + Duration::seconds(s)))
}

fn insert(version: u32, effective: Option<i64>, now: Option<i64>) -> SlotStep {
    SlotStep { op: "insert".into(), version: Some(version), effective_at: effective.map(at), now: now.map(at), ..Default::default() }
}

fn tick(now: i64) -> SlotStep {
    SlotStep { op: "tick".into(), now: Some(at(now)), ..Default::default() }
}

fn boot() -> SlotStep {
    SlotStep { op: "boot".into(), ..Default::default() }
}

fn ack(version: u32, status: &str, at_s: Option<i64>) -> ExpAck {
    ExpAck { version, status: status.into(), code: None, at: at_s.map(at) }
}

fn rejected(version: u32, code: RejectCode, at_s: Option<i64>) -> ExpAck {
    ExpAck { version, status: "rejected".into(), code: Some(code.as_str().into()), at: at_s.map(at) }
}

fn state(acks: Vec<ExpAck>, active: Option<u32>, have: u32, free: usize, free_bytes: Option<usize>) -> Expect {
    Expect { acks, active, have, free, free_bytes }
}

/// Bytes of the default 4-vertex boundary.
const R4: usize = 192 + 8 * 4;
/// Bytes of a 384-vertex boundary.
const R384: usize = 192 + 8 * 384;
const V0B: usize = 24_576;

fn slot_cases() -> Vec<SlotCase> {
    let v0 = CollarLimits::V0;
    let legacy = CollarLimits::LEGACY;
    let case = |name: &str, limits: CollarLimits, steps: Vec<(SlotStep, Expect)>| {
        let (steps, expect) = steps.into_iter().unzip();
        SlotCase { name: name.into(), limits, herd_id: SLOT_HERD.into(), collar_id: SLOT_COLLAR.into(), steps, expect }
    };
    let fb = |n4: usize| Some(V0B - n4 * R4);
    let big = |step: SlotStep| SlotStep { vertices: Some(384), ..step };
    let mut out = vec![
        case(
            "immediate_then_staged",
            v0,
            vec![
                (insert(1, None, Some(0)), state(vec![ack(1, "applied", Some(0))], Some(1), 1, 15, fb(1))),
                (insert(2, Some(60), Some(0)), state(vec![ack(2, "received", Some(0))], Some(1), 2, 14, fb(2))),
                (tick(59), state(vec![], Some(1), 2, 14, fb(2))),
                (tick(60), state(vec![ack(2, "applied", Some(60))], Some(2), 2, 15, fb(1))),
            ],
        ),
        case(
            "supersede_without_ack",
            v0,
            vec![
                (insert(1, None, Some(0)), state(vec![ack(1, "applied", Some(0))], Some(1), 1, 15, fb(1))),
                (insert(2, Some(60), Some(0)), state(vec![ack(2, "received", Some(0))], Some(1), 2, 14, fb(2))),
                (insert(3, Some(120), Some(0)), state(vec![ack(3, "received", Some(0))], Some(1), 3, 13, fb(3))),
                (tick(130), state(vec![ack(3, "applied", Some(130))], Some(3), 3, 15, fb(1))),
            ],
        ),
        case(
            "dead_slots_pruned",
            v0,
            vec![
                (insert(1, None, Some(0)), state(vec![ack(1, "applied", Some(0))], Some(1), 1, 15, fb(1))),
                (insert(2, Some(120), Some(0)), state(vec![ack(2, "received", Some(0))], Some(1), 2, 14, fb(2))),
                // 3 activates before 2, so 2 can never be in effect.
                (insert(3, Some(60), Some(0)), state(vec![ack(3, "received", Some(0))], Some(1), 3, 14, fb(2))),
                (tick(130), state(vec![ack(3, "applied", Some(130))], Some(3), 3, 15, fb(1))),
                (insert(4, Some(300), Some(130)), state(vec![ack(4, "received", Some(130))], Some(3), 4, 14, fb(2))),
                // Same activation time: the higher version wins, 4 is dead.
                (insert(5, Some(300), Some(130)), state(vec![ack(5, "received", Some(130))], Some(3), 5, 14, fb(2))),
            ],
        ),
        case(
            "duplicate_reacks_current_status",
            v0,
            vec![
                (insert(1, None, Some(0)), state(vec![ack(1, "applied", Some(0))], Some(1), 1, 15, fb(1))),
                (insert(1, None, Some(10)), state(vec![ack(1, "applied", Some(0))], Some(1), 1, 15, fb(1))),
                (insert(2, Some(60), Some(10)), state(vec![ack(2, "received", Some(10))], Some(1), 2, 14, fb(2))),
                (insert(2, Some(60), Some(20)), state(vec![ack(2, "received", Some(10))], Some(1), 2, 14, fb(2))),
                (tick(60), state(vec![ack(2, "applied", Some(60))], Some(2), 2, 15, fb(1))),
                (insert(2, Some(60), Some(70)), state(vec![ack(2, "applied", Some(60))], Some(2), 2, 15, fb(1))),
            ],
        ),
        case(
            "stale",
            v0,
            vec![
                (insert(3, None, Some(0)), state(vec![ack(3, "applied", Some(0))], Some(3), 3, 15, fb(1))),
                (insert(2, None, Some(0)), state(vec![rejected(2, RejectCode::Stale, Some(0))], Some(3), 3, 15, fb(1))),
                (insert(5, Some(60), Some(0)), state(vec![ack(5, "received", Some(0))], Some(3), 5, 14, fb(2))),
                (insert(4, Some(30), Some(0)), state(vec![rejected(4, RejectCode::Stale, Some(0))], Some(3), 5, 14, fb(2))),
                (insert(3, None, Some(1)), state(vec![ack(3, "applied", Some(0))], Some(3), 5, 14, fb(2))),
            ],
        ),
        case(
            "slots_full_by_count_legacy",
            legacy,
            vec![
                (insert(1, None, Some(0)), state(vec![ack(1, "applied", Some(0))], Some(1), 1, 1, None)),
                (insert(2, Some(60), Some(0)), state(vec![ack(2, "received", Some(0))], Some(1), 2, 0, None)),
                (insert(3, Some(120), Some(0)), state(vec![rejected(3, RejectCode::SlotsFull, Some(0))], Some(1), 2, 0, None)),
                // 4 activates before 2: 2 is dead, so there is room.
                (insert(4, Some(30), Some(0)), state(vec![ack(4, "received", Some(0))], Some(1), 4, 0, None)),
                (insert(5, None, Some(40)), state(vec![ack(5, "applied", Some(40))], Some(5), 5, 1, None)),
            ],
        ),
    ];
    // V0: the 16th slot fits, a 17th doesn't.
    let mut steps = vec![(insert(1, None, Some(0)), state(vec![ack(1, "applied", Some(0))], Some(1), 1, 15, fb(1)))];
    for v in 2..=16u32 {
        let held = v as usize;
        steps.push((insert(v, Some(60 * v as i64), Some(0)), state(vec![ack(v, "received", Some(0))], Some(1), v, 16 - held, fb(held))));
    }
    steps.push((insert(17, Some(60 * 17), Some(0)), state(vec![rejected(17, RejectCode::SlotsFull, Some(0))], Some(1), 16, 0, fb(16))));
    out.push(case("slots_full_by_count_v0", v0, steps));
    // V0 with 384-vertex boundaries: seven fit the slot bytes, an eighth doesn't.
    let mut steps = vec![(big(insert(1, None, Some(0))), state(vec![ack(1, "applied", Some(0))], Some(1), 1, 15, Some(V0B - R384)))];
    for v in 2..=7u32 {
        let held = v as usize;
        steps.push((big(insert(v, Some(60 * v as i64), Some(0))), state(vec![ack(v, "received", Some(0))], Some(1), v, 16 - held, Some(V0B - held * R384))));
    }
    steps.push((big(insert(8, Some(480), Some(0))), state(vec![rejected(8, RejectCode::SlotsFull, Some(0))], Some(1), 7, 9, Some(V0B - 7 * R384))));
    steps.push((insert(9, Some(540), Some(0)), state(vec![ack(9, "received", Some(0))], Some(1), 9, 8, Some(V0B - 7 * R384 - R4))));
    steps.push((big(insert(10, None, Some(1))), state(vec![ack(10, "applied", Some(1))], Some(10), 10, 15, Some(V0B - R384))));
    out.push(case("slots_full_by_bytes", v0, steps));
    out.push(case(
        "boot_without_clock",
        v0,
        vec![
            (insert(1, None, Some(0)), state(vec![ack(1, "applied", Some(0))], Some(1), 1, 15, fb(1))),
            (insert(2, Some(60), Some(0)), state(vec![ack(2, "received", Some(0))], Some(1), 2, 14, fb(2))),
            (boot(), state(vec![], Some(1), 2, 14, fb(2))),
            // No clock: staged, and it kills 2 (it activates first).
            (insert(3, Some(30), None), state(vec![ack(3, "received", None)], Some(1), 3, 14, fb(2))),
            // Already past, but without a fix the collar can't know: it waits.
            (insert(4, Some(-100), None), state(vec![ack(4, "received", None)], Some(1), 4, 14, fb(2))),
            (tick(200), state(vec![ack(4, "applied", Some(200))], Some(4), 4, 15, fb(1))),
            (insert(5, None, None), state(vec![ack(5, "applied", None)], Some(5), 5, 15, fb(1))),
            // The last fix (200) is a clock again: 150 has passed.
            (insert(6, Some(150), None), state(vec![ack(6, "applied", None)], Some(6), 6, 15, fb(1))),
        ],
    ));
    let scoped = |collar: &str, step: SlotStep| SlotStep { collar_id: Some(collar.into()), ..step };
    out.push(case(
        "wrong_collar",
        v0,
        vec![
            (scoped("col_01J9OTHER", insert(1, None, Some(0))), state(vec![rejected(1, RejectCode::WrongCollar, Some(0))], None, 0, 16, Some(V0B))),
            (scoped(SLOT_COLLAR, insert(1, None, Some(0))), state(vec![ack(1, "applied", Some(0))], Some(1), 1, 15, fb(1))),
            (insert(2, None, Some(5)), state(vec![ack(2, "applied", Some(5))], Some(2), 2, 15, fb(1))),
        ],
    ));
    out.push(case(
        "wrong_herd",
        v0,
        vec![(
            SlotStep { herd_id: Some("herd_01J7OTHER".into()), ..insert(1, None, Some(0)) },
            state(vec![rejected(1, RejectCode::WrongHerd, Some(0))], None, 0, 16, Some(V0B)),
        )],
    ));
    out.push(case(
        "past_effective_at_applies_at_once",
        v0,
        vec![(insert(1, Some(-60), Some(0)), state(vec![ack(1, "applied", Some(0))], Some(1), 1, 15, fb(1)))],
    ));
    out
}

/// A valid boundary of `total` vertices within `limits`: a 400 m circle,
/// plus 20 m holes of up to `hole_vertices` each on a 100 m grid.
fn shape_with(total: usize, limits: &CollarLimits) -> Polygon {
    let outer_n = total.min(limits.outer);
    let outer = circle_m(0.0, 0.0, 400.0, outer_n);
    let mut left = total - outer_n;
    let mut holes = Vec::new();
    let mut spot = 0;
    while left > 0 {
        let mut k = left.min(limits.hole_vertices);
        if left - k > 0 && left - k < 3 {
            k -= 3 - (left - k);
        }
        holes.push(circle_m(-150.0 + 100.0 * (spot % 4) as f64, -150.0 + 100.0 * (spot / 4) as f64, 20.0, k));
        left -= k;
        spot += 1;
    }
    Polygon::from_rings(outer, holes)
}

fn slot_command(case: &SlotCase, step: &SlotStep) -> BoundaryCommand {
    let version = step.version.unwrap();
    let poly = shape_with(step.vertices.unwrap_or(4), &case.limits);
    let mut cmd = BoundaryCommand::from_shape(format!("bnd_v{version}"), version, &poly, step.effective_at.as_deref().map(t)).unwrap();
    cmd.herd_id = Some(step.herd_id.clone().unwrap_or_else(|| case.herd_id.clone()));
    cmd.collar_id = step.collar_id.clone();
    assert_eq!(cmd.total_vertices(), step.vertices.unwrap_or(4));
    cmd
}

// ---------------------------------------------------------------- files

fn render() -> Vec<(&'static str, String)> {
    let pk = json(&encode_public_key(&key().verifying_key()));
    let header = |extra: Vec<(&'static str, String)>| {
        let mut h = vec![("seed_hex", json(&seed_hex())), ("public_key", pk.clone())];
        h.extend(extra);
        h
    };
    vec![
        ("commands.json", file(&header(vec![("max_bytes", json(&MAX_COMMAND_BYTES)), ("max_keys", json(&MAX_TOP_LEVEL_KEYS))]), &command_cases())),
        ("config.json", file(&header(vec![("collar_id", json(&SIM_COLLAR)), ("held_version", json(&HELD_CONFIG))]), &config_cases())),
        ("shapes.json", file(&[("slack_m", json(&0.0))], &shape_cases())),
        ("geofence.json", file(&[("tolerance_m", json(&0.001))], &geofence_cases())),
        ("slots.json", file(&[("start", json(&T0))], &slot_cases())),
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
            assert!(on_disk == text, "{name} is out of date: run OP_WRITE_VECTORS=1 cargo test -p op-protocol --test vectors");
        }
    }
}

/// The committed file (or, while rewriting, the fresh text, so the checks
/// don't race the writer).
fn read<T: serde::de::DeserializeOwned>(name: &str) -> T {
    let text = if writing() {
        render().into_iter().find(|(n, _)| *n == name).unwrap().1
    } else {
        std::fs::read_to_string(dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
    };
    serde_json::from_str(&text).unwrap()
}

fn outcome(r: Result<(), RejectCode>) -> (bool, Option<String>) {
    match r {
        Ok(()) => (true, None),
        Err(c) => (false, Some(c.as_str().to_string())),
    }
}

#[test]
fn commands_agree() {
    let f: WireFile = read("commands.json");
    let pk = decode_public_key(&f.public_key).unwrap();
    assert!(f.cases.len() >= 30);
    for c in &f.cases {
        let got = outcome(verify_wire(c.wire.as_bytes(), &pk).map(|_| ()));
        assert_eq!(got, (c.valid, c.code.clone()), "{}", c.name);
        if let Some(canonical) = &c.canonical {
            assert_eq!(canonical_wire(c.wire.as_bytes()).unwrap().bytes, canonical.as_bytes(), "{}", c.name);
        }
    }
    // verify_json passes on the duplicate, nested and oversize cases verify_wire rejects.
    for name in ["duplicate_key_first_differs", "nested_object", "object_in_array", "seventeen_keys", "oversize_12289_bytes", "arrays_too_deep"] {
        let c = f.cases.iter().find(|c| c.name == name).unwrap();
        verify_json(&serde_json::from_str(&c.wire).unwrap(), &pk).unwrap_or_else(|e| panic!("{name}: verify_json {e}"));
    }
}

#[test]
fn config_agrees() {
    let f: WireFile = read("config.json");
    let pk = decode_public_key(&f.public_key).unwrap();
    let (collar, held) = (f.collar_id.unwrap(), f.held_version);
    for c in &f.cases {
        let got = outcome(verify_config_wire(c.wire.as_bytes(), &pk).and_then(|cfg| cfg.check(&collar, held)));
        assert_eq!(got, (c.valid, c.code.clone()), "{}", c.name);
    }
}

#[test]
fn shapes_agree() {
    #[derive(Deserialize)]
    struct F {
        slack_m: f64,
        cases: Vec<ShapeCase>,
    }
    let f: F = read("shapes.json");
    for c in &f.cases {
        let got = outcome(op_geo::shape::check_rings(&c.boundary, &c.holes, &c.limits, c.warn_m, c.hysteresis_m, f.slack_m).map_err(RejectCode::from));
        assert_eq!(got, (c.ok, c.code.clone()), "{}", c.name);
    }
    for code in op_geo::ShapeCode::ALL {
        assert!(f.cases.iter().any(|c| c.code.as_deref() == Some(code.as_str())), "no case for {code}");
    }
}

#[test]
fn geofence_agrees() {
    #[derive(Deserialize)]
    struct F {
        tolerance_m: f64,
        cases: Vec<GeoCase>,
    }
    let f: F = read("geofence.json");
    for c in &f.cases {
        let poly = Polygon::from_rings(c.boundary.clone(), c.holes.iter().cloned());
        let gf = Geofence::from_polygon(GeofenceConfig::default(), &poly, 1, &CollarLimits::V0).unwrap();
        let (margin, ring) = gf.measure(c.point);
        assert!((margin - c.margin_m).abs() <= f.tolerance_m, "{}: {margin} vs {}", c.name, c.margin_m);
        assert_eq!((margin > 0.0, ring), (c.inside, c.nearest_ring), "{}", c.name);
    }
}

#[test]
fn slots_agree() {
    #[derive(Deserialize)]
    struct F {
        cases: Vec<SlotCase>,
    }
    let f: F = read("slots.json");
    let fmt = |a: &SlotAck| ExpAck {
        version: a.version,
        status: a.status.as_str().into(),
        code: a.code.map(|c| c.as_str().into()),
        at: a.at.as_ref().map(wire_time::format),
    };
    for case in &f.cases {
        let mut store = SlotStore::new(case.limits, Some(case.herd_id.clone()), Some(case.collar_id.clone()));
        for (i, (step, expect)) in case.steps.iter().zip(&case.expect).enumerate() {
            let acks: Vec<ExpAck> = match step.op.as_str() {
                "insert" => vec![fmt(&store.insert(slot_command(case, step), step.now.as_deref().map(t)))],
                "tick" => store.tick(t(step.now.as_deref().unwrap())).iter().map(fmt).collect(),
                "boot" => {
                    store.boot();
                    vec![]
                }
                op => panic!("unknown op {op}"),
            };
            let got = Expect { acks, active: store.active().map(|a| a.cmd.version), have: store.have(), free: store.free(), free_bytes: store.free_bytes() };
            assert_eq!(&got, expect, "{} step {i} ({})", case.name, step.op);
        }
    }
}
