use super::*;
use serde_json::json;

fn north() -> Polygon {
    serde_json::from_value(json!({"type": "Polygon", "coordinates": [[[-92.40, 38.12], [-92.39, 38.12], [-92.39, 38.13], [-92.40, 38.13], [-92.40, 38.12]]]}))
        .unwrap()
}

fn t(s: &str) -> DateTime<Utc> {
    wire_time::parse(s).unwrap()
}

#[test]
fn readme_boundary_parses_and_validates() {
    let cmd: BoundaryCommand = serde_json::from_value(json!({
        "command_id": "bc_01J8",
        "version": 42,
        "effective_at": "2026-09-25T12:30:00Z",
        "boundary": [[-92.4100, 38.1200], [-92.4000, 38.1200], [-92.4000, 38.1300], [-92.4100, 38.1300]],
        "warn_m": 10,
        "hysteresis_m": 3
    }))
    .unwrap();
    assert_eq!(cmd.version, 42);
    assert_eq!(cmd.warn_m, Some(10.0));
    cmd.validate(Some(41)).unwrap();
    cmd.validate(None).unwrap();
    assert_eq!(cmd.validate(Some(42)), Err(ProtocolError::StaleVersion { version: 42, current: 42 }));
    let out = serde_json::to_value(&cmd).unwrap();
    assert_eq!(out["effective_at"], "2026-09-25T12:30:00Z");
    assert!(out.get("sig").is_none());
}

#[test]
fn from_polygon_is_one_unclosed_ring() {
    let cmd = BoundaryCommand::from_polygon("bc_test", 42, &north(), Some(t("2026-09-25T12:30:00.250Z"))).unwrap();
    assert_eq!(cmd.boundary, vec![[-92.4, 38.12], [-92.39, 38.12], [-92.39, 38.13], [-92.4, 38.13]]);
    assert_eq!(wire_time::format(&cmd.effective_at.unwrap()), "2026-09-25T12:30:00Z");
}

#[test]
fn holes_and_bad_rings_are_refused() {
    let mut holed = north();
    holed.coordinates.push(vec![[-92.396, 38.124], [-92.394, 38.124], [-92.394, 38.126], [-92.396, 38.124]]);
    assert_eq!(BoundaryCommand::from_polygon("bc", 1, &holed, None), Err(ProtocolError::Holes));

    let bowtie = Polygon::from_ring(vec![[-92.41, 38.12], [-92.40, 38.13], [-92.40, 38.12], [-92.41, 38.13]]);
    assert_eq!(BoundaryCommand::from_polygon("bc", 1, &bowtie, None), Err(ProtocolError::Geo(GeoError::SelfIntersecting)));

    let circle: Vec<LonLat> = (0..70)
        .map(|i| {
            let a = 2.0 * std::f64::consts::PI * i as f64 / 70.0;
            [-92.395 + 0.004 * a.cos(), 38.125 + 0.004 * a.sin()]
        })
        .collect();
    let err = BoundaryCommand::from_polygon("bc", 1, &Polygon::from_ring(circle), None).unwrap_err();
    assert!(err.to_string().contains("at most 64"), "{err}");

    let mut cmd = BoundaryCommand::from_polygon("bc", 1, &north(), None).unwrap();
    cmd.boundary[0] = [-200.0, 38.12];
    assert_eq!(cmd.validate(None), Err(ProtocolError::Geo(GeoError::OutOfRange)));
    cmd.boundary.truncate(2);
    assert!(cmd.validate(None).is_err());

    let mut cmd = BoundaryCommand::from_polygon("bc", 1, &north(), None).unwrap();
    cmd.warn_m = Some(-1.0);
    assert_eq!(cmd.validate(None), Err(ProtocolError::Invalid("warn_m")));
}

#[test]
fn canonical_json_sorts_keys_without_whitespace() {
    let v = json!({"b": 1, "a": {"z": [1.5, "x"], "y": null}, "c": 10.0});
    assert_eq!(canonical_json(&v), r#"{"a":{"y":null,"z":[1.5,"x"]},"b":1,"c":10.0}"#);
}

#[test]
fn sign_and_verify() {
    let key = generate_signing_key();
    let mut cmd = BoundaryCommand::from_polygon("bc_1", 7, &north(), Some(t("2026-09-25T12:30:00Z"))).unwrap();
    cmd.warn_m = Some(10.0);
    assert_eq!(verify_command(&cmd, &key.verifying_key()), Err(ProtocolError::MissingSignature));
    sign_command(&mut cmd, &key);
    verify_command(&cmd, &key.verifying_key()).unwrap();

    // As a collar sees it: raw JSON off the wire.
    let raw: serde_json::Value = serde_json::from_str(&serde_json::to_string_pretty(&cmd).unwrap()).unwrap();
    verify_json(&raw, &key.verifying_key()).unwrap();

    // Another server's key fails.
    let other = generate_signing_key();
    assert_eq!(verify_command(&cmd, &other.verifying_key()), Err(ProtocolError::BadSignature));

    // Tampering fails.
    let mut tampered = cmd.clone();
    tampered.version = 8;
    assert_eq!(verify_command(&tampered, &key.verifying_key()), Err(ProtocolError::BadSignature));

    // The herd is signed: moving a command to another herd breaks the signature.
    let mut for_herd = cmd.clone();
    for_herd.herd_id = Some("herd_a".into());
    sign_command(&mut for_herd, &key);
    verify_command(&for_herd, &key.verifying_key()).unwrap();
    assert!(serde_json::to_string(&for_herd).unwrap().contains(r#""herd_id":"herd_a""#));
    for_herd.check_herd("herd_a").unwrap();
    assert_eq!(for_herd.check_herd("herd_b"), Err(ProtocolError::WrongHerd));
    let mut moved = for_herd.clone();
    moved.herd_id = Some("herd_b".into());
    assert_eq!(verify_command(&moved, &key.verifying_key()), Err(ProtocolError::BadSignature));
    let mut stripped = for_herd.clone();
    stripped.herd_id = None;
    assert_eq!(verify_command(&stripped, &key.verifying_key()), Err(ProtocolError::BadSignature));
    cmd.check_herd("anything").unwrap();

    // Keys round trip through base64.
    let pk = decode_public_key(&encode_public_key(&key.verifying_key())).unwrap();
    verify_command(&cmd, &pk).unwrap();
    let sk = decode_signing_key(&encode_signing_key(&key)).unwrap();
    assert_eq!(sk.verifying_key(), key.verifying_key());
    assert_eq!(decode_public_key("nope"), Err(ProtocolError::BadKey));
}

#[test]
fn readme_report_and_ack_parse() {
    let report: PositionReport = serde_json::from_value(json!({
        "collar_id": "oc_0012",
        "boundary_version": 42,
        "fixes": [{ "at": "2026-09-25T10:40:00Z", "point": [-92.4051, 38.1244], "accuracy_m": 1.8, "sats": 9 }],
        "cues": [{ "at": "2026-09-25T10:12:00Z", "level": 2, "margin_m": -1.4 }],
        "battery": 0.81
    }))
    .unwrap();
    report.validate().unwrap();
    assert_eq!(report.collar_id.as_deref(), Some("oc_0012"));
    assert_eq!(report.fixes[0].sats, Some(9));
    assert_eq!(report.cues[0].margin_m, -1.4);
    assert!(report.health.is_none());

    // Kit contract names and health.
    let report: PositionReport = serde_json::from_value(json!({
        "fixes": [{ "at": "2026-09-25T10:40:00+01:00", "point": [-92.4, 38.1], "accuracy_meters": 2.0, "cn0": 38.5, "ttf_s": 4.0 }],
        "health": { "sats": 11, "cn0": 40.0, "ttf_s": 2.5 }
    }))
    .unwrap();
    assert_eq!(report.fixes[0].accuracy_m, 2.0);
    assert_eq!(wire_time::format(&report.fixes[0].at), "2026-09-25T09:40:00Z");
    assert_eq!(report.health.as_ref().unwrap().sats, Some(11));

    let bad = PositionReport { battery: Some(1.5), ..report.clone() };
    assert_eq!(bad.validate(), Err(ProtocolError::Invalid("battery")));

    let ack: Ack = serde_json::from_value(json!({ "command_id": "bc_01J8", "version": 42, "status": "applied", "at": "2026-09-25T12:30:04Z" })).unwrap();
    assert_eq!(ack.status, AckStatus::Applied);
    let ack: Ack = serde_json::from_value(
        json!({ "command_id": "bc", "version": 1, "status": "rejected", "reason": "Ring crosses itself.", "received_at": "2026-09-25T12:30:04Z" }),
    )
    .unwrap();
    assert_eq!(ack.reason.as_deref(), Some("Ring crosses itself."));
    assert!(serde_json::from_value::<Ack>(json!({ "command_id": "bc", "version": 1, "status": "maybe", "at": "2026-09-25T12:30:04Z" })).is_err());
}

#[test]
fn command_geofence_uses_overrides() {
    let mut cmd = BoundaryCommand::from_polygon("bc", 3, &north(), None).unwrap();
    cmd.warn_m = Some(10.0);
    let gf = cmd.geofence(GeofenceConfig::default()).unwrap();
    assert_eq!(gf.config().warn_m, 10.0);
    assert_eq!(gf.config().hysteresis_m, 1.0);
    assert_eq!(gf.version(), 3);
    assert!(gf.margin_m([-92.395, 38.125]) > 400.0);
}
