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

// ---- Protocol v1 ----

/// Signed with seed bytes 00..1f by the code before protocol v1 (base e1ac4ec).
const GOLDEN_V0: &str = r#"{"command_id":"bnd_01J8V0GOLDEN","herd_id":"herd_01J7GOLDEN","version":57,"effective_at":"2026-09-27T12:00:00Z","boundary":[[-92.41,38.12],[-92.4,38.12],[-92.4,38.13],[-92.41,38.13]],"warn_m":5.0,"hysteresis_m":1.0,"sig":"lsSq582udh5guEV7Y6NRw8kmI6XHaaFTOqOjTQZMw9DBg8mvFcHb8s6gROO0S5YQh20IAXxtiEPTKFxs+6+iCg=="}"#;
const GOLDEN_V0_BARE: &str = r#"{"command_id":"bnd_x","version":3,"boundary":[[-92.41,38.12],[-92.4,38.12],[-92.4,38.13],[-92.41,38.13]],"sig":"EAMXMzOMqAqfcCJ9qm0L2TDcdLR/50XmQIXrBPc2wruZD1XNPKQqDdFYe3ot3Weo0t3qRC7eyyh5qGgs7UFdCA=="}"#;

fn seed_key() -> SigningKey {
    SigningKey::from_bytes(&core::array::from_fn(|i| i as u8))
}

fn home() -> Polygon {
    serde_json::from_value(json!({"type": "Polygon", "coordinates": [[[-92.41, 38.12], [-92.40, 38.12], [-92.40, 38.13], [-92.41, 38.13], [-92.41, 38.12]]]}))
        .unwrap()
}

#[test]
fn v0_commands_serialize_and_sign_byte_for_byte_as_before() {
    let key = seed_key();
    let mut cmd = BoundaryCommand::from_polygon("bnd_01J8V0GOLDEN", 57, &home(), Some(t("2026-09-27T12:00:00Z"))).unwrap();
    cmd.herd_id = Some("herd_01J7GOLDEN".into());
    cmd.warn_m = Some(5.0);
    cmd.hysteresis_m = Some(1.0);
    sign_command(&mut cmd, &key);
    assert_eq!(serde_json::to_string(&cmd).unwrap(), GOLDEN_V0);
    let mut bare = BoundaryCommand::from_polygon("bnd_x", 3, &home(), None).unwrap();
    sign_command(&mut bare, &key);
    assert_eq!(serde_json::to_string(&bare).unwrap(), GOLDEN_V0_BARE);
    assert_eq!(encode_public_key(&key.verifying_key()), "A6EHv/POEL4dcN0Y50vAmWfk1jCbpQ1fHdyGZBJVMbg=");
}

#[test]
fn old_signatures_verify() {
    let pk = seed_key().verifying_key();
    for golden in [GOLDEN_V0, GOLDEN_V0_BARE] {
        let parsed: BoundaryCommand = serde_json::from_str(golden).unwrap();
        verify_command(&parsed, &pk).unwrap();
        verify_json(&serde_json::from_str(golden).unwrap(), &pk).unwrap();
        assert_eq!(verify_wire(golden.as_bytes(), &pk).unwrap(), parsed);
        assert!(parsed.holes.is_empty() && parsed.collar_id.is_none() && parsed.cue_mode == CueMode::Audio);
    }
}

#[test]
fn new_fields_are_omitted_when_empty_and_signed_when_present() {
    let key = seed_key();
    let mut cmd = BoundaryCommand::from_shape("bnd_1", 9, &home(), None).unwrap();
    let json = serde_json::to_string(&cmd).unwrap();
    for field in ["holes", "collar_id", "cue_mode", "herd_id", "effective_at", "warn_m", "sig"] {
        assert!(!json.contains(field), "{field} in {json}");
    }
    cmd.holes = vec![vec![[-92.406, 38.124], [-92.404, 38.124], [-92.404, 38.126]]];
    cmd.collar_id = Some("col_a".into());
    cmd.cue_mode = CueMode::Track;
    sign_command(&mut cmd, &key);
    let json = serde_json::to_string(&cmd).unwrap();
    assert!(
        json.contains(r#""collar_id":"col_a","version":9"#) && json.contains(r#""holes":[[[-92.406,38.124]"#) && json.contains(r#""cue_mode":"track""#),
        "{json}"
    );
    verify_command(&cmd, &key.verifying_key()).unwrap();
    for tamper in [
        |c: &mut BoundaryCommand| c.holes.clear(),
        |c: &mut BoundaryCommand| c.collar_id = Some("col_b".into()),
        |c: &mut BoundaryCommand| c.cue_mode = CueMode::Audio,
    ] {
        let mut c = cmd.clone();
        tamper(&mut c);
        assert_eq!(verify_command(&c, &key.verifying_key()), Err(ProtocolError::BadSignature));
    }
    // An explicit "audio" parses to the default.
    let explicit: BoundaryCommand = serde_json::from_str(r#"{"command_id":"b","version":1,"boundary":[],"cue_mode":"audio"}"#).unwrap();
    assert_eq!(explicit.cue_mode, CueMode::Audio);
    // Other wire types: nothing new appears until set.
    let ack = Ack { command_id: "bnd_x".into(), version: 3, status: AckStatus::Applied, at: t("2026-09-27T12:00:00Z"), ..Default::default() };
    assert_eq!(serde_json::to_string(&ack).unwrap(), r#"{"command_id":"bnd_x","version":3,"status":"applied","at":"2026-09-27T12:00:00Z"}"#);
    let rep = PositionReport {
        boundary_version: Some(3),
        fixes: vec![WireFix { at: t("2026-09-27T12:00:00Z"), point: [-92.405, 38.125], accuracy_m: 1.8, sats: Some(9), ..Default::default() }],
        cues: vec![WireCue { at: t("2026-09-27T12:00:00Z"), level: 2, margin_m: 1.4, ..Default::default() }],
        battery: Some(0.81),
        ..Default::default()
    };
    assert_eq!(
        serde_json::to_string(&rep).unwrap(),
        r#"{"boundary_version":3,"fixes":[{"at":"2026-09-27T12:00:00Z","point":[-92.405,38.125],"accuracy_m":1.8,"sats":9}],"cues":[{"at":"2026-09-27T12:00:00Z","level":2,"margin_m":1.4}],"battery":0.81}"#
    );
    assert_eq!(serde_json::to_string(&ReportResponse { latest_version: Some(3), ..Default::default() }).unwrap(), r#"{"latest_version":3}"#);
}

#[test]
fn ids_are_at_most_64_bytes() {
    let mut cmd = BoundaryCommand::from_polygon("b".repeat(64), 1, &home(), None).unwrap();
    cmd.validate(None).unwrap();
    cmd.check_ids().unwrap();
    cmd.command_id = "b".repeat(65);
    assert_eq!(cmd.validate(None), Err(ProtocolError::Invalid("command_id")));
    assert_eq!(cmd.check_ids(), Err(RejectCode::BadJson));
    cmd.command_id = "b".into();
    cmd.herd_id = Some("h".repeat(65));
    assert_eq!(cmd.check_ids(), Err(RejectCode::BadJson));
    cmd.herd_id = None;
    cmd.collar_id = Some(String::new());
    assert_eq!(cmd.check_ids(), Err(RejectCode::BadJson));
}

#[test]
fn check_codes() {
    let d = GeofenceConfig::default();
    let limits = CollarLimits::V0;
    let mut cmd = BoundaryCommand::from_shape("b", 1, &home(), None).unwrap();
    cmd.herd_id = Some("herd_a".into());
    cmd.check(Some("herd_a"), Some("col_a"), &limits, &d).unwrap();
    cmd.check(None, None, &limits, &d).unwrap();
    assert_eq!(cmd.check(Some("herd_b"), None, &limits, &d), Err(RejectCode::WrongHerd));
    cmd.collar_id = Some("col_a".into());
    assert_eq!(cmd.check(Some("herd_a"), Some("col_b"), &limits, &d), Err(RejectCode::WrongCollar));
    cmd.holes = vec![vec![[-92.406, 38.124], [-92.404, 38.124], [-92.404, 38.126]]];
    cmd.check(Some("herd_a"), Some("col_a"), &limits, &d).unwrap();
    assert_eq!(cmd.check(Some("herd_a"), Some("col_a"), &CollarLimits::LEGACY, &d), Err(RejectCode::TooManyHoles));
    cmd.warn_m = Some(-1.0);
    assert_eq!(cmd.check_shape(&limits, &d), Err(RejectCode::BadMargins));
    cmd.warn_m = None;
    cmd.boundary.push([-92.405, 38.14]);
    assert_eq!(cmd.check_shape(&limits, &d), Err(RejectCode::SelfIntersecting));
    // Every shape code maps to the rejection code of the same name.
    for c in op_geo::ShapeCode::ALL {
        assert_eq!(RejectCode::from(c).as_str(), c.as_str());
    }
    for c in RejectCode::ALL {
        assert_eq!(RejectCode::parse(c.as_str()), Some(c));
        assert_eq!(serde_json::to_value(c).unwrap(), c.as_str());
        assert_eq!(c.is_permanent(), c != RejectCode::SlotsFull);
    }
    assert_eq!(cmd.total_vertices(), 5 + 3);
    assert_eq!(cmd.record_bytes(), 192 + 8 * 8);
}

#[test]
fn contract_command_example_parses() {
    let cmd: BoundaryCommand = serde_json::from_value(json!({
      "command_id": "bnd_01J8...",
      "herd_id": "herd_01J7...",
      "collar_id": "col_01J9...",
      "version": 57,
      "effective_at": "2026-09-27T12:00:00Z",
      "boundary": [[-92.41,38.12],[-92.4,38.12],[-92.4,38.13],[-92.41,38.13]],
      "holes": [[[-92.406,38.124],[-92.404,38.124],[-92.404,38.126]]],
      "cue_mode": "track",
      "warn_m": 5.0,
      "hysteresis_m": 1.0,
      "sig": "base64..."
    }))
    .unwrap();
    assert_eq!((cmd.holes.len(), cmd.cue_mode, cmd.collar_id.as_deref()), (1, CueMode::Track, Some("col_01J9...")));
    cmd.check_shape(&CollarLimits::V0, &GeofenceConfig::default()).unwrap();
    let gf = cmd.fence(GeofenceConfig::default(), &CollarLimits::V0).unwrap();
    assert!(gf.margin_m([-92.4045, 38.1245]) < 0.0, "in the hole");
    assert!(cmd.geofence(GeofenceConfig::default()).unwrap().margin_m([-92.4045, 38.1245]) > 0.0, "the one-ring fence ignores holes");
}

#[test]
fn readme_v1_report_parses() {
    let report: PositionReport = serde_json::from_value(json!({
      "boundary_version": 57,
      "device": {"fw": "0.2.0", "caps": ["holes","slots","collar_id","cue_mode","episodes","config"],
                 "limits": {"outer":128,"holes":16,"hole_vertices":32,"total":384,"slots":16,"slot_bytes":24576},
                 "config_version": 3},
      "slots": [{"version":57,"status":"applied"},{"version":58,"status":"received","effective_at":"2026-09-27T12:00:00Z"}],
      "fixes": [{"at":"2026-09-27T11:00:00Z","point":[-92.405,38.124],"accuracy_m":1.8,"sats":9,"cn0":41.0,"hdop":0.9}],
      "cues": [{"at":"2026-09-27T11:00:00Z","kind":"warn","level":2,"dur_ms":300,"margin_m":1.4,"ring":2,"boundary_version":57}],
      "episodes": [{"start":"2026-09-27T10:59:50Z","end":"2026-09-27T11:00:10Z","boundary_version":57,"ring":2,"cues":4,"max_level":3,"min_margin_m":0.8,"outcome":"turned_back"}],
      "battery": 0.81,
      "health": {"fix_attempts":120,"fix_ok":118,
                 "cell": {"rsrp_dbm":-104,"rsrq_db":-11,"snr_db":6,"mode":"ltem","band":12,"cell_id":"1A2B3C","tac":1234,"at":"2026-09-27T11:00:00Z"},
                 "still_s":40,"tilt_deg":12,"temp_c":21.5,"battery_v":3.31,"charging":true,"uptime_s":86400,"reset":"power_on"}
    }))
    .unwrap();
    report.validate().unwrap();
    let device = report.device.as_ref().unwrap();
    assert_eq!(device.limits_or_default(), CollarLimits::V0);
    assert!(caps::ALL.iter().all(|c| device.has(c)));
    assert_eq!(device.config_version, Some(3));
    let slots = report.slots.as_ref().unwrap();
    assert_eq!((slots[1].version, slots[1].status), (58, AckStatus::Received));
    assert_eq!(report.fixes[0].hdop, Some(0.9));
    assert_eq!((report.cues[0].kind, report.cues[0].ring, report.cues[0].dur_ms), (Some(CueKind::Warn), Some(2), Some(300)));
    assert_eq!(report.episodes[0].outcome, EpisodeOutcome::TurnedBack);
    let h = report.health.as_ref().unwrap();
    assert_eq!((h.fix_ok, h.cell.as_ref().unwrap().rsrp_dbm, h.charging, h.reset.as_deref()), (Some(118), Some(-104.0), Some(true), Some("power_on")));
    // Round trip keeps every field.
    let back: PositionReport = serde_json::from_str(&serde_json::to_string(&report).unwrap()).unwrap();
    assert_eq!(back, report);

    // Empty slots differ from absent ones.
    let empty: PositionReport = serde_json::from_value(json!({"slots": []})).unwrap();
    assert_eq!(empty.slots, Some(vec![]));
    assert_eq!(PositionReport::default().slots, None);
}

#[test]
fn device_defaults() {
    assert_eq!(DeviceInfo::default().limits_or_default(), CollarLimits::LEGACY);
    assert_eq!(DeviceInfo { caps: vec!["holes".into()], ..Default::default() }.limits_or_default(), CollarLimits::V0);
    let v1 = DeviceInfo { caps: vec!["slots".into()], limits: Some(CollarLimits::V1), ..Default::default() };
    assert_eq!(v1.limits_or_default(), CollarLimits::V1);
}

#[test]
fn report_validation_covers_new_fields() {
    let ok = PositionReport::default();
    ok.validate().unwrap();
    let bad_limits = PositionReport {
        device: Some(DeviceInfo { caps: vec!["holes".into()], limits: Some(CollarLimits { outer: 0, ..CollarLimits::V0 }), ..Default::default() }),
        ..Default::default()
    };
    assert_eq!(bad_limits.validate(), Err(ProtocolError::Invalid("device limits")));
    let rejected_slot = PositionReport { slots: Some(vec![SlotReport { version: 1, status: AckStatus::Rejected, effective_at: None }]), ..Default::default() };
    assert_eq!(rejected_slot.validate(), Err(ProtocolError::Invalid("slots")));
    let at = t("2026-09-27T12:00:00Z");
    let backwards =
        PositionReport { episodes: vec![WireEpisode { start: at, end: at - chrono::Duration::seconds(1), ..Default::default() }], ..Default::default() };
    assert_eq!(backwards.validate(), Err(ProtocolError::Invalid("episode")));
    let nan = PositionReport { health: Some(Health { temp_c: Some(f64::NAN), ..Default::default() }), ..Default::default() };
    assert_eq!(nan.validate(), Err(ProtocolError::Invalid("health")));
}

#[test]
fn ack_codes_parse_leniently() {
    let ack: Ack =
        serde_json::from_value(json!({"command_id":"b","version":2,"status":"rejected","code":"hole_too_close","at":"2026-09-27T12:00:00Z"})).unwrap();
    assert_eq!(ack.code, Some(RejectCode::HoleTooClose));
    assert!(serde_json::to_string(&ack).unwrap().contains(r#""code":"hole_too_close""#));
    // A newer firmware's code doesn't make the server refuse the ack.
    let ack: Ack =
        serde_json::from_value(json!({"command_id":"b","version":2,"status":"rejected","code":"from_the_future","at":"2026-09-27T12:00:00Z"})).unwrap();
    assert_eq!(ack.code, None);
    let reject: ConfigReject = serde_json::from_value(json!({"version": 3, "code": "bad_config"})).unwrap();
    assert_eq!(reject.code, Some(RejectCode::BadConfig));
}

#[test]
fn episodes_convert_from_the_cue_policy() {
    let e = op_geo::Episode {
        start: 1_790_000_000_000,
        end: 1_790_000_012_000,
        ring: 2,
        cues: 4,
        max_level: 3,
        min_margin_m: 0.8,
        outcome: EpisodeOutcome::Crossed,
    };
    let w = WireEpisode::from_episode(&e, Some(57)).unwrap();
    assert_eq!((w.end - w.start).num_seconds(), 12);
    assert_eq!((w.ring, w.boundary_version, w.outcome), (2, Some(57), EpisodeOutcome::Crossed));
    assert_eq!(serde_json::to_value(&w).unwrap()["outcome"], "crossed");
}

#[test]
fn report_response_carries_the_config() {
    let mut config = ConfigCommand { command_id: "cfg_1".into(), collar_id: "col_a".into(), version: 2, report_s: 60, poll_s: 60, ..Default::default() };
    sign_config(&mut config, &seed_key());
    let r = ReportResponse { latest_version: Some(7), config: Some(config.clone()) };
    let json = serde_json::to_string(&r).unwrap();
    let back: ReportResponse = serde_json::from_str(&json).unwrap();
    verify_config(back.config.as_ref().unwrap(), &seed_key().verifying_key()).unwrap();
    // The config object's own bytes verify the way a collar checks them.
    let v: serde_json::Value = serde_json::from_str(&json).unwrap();
    let wire = serde_json::to_string(&v["config"]).unwrap();
    assert_eq!(verify_config_wire(wire.as_bytes(), &seed_key().verifying_key()).unwrap(), config);
}
