//! The engine: rollups, dedupe, severity, resolve and reopen, wake-ups, the
//! REST routes and MCP tools, and how long evaluation takes at 250 collars.

mod a_engine_fixture;

use std::collections::HashSet;
use std::time::Instant;

use a_engine_fixture::*;
use axum::http::StatusCode;
use op_core::alert::AlertStatus;
use op_core::time::to_db;
use op_core::tools::ToolScope;
use op_core::{Event, Identity, Role, Severity, Via};
use serde_json::json;

async fn two_people(f: &Farm) -> (String, String) {
    f.sms().await;
    let a = f.person("Cody", Role::Owner, Some("+15155550101"), true, None).await;
    let b = f.person("Sam", Role::Hand, Some("+15155550102"), true, None).await;
    (a, b)
}

#[tokio::test]
async fn a_breakout_of_250_is_one_alert_and_one_text_per_person() {
    let f = Farm::new().await;
    let (a, b) = two_people(&f).await;
    let t = t0();
    let cs = f.collars(250, t).await;
    for c in &cs {
        f.outside(c, t - mins(2), t).await;
        f.escape(c, "returning", t - mins(1), None).await;
    }
    f.eval(t).await;
    let open = f.open().await;
    let escaped: Vec<_> = open.iter().filter(|x| x.kind == "escaped").collect();
    assert_eq!(escaped.len(), 1, "one rollup row");
    let r = escaped[0];
    assert_eq!((r.title.as_str(), r.severity), ("250 outside P1", Severity::Critical));
    assert_eq!(r.key, format!("escaped:herd:{}", f.herd));
    assert_eq!(r.targets.iter().filter(|(k, _)| k == "collar").count(), 250);
    assert_eq!(r.data["count"], 250);
    // Nothing goes before the critical window; then one text each.
    assert!(f.route(t + secs(5)).await.is_empty());
    let sent = f.route(t + secs(10)).await;
    assert_eq!(sent.len(), 2);
    assert_eq!(f.messages_to(&a).await.len(), 1);
    assert_eq!(f.messages_to(&b).await.len(), 1);
    assert_eq!(sent[0].text, "250 outside P1 since 06:58. Reply OK to ack");
    // Later evaluations and routing passes add nothing.
    f.eval(t + secs(20)).await;
    f.route(t + secs(22)).await;
    assert_eq!(f.messages().await.len(), 2);
}

#[tokio::test]
async fn a_trickle_of_six_in_a_minute_is_at_most_two_texts() {
    let f = Farm::new().await;
    let (a, _) = two_people(&f).await;
    let t = t0();
    let cs = f.collars(6, t).await;
    // One more crosses five minutes outside every ten seconds.
    for (i, c) in cs.iter().enumerate() {
        f.outside(c, t - mins(5) + secs(10 * i as i64), t + mins(5)).await;
    }
    let mut now = t;
    while now <= t + mins(4) {
        f.eval(now).await;
        f.route(now).await;
        now += secs(2);
    }
    let texts = f.messages_to(&a).await;
    assert!(!texts.is_empty() && texts.len() <= 2, "{texts:#?}");
    assert_eq!(f.open_kind("outside").await.len(), 1, "one rollup");
}

#[tokio::test]
async fn a_rollup_keeps_its_members_until_the_last_clears() {
    let f = Farm::new().await;
    let t = t0();
    let cs = f.collars(5, t).await;
    for c in &cs {
        f.outside(c, t - mins(6), t).await;
    }
    f.eval(t).await;
    let r = f.open_kind("outside").await;
    assert_eq!((r.len(), r[0].title.as_str()), (1, "5 outside P1"));
    for c in &cs[1..] {
        f.inside(c, t + mins(1)).await;
    }
    f.eval(t + mins(1)).await;
    let r2 = f.open_kind("outside").await;
    assert_eq!(r2.len(), 1, "never splits back into rows");
    assert_eq!((r2[0].id.as_str(), r2[0].title.as_str()), (r[0].id.as_str(), "1 outside P1"));
    f.inside(&cs[0], t + mins(2)).await;
    f.eval(t + mins(2)).await;
    f.eval(t + mins(4) + secs(1)).await;
    assert!(f.open_kind("outside").await.is_empty());
    assert_eq!(f.every_alert().await.iter().filter(|a| a.kind == "outside").count(), 1);
}

#[tokio::test]
async fn members_already_open_roll_into_the_new_rollup() {
    let f = Farm::new().await;
    let t = t0();
    let cs = f.collars(4, t).await;
    for c in &cs[..3] {
        f.outside(c, t - mins(6), t).await;
    }
    f.eval(t).await;
    let singles = f.open_kind("outside").await;
    assert_eq!(singles.len(), 3);
    f.outside(&cs[3], t - mins(6), t).await;
    f.eval(t + secs(10)).await;
    let open = f.open_kind("outside").await;
    assert_eq!((open.len(), open[0].title.as_str()), (1, "4 outside P1"));
    for s in singles {
        let s = f.alert(&s.id).await;
        assert_eq!((s.status, s.rolled_into.as_deref()), (AlertStatus::Resolved, Some(open[0].id.as_str())));
    }
}

#[tokio::test]
async fn one_row_per_key_and_events_only_on_change() {
    let f = Farm::new().await;
    let t = t0();
    let c = f.collar(Some("031"), t).await;
    f.set(&c, "battery = 0.14", &[]).await;
    let mut rx = f.ctx.subscribe();
    for i in 0..5 {
        f.eval(t + secs(10 * i)).await;
    }
    assert_eq!(f.every_alert().await.len(), 1);
    let mut events = 0;
    while let Ok(e) = rx.try_recv() {
        if matches!(e, Event::Alert { .. }) {
            events += 1;
        }
    }
    assert_eq!(events, 1, "opened once, unchanged since");
    f.set(&c, "battery = 0.12", &[]).await;
    f.eval(t + mins(1)).await;
    let a = &f.open().await[0];
    assert_eq!(a.title, "031 battery 12%");
    assert!(matches!(rx.try_recv(), Ok(Event::Alert { .. })));
}

#[tokio::test]
async fn a_rule_severity_can_be_raised() {
    let f = Farm::new().await;
    let t = t0();
    let c = f.collar(Some("214"), t).await;
    f.outside(&c, t - mins(6), t).await;
    f.rule("outside", json!({"severity": "critical"})).await;
    f.eval(t).await;
    assert_eq!(f.open_kind("outside").await[0].severity, Severity::Critical);
    // Turning a rule off clears its alerts.
    f.rule("outside", json!({"enabled": false})).await;
    f.eval(t + mins(3)).await;
    assert!(f.open_kind("outside").await.is_empty());
}

#[tokio::test]
async fn resolved_by_hand_stays_closed_until_it_comes_back() {
    let f = Farm::new().await;
    let t = t0();
    let c = f.collar(Some("214"), t).await;
    f.outside(&c, t - mins(6), t).await;
    f.eval(t).await;
    let a = f.open_kind("outside").await.remove(0);
    let hand = Identity { role: Role::Hand, user_id: None, name: Some("Sam".into()), via: Via::UserToken };
    let (s, v) = f.api(hand.clone(), "POST", &format!("/api/alerts/{}/resolve", a.id), None).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "resolved");
    assert_eq!(v["resolved_by"]["name"], "Sam");
    // Still outside: no new alert.
    for i in 1..6 {
        f.eval(t + mins(i)).await;
    }
    assert!(f.open_kind("outside").await.is_empty());
    // Back in for longer than clear_after_min, then out again: a new alert.
    f.inside(&c, t + mins(6)).await;
    f.eval(t + mins(6)).await;
    f.outside(&c, t + mins(3), t + mins(9)).await;
    f.eval(t + mins(9)).await;
    let again = f.open_kind("outside").await;
    assert_eq!(again.len(), 1);
    assert_ne!(again[0].id, a.id);
    // Resolving twice is a conflict; an unknown id is 404.
    let (s, _) = f.api(hand.clone(), "POST", &format!("/api/alerts/{}/resolve", a.id), None).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, _) = f.api(hand, "POST", "/api/alerts/alr_nope/ack", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ack_needs_a_hand_and_records_who() {
    let f = Farm::new().await;
    let t = t0();
    let c = f.collar(Some("214"), t).await;
    f.outside(&c, t - mins(6), t).await;
    f.eval(t).await;
    let a = f.open().await.remove(0);
    let viewer = Identity { role: Role::Viewer, user_id: None, name: None, via: Via::UserToken };
    let (s, _) = f.api(viewer.clone(), "POST", &format!("/api/alerts/{}/ack", a.id), None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = f.api(Identity::anonymous(), "POST", &format!("/api/alerts/{}/ack", a.id), None).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    let (s, v) = f.api(viewer, "GET", "/api/alerts", None).await;
    assert_eq!((s, v.as_array().unwrap().len()), (StatusCode::OK, 1));
    let hand = Identity { role: Role::Hand, user_id: Some("usr_x".into()), name: Some("Sam".into()), via: Via::Text };
    let (s, v) = f.api(hand.clone(), "POST", &format!("/api/alerts/{}/ack", a.id), None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((v["status"].as_str(), v["acked_by"]["via"].as_str(), v["acked_by"]["user_id"].as_str()), (Some("acked"), Some("text"), Some("usr_x")));
    // Acking again returns it as it is.
    let (s, v2) = f.api(hand, "POST", &format!("/api/alerts/{}/ack", a.id), None).await;
    assert_eq!((s, &v2["acked_at"]), (StatusCode::OK, &v["acked_at"]));
}

#[tokio::test]
async fn the_list_filters_by_status_herd_and_time() {
    let f = Farm::new().await;
    let t = t0();
    let cs = f.collars(2, t).await;
    f.set(&cs[0], "battery = 0.1", &[]).await;
    f.outside(&cs[1], t - mins(6), t).await;
    f.rule("outside", json!({"severity": "critical"})).await;
    f.eval(t).await;
    let open = f.open().await;
    let low = open.iter().find(|a| a.kind == "low_battery").unwrap();
    f.owner("POST", &format!("/api/alerts/{}/ack", low.id), None).await;
    let count = |v: serde_json::Value| v.as_array().unwrap().len();
    assert_eq!(count(f.owner("GET", "/api/alerts", None).await.1), 2);
    assert_eq!(count(f.owner("GET", "/api/alerts?status=open", None).await.1), 1);
    assert_eq!(count(f.owner("GET", "/api/alerts?status=acked", None).await.1), 1);
    assert_eq!(count(f.owner("GET", "/api/alerts?status=resolved", None).await.1), 0);
    assert_eq!(count(f.owner("GET", &format!("/api/alerts?herd_id={}", f.herd), None).await.1), 2);
    assert_eq!(count(f.owner("GET", "/api/alerts?herd_id=herd_other", None).await.1), 0);
    assert_eq!(count(f.owner("GET", &format!("/api/alerts?status=all&from={}", to_db(&(t + mins(1)))), None).await.1), 0);
    let (s, v) = f.owner("GET", "/api/alerts?status=sideways", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{v}");
    // Critical first among the unresolved.
    let (_, v) = f.owner("GET", "/api/alerts", None).await;
    assert_eq!(v[0]["kind"], "outside");
    let (s, one) = f.owner("GET", &format!("/api/alerts/{}", low.id), None).await;
    assert_eq!((s, one["kind"].as_str()), (StatusCode::OK, Some("low_battery")));
}

#[tokio::test]
async fn rules_and_policy_read_and_validate() {
    let f = Farm::new().await;
    let (s, v) = f.owner("GET", "/api/alerts/rules", None).await;
    assert_eq!(s, StatusCode::OK);
    let kinds: Vec<&str> = v["rules"].as_array().unwrap().iter().map(|r| r["kind"].as_str().unwrap()).collect();
    // A-engine's rules first, in Settings order; later streams' follow under their anchors.
    assert!(kinds.contains(&"schedule_not_stored"), "{kinds:?}");
    // @H
    assert!(kinds.contains(&"fit_check_due"), "{kinds:?}");
    assert_eq!(
        kinds[..11],
        [
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
            "gps_degraded"
        ]
    );
    let silent = v["rules"].as_array().unwrap().iter().find(|r| r["kind"] == "silent").unwrap();
    assert_eq!((silent["sentence"].as_str(), silent["after_min"].as_u64(), silent["unit"].as_str()), (Some("Collar silent for {n}"), Some(20), Some("min")));
    assert_eq!(v["policy"]["escalate_after_min"], 15);
    assert_eq!(v["policy"]["rollup_min"], 4);
    assert_eq!(v["configured"], json!([]));
    // A change keeps what it doesn't name.
    let (s, v) = f.owner("PUT", "/api/alerts/rules", Some(json!({"rules": {"silent": {"after_min": 1}}, "policy": {"start_grace_min": 0}}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let silent = v["rules"].as_array().unwrap().iter().find(|r| r["kind"] == "silent").unwrap();
    assert_eq!((silent["after_min"].as_u64(), silent["notify"].as_bool()), (Some(1), Some(true)));
    assert_eq!((v["policy"]["start_grace_min"].as_u64(), v["policy"]["escalate_after_min"].as_u64()), (Some(0), Some(15)));
    for bad in [
        json!({"rules": {"nope": {"enabled": false}}}),
        json!({"rules": {"silent": {"after_min": 0}}}),
        json!({"rules": {"low_battery": {"threshold": 150}}}),
        json!({"policy": {"herd_silent_share": 1.5}}),
        json!({"policy": {"quiet_start": "22:00"}}),
        json!({"policy": {"quiet_start": "25:00", "quiet_end": "06:00"}}),
    ] {
        let (s, v) = f.owner("PUT", "/api/alerts/rules", Some(bad.clone())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad} → {v}");
    }
    let hand = Identity { role: Role::Hand, user_id: None, name: None, via: Via::UserToken };
    let (s, _) = f.api(hand, "PUT", "/api/alerts/rules", Some(json!({"policy": {"rollup_min": 5}}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    // null puts a rule's number back to its default.
    let (_, v) = f.owner("PUT", "/api/alerts/rules", Some(json!({"rules": {"silent": {"after_min": null}}}))).await;
    let silent = v["rules"].as_array().unwrap().iter().find(|r| r["kind"] == "silent").unwrap();
    assert_eq!(silent["after_min"].as_u64(), Some(20));
}

#[tokio::test]
async fn mcp_tools_list_ack_and_resolve_through_the_registry() {
    let f = Farm::new().await;
    let t = t0();
    let c = f.collar(Some("214"), t).await;
    f.outside(&c, t - mins(6), t).await;
    f.eval(t).await;
    let reg = f.ctx.tools();
    for n in ["list_alerts", "ack_alert", "resolve_alert"] {
        assert!(reg.has(n), "{n}");
    }
    let viewer = Identity { role: Role::Viewer, user_id: None, name: None, via: Via::UserToken };
    let listed: Vec<&str> = reg.listed_for(&viewer, &ToolScope::Full).iter().map(|s| s.name).filter(|n| n.contains("alert")).collect();
    assert_eq!(listed, ["list_alerts"]);
    assert!(!reg.brain_tools().iter().any(|n| n.contains("alert")), "not offered to the decision brain");
    let v = reg.call(&f.ctx, "list_alerts", json!({}), None, viewer.clone(), &ToolScope::Full).await.unwrap();
    let id = v["alerts"][0]["id"].as_str().unwrap().to_owned();
    let e = reg.call(&f.ctx, "ack_alert", json!({"id": id}), None, viewer, &ToolScope::Full).await.unwrap_err();
    assert_eq!(e.status, StatusCode::FORBIDDEN);
    let hand = Identity { role: Role::Hand, user_id: None, name: Some("Sam".into()), via: Via::UserToken };
    let v = reg.call(&f.ctx, "ack_alert", json!({"id": id}), None, hand.clone(), &ToolScope::Full).await.unwrap();
    assert_eq!(v["status"], "acked");
    let v = reg.call(&f.ctx, "resolve_alert", json!({"id": id}), None, hand, &ToolScope::Full).await.unwrap();
    assert_eq!(v["status"], "resolved");
    let v = reg.call(&f.ctx, "list_alerts", json!({"status": "resolved"}), None, Identity::system(), &ToolScope::Full).await.unwrap();
    assert_eq!(v["alerts"].as_array().unwrap().len(), 1);
}

fn fix_event(collar: &str, herd: &str) -> Event {
    Event::Fix {
        collar_id: collar.into(),
        animal_id: None,
        herd_id: herd.into(),
        fix: op_core::Fix { at: t0(), point: [-93.62, 42.03], accuracy_m: 3.0, sats: 9, cn0: None, ttf_s: None },
        state: op_core::FenceState::Inside,
    }
}

#[tokio::test]
async fn wake_ups_are_debounced_and_never_come_from_fixes() {
    let f = Farm::new().await;
    let c = f.collar(Some("1"), t0()).await;
    op_alerts::engine::start(f.ctx.clone());
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    for _ in 0..300 {
        f.ctx.publish(fix_event(&c, &f.herd));
        f.ctx.publish(Event::Collar { collar: op_core::Collar { id: c.clone(), herd_id: f.herd.clone(), ..Default::default() } });
    }
    tokio::time::sleep(std::time::Duration::from_millis(3200)).await;
    assert_eq!(op_alerts::engine::woken_runs(&f.ctx).runs, 0, "fixes and collar events wake nothing");
    let escape = op_core::Escape {
        id: "esc_1".into(),
        herd_id: f.herd.clone(),
        collar_id: c.clone(),
        status: op_core::EscapeStatus::Returning,
        geometry: None,
        version: None,
        step: 1,
        remaining_m: 10.0,
        started_at: t0(),
        updated_at: t0(),
        ended_at: None,
    };
    for _ in 0..5 {
        f.ctx.publish(Event::Escape { escape: escape.clone() });
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
    assert_eq!(op_alerts::engine::woken_runs(&f.ctx).runs, 0, "waits for two quiet seconds");
    let start = Instant::now();
    while op_alerts::engine::woken_runs(&f.ctx).runs == 0 && start.elapsed().as_secs() < 8 {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    let w = op_alerts::engine::woken_runs(&f.ctx);
    assert_eq!(w.runs, 1, "one evaluation for the burst");
    assert_eq!(w.last_kinds, ["escaped", "outside"]);
    f.ctx.shutdown();
}

/// 250 collars reporting every minute with a fix every 5 s for 4 hours:
/// 720,000 fixes and 60,000 health rows, written by SQLite itself.
async fn fleet_fixture(f: &Farm) -> chrono::DateTime<chrono::Utc> {
    let t = t0();
    let cs = f.collars(250, t).await;
    let start = (t - chrono::Duration::hours(4)).timestamp_millis();
    sqlx::query(
        "WITH RECURSIVE seq(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM seq WHERE i < 2879)
         INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m, sats)
         SELECT c.id, c.herd_id, strftime('%Y-%m-%dT%H:%M:%fZ', (?1 + i * 5000) / 1000.0, 'unixepoch'), ?1 + i * 5000,
                CASE WHEN c.id <= 'col_0005' THEN -93.6230 ELSE -93.6245 + (abs(random()) % 1000) / 250000.0 END,
                CASE WHEN c.id <= 'col_0005' THEN 42.0320 ELSE 42.0305 + (abs(random()) % 1000) / 400000.0 END,
                3.0, 9
         FROM seq, collars c",
    )
    .bind(start)
    .execute(f.ctx.db())
    .await
    .unwrap();
    sqlx::query(
        "WITH RECURSIVE seq(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM seq WHERE i < 239)
         INSERT INTO health (collar_id, herd_id, at, t, battery, fixes)
         SELECT c.id, c.herd_id, strftime('%Y-%m-%dT%H:%M:%fZ', (?1 + i * 60000) / 1000.0, 'unixepoch'), ?1 + i * 60000, 0.8, 12
         FROM seq, collars c",
    )
    .bind(start)
    .execute(f.ctx.db())
    .await
    .unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fixes").fetch_one(f.ctx.db()).await.unwrap();
    assert_eq!(n, 720_000);
    // Something for every rule to find.
    for c in &cs[10..40] {
        f.outside(c, t - mins(8), t).await;
    }
    for c in &cs[40..50] {
        f.escape(c, "returning", t - mins(3), None).await;
    }
    for c in &cs[50..60] {
        f.seen(c, t - mins(45)).await;
    }
    f.boundary(3, t - mins(30), None).await;
    for c in &cs {
        f.holds(c, 3, "applied").await;
    }
    for c in &cs[60..70] {
        f.holds(c, 2, "applied").await;
    }
    f.decision("MOVE", "proposed", t - mins(40), None).await;
    f.movement("sweeping", t - mins(60), Some(t - mins(20)), &[&cs[70], &cs[71]]).await;
    t
}

#[tokio::test]
async fn ten_second_rules_take_under_200_ms_and_drop_off_under_a_second_at_250_collars() {
    let f = Farm::new().await;
    let t = fleet_fixture(&f).await;
    let mut e = op_alerts::engine::Engine::new();
    let ten: HashSet<&str> = e.descriptors().into_iter().filter(|d| d.cadence_s == 10).map(|d| d.kind).collect();
    assert_eq!(ten.len(), 8);
    // First pass reads every collar's usual report interval; later passes use it.
    let cold = Instant::now();
    e.evaluate(&f.ctx, t, &ten).await.unwrap();
    let cold = cold.elapsed();
    let mut worst = std::time::Duration::ZERO;
    for i in 1..=5 {
        let s = Instant::now();
        e.evaluate(&f.ctx, t + secs(10 * i), &ten).await.unwrap();
        worst = worst.max(s.elapsed());
    }
    eprintln!("10 s rules: cold {cold:?}, warm worst {worst:?}");
    assert!(worst.as_millis() < 200, "10 s rules took {worst:?}");
    let kinds: HashSet<String> = f.open().await.into_iter().map(|a| a.kind).collect();
    for k in ["escaped", "outside", "silent", "boundary_not_applied", "decision_waiting", "move_stalled", "stragglers"] {
        assert!(kinds.contains(k), "{k} found nothing: {kinds:?}");
    }
    let drop: HashSet<&str> = ["drop_off"].into();
    let s = Instant::now();
    e.evaluate(&f.ctx, t + mins(1), &drop).await.unwrap();
    let took = s.elapsed();
    eprintln!("drop_off: {took:?}");
    assert!(took.as_millis() < 1000, "drop_off took {took:?}");
    let dropped = f.open_kind("drop_off").await;
    assert_eq!((dropped.len(), dropped[0].data["count"].as_u64()), (1, Some(5)), "the five collars that never moved, as one rollup");
}

/// These collars out on an open escape at `at`.
async fn break_out(f: &Farm, cs: &[String], at: chrono::DateTime<chrono::Utc>) {
    for c in cs {
        f.outside(c, at - mins(2), at).await;
        f.escape(c, "returning", at - mins(1), None).await;
    }
}

/// These collars' escapes end: they're back in.
async fn back_in(f: &Farm, cs: &[String], at: chrono::DateTime<chrono::Utc>) {
    for c in cs {
        f.exec(&format!("UPDATE escapes SET status = 'ended', ended_at = '{}' WHERE collar_id = '{c}'", to_db(&at))).await;
        f.inside(c, at).await;
    }
}

/// Evaluate and route every `step` from `from` to `to`, every collar reporting.
async fn run(f: &Farm, from: chrono::DateTime<chrono::Utc>, to: chrono::DateTime<chrono::Utc>, step: chrono::Duration) {
    let mut now = from;
    while now <= to {
        f.exec(&format!("UPDATE collars SET last_seen = '{}'", to_db(&now))).await;
        f.eval(now).await;
        f.route(now).await;
        now += step;
    }
}

#[tokio::test]
async fn a_new_breakout_joining_an_acked_rollup_is_texted_again() {
    let f = Farm::new().await;
    let (a, b) = two_people(&f).await;
    let t = t0();
    let cs = f.collars(250, t).await;
    break_out(&f, &cs[..6], t).await;
    run(&f, t, t + secs(10), secs(10)).await;
    assert_eq!(f.messages_to(&a).await.len(), 1);
    let first = f.open_kind("escaped").await.remove(0);
    let (s, _) = f.owner("POST", &format!("/api/alerts/{}/ack", first.id), None).await;
    assert_eq!(s, StatusCode::OK);
    // Five come back; 101 stays out (its collar fell off outside).
    back_in(&f, &cs[1..6], t + mins(1)).await;
    run(&f, t + mins(1), t + mins(60), mins(1)).await;
    let still = f.open_kind("escaped").await;
    assert_eq!((still.len(), still[0].id.as_str(), still[0].title.as_str(), still[0].status), (1, first.id.as_str(), "1 outside P1", AlertStatus::Acked));
    assert_eq!(f.messages_to(&a).await.len(), 1);
    // 240 more get out: a new alert for all 241, texted to everyone.
    let later = t + mins(120);
    break_out(&f, &cs[6..246], later).await;
    run(&f, later, later + mins(1), secs(10)).await;
    let open = f.open_kind("escaped").await;
    assert_eq!(open.len(), 1);
    assert_ne!(open[0].id, first.id);
    assert_eq!((open[0].title.as_str(), open[0].status, open[0].data["count"].as_u64()), ("241 outside P1", AlertStatus::Open, Some(241)));
    let old = f.alert(&first.id).await;
    assert_eq!((old.status, old.rolled_into.as_deref(), old.resolved_by), (AlertStatus::Resolved, Some(open[0].id.as_str()), None));
    for p in [&a, &b] {
        let m = f.messages_to(p).await;
        assert_eq!(m.len(), 2, "{m:#?}");
        assert!(m[1].text.starts_with("241 outside P1 since "), "{}", m[1].text);
        assert_eq!(m[1].alert_id.as_deref(), Some(open[0].id.as_str()));
    }
    assert_eq!(f.messages_to(&a).await.len(), 2);
    // It's unacked: it re-notifies like any other critical alert.
    run(&f, later + mins(2), later + mins(31), mins(1)).await;
    assert_eq!(f.messages_to(&a).await.len(), 3, "renotified after 30 minutes");
}

#[tokio::test]
async fn a_new_breakout_joining_a_rollup_closed_by_hand_is_texted_again() {
    let f = Farm::new().await;
    let (a, _) = two_people(&f).await;
    let t = t0();
    let cs = f.collars(250, t).await;
    break_out(&f, &cs[..6], t).await;
    f.eval(t).await;
    // Closed in the app before its text went out.
    let first = f.open_kind("escaped").await.remove(0);
    let (s, _) = f.owner("POST", &format!("/api/alerts/{}/resolve", first.id), None).await;
    assert_eq!(s, StatusCode::OK);
    // They stay out, then three come back: still closed, and no alerts of their own.
    run(&f, t + secs(10), t + mins(30), mins(1)).await;
    back_in(&f, &cs[..3], t + mins(31)).await;
    run(&f, t + mins(31), t + mins(60), mins(1)).await;
    assert!(f.open_kind("escaped").await.is_empty(), "{:#?}", f.open().await);
    assert_eq!(f.every_alert().await.iter().filter(|x| x.kind == "escaped").count(), 1);
    assert!(f.messages_to(&a).await.is_empty());
    // 240 more get out.
    let later = t + mins(120);
    break_out(&f, &cs[6..246], later).await;
    run(&f, later, later + mins(1), secs(10)).await;
    let open = f.open_kind("escaped").await;
    assert_eq!((open.len(), open[0].title.as_str()), (1, "243 outside P1"));
    let m = f.messages_to(&a).await;
    assert_eq!(m.len(), 1, "{m:#?}");
    assert!(m[0].text.starts_with("243 outside P1"), "{}", m[0].text);
}

#[tokio::test]
async fn a_straggler_left_in_an_acked_rollup_doesnt_hide_the_next_escape() {
    let f = Farm::new().await;
    let (a, _) = two_people(&f).await;
    let t = t0();
    let cs = f.collars(20, t).await;
    break_out(&f, &cs[..6], t).await;
    run(&f, t, t + secs(10), secs(10)).await;
    let first = f.open_kind("escaped").await.remove(0);
    let (s, _) = f.owner("POST", &format!("/api/alerts/{}/ack", first.id), None).await;
    assert_eq!(s, StatusCode::OK);
    // Five come back; one collar lies outside the fence.
    back_in(&f, &cs[1..6], t + mins(1)).await;
    run(&f, t + mins(1), t + mins(60), mins(1)).await;
    assert_eq!(f.messages_to(&a).await.len(), 1);
    // Hours later two more get out: as many new as the one it still had, so texted.
    let later = t + mins(180);
    break_out(&f, &cs[6..8], later).await;
    run(&f, later, later + mins(1), secs(10)).await;
    let open = f.open_kind("escaped").await;
    assert_eq!((open.len(), open[0].title.as_str(), open[0].status), (1, "3 outside P1", AlertStatus::Open));
    let m = f.messages_to(&a).await;
    assert_eq!(m.len(), 2, "{m:#?}");
    assert!(m[1].text.starts_with("3 outside P1"), "{}", m[1].text);
}

#[tokio::test]
async fn escapes_after_a_rollup_was_closed_by_hand_alert_on_their_own() {
    let f = Farm::new().await;
    let (a, _) = two_people(&f).await;
    let t = t0();
    let cs = f.collars(20, t).await;
    break_out(&f, &cs[..6], t).await;
    run(&f, t, t + secs(10), secs(10)).await;
    assert_eq!(f.messages_to(&a).await.len(), 1);
    let first = f.open_kind("escaped").await.remove(0);
    let (s, _) = f.owner("POST", &format!("/api/alerts/{}/resolve", first.id), None).await;
    assert_eq!(s, StatusCode::OK);
    // The six stay out, closed; later two more get out: those two are alerts of their own.
    run(&f, t + mins(1), t + mins(30), mins(1)).await;
    let later = t + mins(31);
    break_out(&f, &cs[6..8], later).await;
    run(&f, later, later + mins(1), secs(10)).await;
    let open = f.open_kind("escaped").await;
    assert_eq!(open.len(), 2, "{open:#?}");
    assert!(open.iter().all(|x| x.key != first.key && x.data.get("count").is_none()), "{open:#?}");
    assert_eq!(f.alert(&first.id).await.status, AlertStatus::Resolved);
    let m = f.messages_to(&a).await;
    assert_eq!(m.len(), 2, "{m:#?}");
    assert!(m[1].text.starts_with("2 outside P1: "), "{}", m[1].text);
    // Two more make four new since it was closed: one new rollup of all ten.
    break_out(&f, &cs[8..10], later + mins(2)).await;
    run(&f, later + mins(2), later + mins(3), secs(10)).await;
    let open = f.open_kind("escaped").await;
    assert_eq!(open.len(), 1, "{open:#?}");
    assert_eq!((open[0].title.as_str(), open[0].key.as_str()), ("10 outside P1", first.key.as_str()));
    assert_ne!(open[0].id, first.id);
    let m = f.messages_to(&a).await;
    assert_eq!(m.len(), 3, "{m:#?}");
    assert!(m[2].text.starts_with("10 outside P1"), "{}", m[2].text);
}

#[tokio::test]
async fn a_growing_breakout_is_texted_again_each_time_it_doubles() {
    let f = Farm::new().await;
    let (a, _) = two_people(&f).await;
    let t = t0();
    let cs = f.collars(40, t).await;
    break_out(&f, &cs[..6], t).await;
    run(&f, t, t + secs(10), secs(10)).await;
    assert_eq!(f.messages_to(&a).await.len(), 1);
    // Five more: not yet as many new as it had.
    break_out(&f, &cs[6..11], t + mins(1)).await;
    run(&f, t + mins(1), t + mins(2), secs(10)).await;
    assert_eq!(f.messages_to(&a).await.len(), 1);
    assert_eq!(f.open_kind("escaped").await[0].title, "11 outside P1");
    // One more makes six new: texted again, one row open.
    break_out(&f, &cs[11..12], t + mins(3)).await;
    run(&f, t + mins(3), t + mins(4), secs(10)).await;
    let m = f.messages_to(&a).await;
    assert_eq!(m.len(), 2, "{m:#?}");
    assert!(m[1].text.starts_with("12 outside P1"), "{}", m[1].text);
    assert_eq!(f.open_kind("escaped").await.len(), 1);
}

#[tokio::test]
async fn a_slow_rule_waits_two_of_its_own_runs_before_it_clears() {
    let f = Farm::new().await;
    let (a, _) = two_people(&f).await;
    let t = t0();
    let c = f.collar(Some("214"), t).await;
    let base = f.spot(100.0, 100.0);
    let last_fix = |at: chrono::DateTime<chrono::Utc>| json!({"at": to_db(&at), "point": base, "accuracy_m": 3.0, "sats": 9}).to_string();
    for i in 0..245 {
        f.fix(&c, t - mins(245) + mins(i), base, 3.0).await;
    }
    f.set(&c, "last_fix = ?", &[&last_fix(t - mins(1))]).await;
    let drop: HashSet<&str> = ["drop_off"].into();
    let mut e = op_alerts::engine::Engine::new();
    e.evaluate(&f.ctx, t, &drop).await.unwrap();
    let first = f.open_kind("drop_off").await;
    assert_eq!(first.len(), 1);
    f.route(t + secs(61)).await;
    assert_eq!(f.messages_to(&a).await.len(), 1);
    // No fix in its last five minutes: one run (300 s later) misses it.
    assert!(!e.due(t + mins(4)).contains("drop_off"));
    e.evaluate(&f.ctx, t + mins(5), &drop).await.unwrap();
    assert_eq!(f.open_kind("drop_off").await.len(), 1, "one missed run of a 300 s rule doesn't clear it");
    // Fixes again: the same row, and no second text.
    for i in 5..10 {
        f.fix(&c, t + mins(i), base, 3.0).await;
    }
    f.set(&c, "last_fix = ?", &[&last_fix(t + mins(9))]).await;
    e.evaluate(&f.ctx, t + mins(10), &drop).await.unwrap();
    f.route(t + mins(11)).await;
    let again = f.open_kind("drop_off").await;
    assert_eq!((again.len(), again[0].id.as_str()), (1, first[0].id.as_str()));
    assert_eq!(f.messages_to(&a).await.len(), 1);
    // Two runs in a row without it: resolved by itself.
    e.evaluate(&f.ctx, t + mins(15), &drop).await.unwrap();
    assert_eq!(f.open_kind("drop_off").await.len(), 1);
    e.evaluate(&f.ctx, t + mins(20), &drop).await.unwrap();
    assert!(f.open_kind("drop_off").await.is_empty());
    assert!(f.alert(&first[0].id).await.resolved_by.is_none());
}

#[tokio::test]
async fn a_slow_rule_closed_by_hand_stays_closed_between_its_runs() {
    let f = Farm::new().await;
    let (a, _) = two_people(&f).await;
    let t = t0();
    let c = f.collar(Some("214"), t).await;
    let base = f.spot(100.0, 100.0);
    for i in 0..300 {
        f.fix(&c, t - mins(245) + mins(i), base, 3.0).await;
    }
    let drop: HashSet<&str> = ["drop_off"].into();
    let mut e = op_alerts::engine::Engine::new();
    let fix_at = |at: chrono::DateTime<chrono::Utc>| json!({"at": to_db(&(at - mins(1))), "point": base, "accuracy_m": 3.0, "sats": 9}).to_string();
    f.set(&c, "last_fix = ?", &[&fix_at(t)]).await;
    e.evaluate(&f.ctx, t, &drop).await.unwrap();
    f.route(t + secs(61)).await;
    assert_eq!(f.messages_to(&a).await.len(), 1);
    let first = f.open_kind("drop_off").await.remove(0);
    let (s, _) = f.owner("POST", &format!("/api/alerts/{}/resolve", first.id), None).await;
    assert_eq!(s, StatusCode::OK);
    // Still lying there, run after run 300 s apart: it stays closed.
    for k in 1..=6 {
        let now = t + mins(5 * k);
        f.set(&c, "last_fix = ?", &[&fix_at(now)]).await;
        e.evaluate(&f.ctx, now, &drop).await.unwrap();
        f.route(now + secs(61)).await;
    }
    assert!(f.open_kind("drop_off").await.is_empty(), "{:#?}", f.open().await);
    assert_eq!(f.messages_to(&a).await.len(), 1);
}

#[tokio::test]
async fn approval_codes_stay_out_of_the_api_and_live_events() {
    let f = Farm::new().await;
    let mut rx = f.ctx.subscribe();
    f.decision("MOVE", "proposed", t0() - mins(31), None).await;
    f.eval(t0()).await;
    let a = f.open_kind("decision_waiting").await.remove(0);
    assert!(a.data["code"].as_str().is_some_and(|c| c.len() == 4), "kept for the text and the replies");
    let mut seen = 0;
    while let Ok(e) = rx.try_recv() {
        if let Event::Alert { alert } = e {
            assert!(alert.data.get("code").is_none(), "live: {:?}", alert.data);
            assert_eq!(alert.data["herd"], "Cows");
            seen += 1;
        }
    }
    assert_eq!(seen, 1);
    let viewer = Identity { role: Role::Viewer, user_id: None, name: None, via: Via::UserToken };
    for id in [viewer.clone(), Identity::owner(Via::Local)] {
        let (_, v) = f.api(id.clone(), "GET", "/api/alerts", None).await;
        assert!(v[0]["data"].get("code").is_none(), "{v}");
        assert_eq!(v[0]["data"]["herd"], "Cows");
        let (_, v) = f.api(id.clone(), "GET", &format!("/api/alerts/{}", a.id), None).await;
        assert!(v["data"].get("code").is_none(), "{v}");
        let v = f.ctx.tools().call(&f.ctx, "list_alerts", json!({}), None, id, &ToolScope::Full).await.unwrap();
        assert!(v["alerts"][0]["data"].get("code").is_none(), "{v}");
    }
    let (s, v) = f.owner("POST", &format!("/api/alerts/{}/ack", a.id), None).await;
    assert_eq!(s, StatusCode::OK);
    assert!(v["data"].get("code").is_none(), "{v}");
    assert!(rx.try_recv().is_ok_and(|e| matches!(e, Event::Alert { alert } if alert.data.get("code").is_none())));
    // The texts still carry it.
    assert_eq!(f.alert(&a.id).await.data["code"], a.data["code"]);
}
