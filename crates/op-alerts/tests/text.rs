//! Alert texts: the contract's examples exactly, and every template within
//! 160 GSM-7 characters with worst-case names in both unit systems.

mod a_engine_fixture;

use a_engine_fixture::{t, t0};
use chrono::Duration;
use op_alerts::text::{TextCtx, alert_text, group_text, gsm7, is_gsm7, rollup_title, septets};
use op_core::Severity;
use op_core::alert::{Alert, AlertStatus};
use op_core::time::to_db;
use op_core::units::{Fmt, Units};
use serde_json::{Value, json};

fn alert(kind: &str, key: &str, title: &str, data: Value) -> Alert {
    Alert {
        id: "alr_1".into(),
        kind: kind.into(),
        key: key.into(),
        severity: Severity::Warning,
        status: AlertStatus::Open,
        herd_id: Some("herd_1".into()),
        title: title.into(),
        body: None,
        at: None,
        targets: vec![],
        data,
        opened_at: t0(),
        updated_at: t0(),
        acked_at: None,
        acked_by: None,
        resolved_at: None,
        resolved_by: None,
        rolled_into: None,
    }
}

fn ctx(units: Units) -> TextCtx {
    TextCtx { fmt: Fmt::new(units), tz: chrono_tz::America::Chicago, now: t0() }
}

fn ago(m: i64) -> String {
    to_db(&(t0() - Duration::minutes(m)))
}

#[test]
fn the_contract_examples_read_exactly() {
    let imp = ctx(Units::Imperial);
    let one = alert("escaped", "escaped:col_1", "214 outside P3", json!({"label": "214", "paddock": "P3", "since": ago(6)}));
    assert_eq!(alert_text(&one, Some("200 ft N of east gate"), &imp), "214 outside P3, 200 ft N of east gate, 6m. Reply OK to ack");
    let roll = alert("outside", "outside:herd:herd_1", "31 outside P3", json!({"count": 31, "paddock": "P3", "since": to_db(&t("2026-09-27T11:12:00Z"))}));
    assert_eq!(alert_text(&roll, None, &imp), "31 outside P3 since 06:12. Reply OK to ack");
    let dark =
        alert("herd_silent", "herd_silent:herd:herd_1", "180 of 250 collars silent", json!({"herd": "Cows", "count": 180, "total": 250, "since": ago(25)}));
    assert_eq!(alert_text(&dark, None, &imp), "Cows: 180 of 250 collars silent 25m. Check coverage or the server");
    let dec = alert(
        "decision_waiting",
        "decision_waiting:dec_1",
        "Move to P4?",
        json!({"herd": "Cows", "action": "MOVE", "paddock": "P4", "area_ha": 30.6 / 2.471_053_814_671_653, "days": 3.04, "code": "4821"}),
    );
    assert_eq!(alert_text(&dec, None, &imp), "Cows: move to P4 (30.6 ac, 3 d)? Reply Y or N. Code 4821");
    assert_eq!(alert_text(&dec, None, &ctx(Units::Metric)), "Cows: move to P4 (12.4 ha, 3 d)? Reply Y or N. Code 4821");
}

#[test]
fn a_group_lists_its_animals() {
    let c = ctx(Units::Imperial);
    let alerts: Vec<Alert> =
        ["214", "031", "118"].iter().map(|l| alert("outside", &format!("outside:{l}"), "x", json!({"label": l, "paddock": "P3"}))).collect();
    assert_eq!(group_text(&alerts, None, &c), "3 outside P3: 214 031 118");
    let many: Vec<Alert> = (0..250).map(|i| alert("silent", &format!("silent:{i}"), "x", json!({"label": format!("{}", 1000 + i)}))).collect();
    let text = group_text(&many, None, &c);
    assert!(text.starts_with("250 silent: 1000 1001 "), "{text}");
    assert!(septets(&text) <= 160, "{text}");
    let tail: usize = text.rsplit('+').next().unwrap().parse().unwrap();
    let shown = text.split(": ").nth(1).unwrap().split(' ').count() - 1;
    assert_eq!(shown + tail, 250, "{text}");
    // A count reads as one, not as a collar's label ("138 GPS weak" is collar 138).
    assert_eq!(rollup_title("outside", 31, Some("P3")), "31 collars outside P3");
    assert_eq!(rollup_title("silent", 31, None), "31 collars silent");
    assert_eq!(rollup_title("gps_degraded", 5, None), "5 collars GPS weak");
    assert_eq!(rollup_title("drop_off", 2, None), "2 collars not moving");
    assert_eq!(rollup_title("low_battery", 4, None), "4 batteries low");
    assert_eq!(rollup_title("outside", 1, Some("P3")), "1 collar outside P3");
}

#[test]
fn gsm7_keeps_plain_text_and_replaces_the_rest() {
    assert_eq!(gsm7("Cody’s “North” — field"), "Cody's \"North\" - field");
    assert_eq!(gsm7("Pâturage Été ñ Ø ü"), "Paturage Été ñ Ø ü", "É and é are GSM-7; â is not");
    assert_eq!(gsm7("Cows 🐄🐄  back"), "Cows back");
    assert_eq!(gsm7("5 m² ok…"), "5 m2 ok...");
    assert!(is_gsm7("214 outside P3, 200 ft N of east gate, 6m. Reply OK to ack"));
    assert!(!is_gsm7("m²"));
    assert_eq!(septets("a{b}"), 6);
}

/// The longest names people give things, in the worst characters.
fn worst() -> (String, String, String, String) {
    let herd = "Big “Herd” — Śouth Pâsture Cows 🐄 ".repeat(4);
    let paddock = "Pádd0ck ‘North’ – by the old barn ".repeat(4);
    let tag = "TAG-{[~]}-€€€-123456789".repeat(3);
    let place = "1,230 ft NNW of the far north-east gate by the creek crossing ".repeat(2);
    (herd, paddock, tag, place)
}

fn every_alert() -> Vec<(Alert, bool)> {
    let (herd, paddock, tag, _) = worst();
    let base = json!({"herd": herd, "paddock": paddock, "label": tag, "since": ago(3000), "pct": 3, "accuracy_m": 1234.5, "code": "4821",
                      "count": 250, "total": 250, "area_ha": 12_345.678, "days": 365.0, "staged": true, "action": "MOVE",
                      "labels": (0..250).map(|i| format!("{tag}{i}")).collect::<Vec<_>>(),
                      "no_fix_since": ago(90)});
    let mut out = vec![];
    for kind in [
        "escaped",
        "outside",
        "silent",
        "herd_silent",
        "low_battery",
        "boundary_not_applied",
        "decision_waiting",
        "move_stalled",
        "stragglers",
        "drop_off",
        "gps_degraded",
        "future_kind",
    ] {
        for rollup in [false, true] {
            let key = if rollup { format!("{kind}:herd:herd_1") } else { format!("{kind}:col_1") };
            let mut data = base.clone();
            if !rollup {
                data.as_object_mut().unwrap().remove("count");
            }
            out.push((alert(kind, &key, &format!("{tag} {paddock}"), data.clone()), rollup));
            // Without the optional facts too.
            let mut bare = data;
            for k in ["paddock", "area_ha", "days", "accuracy_m", "total"] {
                bare.as_object_mut().unwrap().remove(k);
            }
            bare["action"] = json!("STAY");
            bare["labels"] = json!([tag]);
            out.push((alert(kind, &key, &tag, bare), rollup));
        }
    }
    out
}

#[test]
fn every_template_fits_160_gsm7_characters_with_worst_case_names() {
    let (_, _, _, place) = worst();
    for units in [Units::Metric, Units::Imperial] {
        let c = ctx(units);
        for (a, _) in every_alert() {
            for p in [None, Some(place.as_str()), Some("in P3")] {
                let text = alert_text(&a, p, &c);
                assert!(is_gsm7(&text), "{} not GSM-7: {text}", a.kind);
                assert!(septets(&text) <= 160, "{} is {} long: {text}", a.kind, septets(&text));
                assert!(!text.trim().is_empty());
                if a.kind == "decision_waiting" {
                    assert!(text.ends_with("Reply Y or N. Code 4821"), "{text}");
                }
            }
            let group = group_text(&[a.clone(), a.clone(), a.clone()], None, &c);
            assert!(is_gsm7(&group) && septets(&group) <= 160, "group {}: {group}", a.kind);
        }
    }
}

#[test]
fn texts_use_the_farm_units() {
    let a = alert("gps_degraded", "gps_degraded:col_1", "207 GPS weak", json!({"label": "207", "accuracy_m": 12.2}));
    assert_eq!(alert_text(&a, None, &ctx(Units::Metric)), "207 GPS weak, 12 m accuracy");
    assert_eq!(alert_text(&a, None, &ctx(Units::Imperial)), "207 GPS weak, 40 ft accuracy");
    let b = alert("low_battery", "low_battery:col_1", "031 battery 14%", json!({"label": "031", "pct": 14}));
    assert_eq!(alert_text(&b, None, &ctx(Units::Metric)), "031 battery 14%");
    let s = alert("silent", "silent:col_1", "214 silent", json!({"label": "214", "since": ago(25)}));
    assert_eq!(alert_text(&s, Some("60 m N of P3"), &ctx(Units::Metric)), "214 silent 25m, last 60 m N of P3. Reply OK to ack");
    let m = alert("move_stalled", "move_stalled:mov_1", "Move to P4 stalled", json!({"herd": "Cows", "paddock": "P4", "since": ago(15), "staged": false}));
    assert_eq!(alert_text(&m, None, &ctx(Units::Metric)), "Cows: move to P4 stalled 15m. Check the herd");
    let st = alert("decision_waiting", "decision_waiting:dec_1", "Stay in P3?", json!({"herd": "Cows", "action": "STAY", "paddock": "P3", "code": "0912"}));
    assert_eq!(alert_text(&st, None, &ctx(Units::Metric)), "Cows: stay in P3? Reply Y or N. Code 0912");
}
