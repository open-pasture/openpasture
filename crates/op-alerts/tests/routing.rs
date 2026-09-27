//! Routing: who gets which alert, on which channel, when; on duty,
//! escalation, re-notification, quiet hours, grouping, the webhook, prefs
//! and the brief's attention line.

mod a_engine_fixture;

use a_engine_fixture::*;
use axum::http::StatusCode;
use op_core::{Identity, Role, Via};
use serde_json::json;

/// An escaped animal (critical) opened at `t0`.
async fn escaped(f: &Farm) -> String {
    let c = f.collar(Some("214"), t0()).await;
    f.outside(&c, t0() - mins(2), t0()).await;
    f.escape(&c, "returning", t0() - mins(1), None).await;
    f.eval(t0()).await;
    f.open_kind("escaped").await[0].id.clone()
}

/// An animal outside (warning) opened at `at`.
async fn outside(f: &Farm, tag: &str, at: chrono::DateTime<chrono::Utc>) -> String {
    let c = f.collar(Some(tag), at).await;
    f.outside(&c, at - mins(6), at + mins(600)).await;
    f.eval(at).await;
    f.open_kind("outside").await.into_iter().find(|a| a.data["label"] == tag).unwrap().id
}

async fn tiers(f: &Farm, alert: &str) -> Vec<(String, i64)> {
    sqlx::query_as("SELECT user_id, tier FROM alert_notifications WHERE alert_id = ? AND user_id IS NOT NULL ORDER BY sent_at, user_id")
        .bind(alert)
        .fetch_all(f.ctx.db())
        .await
        .unwrap()
}

#[tokio::test]
async fn texts_go_only_to_verified_phones_over_configured_channels() {
    let f = Farm::new().await;
    f.sms().await;
    let ok = f.person("Cody", Role::Owner, Some("515 555 0101"), true, None).await;
    let unverified = f.person("Sam", Role::Hand, Some("+15155550102"), false, None).await;
    let emailer = f.person("Jo", Role::Manager, None, false, Some("jo@example.com")).await;
    f.prefs(&emailer, json!({"channels": ["email"]})).await;
    escaped(&f).await;
    f.route(t0() + secs(10)).await;
    let all = f.messages().await;
    assert_eq!(all.len(), 1, "{all:#?}");
    assert_eq!((all[0].channel.as_str(), all[0].address.as_str(), all[0].user_id.as_deref()), ("sms", "+15155550101", Some(ok.as_str())));
    assert_eq!((all[0].kind.as_str(), all[0].status.as_str()), ("alert", "queued"));
    assert!(f.messages_to(&unverified).await.is_empty());
    assert!(f.messages_to(&emailer).await.is_empty(), "no email channel set up");

    // With SMTP too, the email person gets it, with a subject.
    let f = Farm::new().await;
    f.channels(json!({"sms": {"from": "+15155550100"}, "email": {"host": "smtp.example.com", "from": "farm@example.com"}})).await;
    let emailer = f.person("Jo", Role::Manager, None, false, Some("jo@example.com")).await;
    f.prefs(&emailer, json!({"channels": ["email", "sms"]})).await;
    escaped(&f).await;
    f.route(t0() + secs(10)).await;
    let m = f.messages_to(&emailer).await;
    assert_eq!(m.len(), 1);
    assert_eq!((m[0].channel.as_str(), m[0].subject.as_deref()), ("email", Some("214 outside P1")));
}

#[tokio::test]
async fn the_relay_carries_sms_and_email_when_the_farm_has_neither() {
    let f = Farm::new().await;
    f.ctx.store().set_setting_json(op_core::notify_config::CHANNELS_KEY, &json!({"relay": {"enabled": true}})).await.unwrap();
    f.ctx.secrets().set("hosted_api_key", "oph_test").unwrap();
    let a = f.person("Cody", Role::Owner, Some("+15155550101"), true, Some("cody@example.com")).await;
    f.prefs(&a, json!({"channels": ["sms", "email", "whatsapp"]})).await;
    escaped(&f).await;
    f.route(t0() + secs(10)).await;
    let m = f.messages_to(&a).await;
    let got: Vec<(&str, &str)> = m.iter().map(|x| (x.channel.as_str(), x.address.as_str())).collect();
    assert_eq!(got, [("relay", "+15155550101"), ("relay", "cody@example.com")], "no WhatsApp without the farm's own");
    assert_eq!(m[0].subject, None);
    assert!(m[1].subject.is_some());
}

#[tokio::test]
async fn prefs_filter_by_severity_herd_kind_and_opt_out() {
    let f = Farm::new().await;
    f.sms().await;
    let critical_only = f.person("A", Role::Hand, Some("+15155550101"), true, None).await;
    f.prefs(&critical_only, json!({"min_severity": "critical"})).await;
    let other_herd = f.person("B", Role::Hand, Some("+15155550102"), true, None).await;
    let (_, h2) = f.core("POST", "/api/herds", Some(json!({"name": "Heifers", "species": "cattle", "count": 10}))).await;
    f.prefs(&other_herd, json!({"herds": [h2["id"]]})).await;
    let muted = f.person("C", Role::Hand, Some("+15155550103"), true, None).await;
    f.prefs(&muted, json!({"muted_kinds": ["outside"]})).await;
    let stopped = f.person("D", Role::Hand, Some("+15155550104"), true, None).await;
    op_alerts::routing::prefs::set_sms_opt_out(&f.ctx, &stopped, true).await.unwrap();
    let everyone = f.person("E", Role::Hand, Some("+15155550105"), true, None).await;
    let disabled = f.person("F", Role::Hand, Some("+15155550106"), true, None).await;
    op_core::users::update_user(&f.ctx, &disabled, op_core::users::UserPatch { disabled: Some(true), ..Default::default() }).await.unwrap();
    outside(&f, "214", t0()).await;
    f.route(t0() + secs(59)).await;
    assert!(f.messages().await.is_empty(), "a warning waits the grouping window");
    f.route(t0() + secs(60)).await;
    let to: Vec<String> = f.messages().await.into_iter().filter_map(|m| m.user_id).collect();
    assert_eq!(to, vec![everyone.clone()]);
    // The critical-only person gets a critical one.
    escaped(&f).await;
    f.route(t0() + secs(70)).await;
    assert_eq!(f.messages_to(&critical_only).await.len(), 1);
    assert_eq!(f.messages_to(&muted).await.len(), 1, "muted outside, not escaped");
    assert!(f.messages_to(&stopped).await.is_empty() && f.messages_to(&other_herd).await.is_empty() && f.messages_to(&disabled).await.is_empty());
}

#[tokio::test]
async fn on_duty_people_get_the_first_send_and_escalation_climbs_the_roles() {
    let f = Farm::new().await;
    f.sms().await;
    f.policy(json!({"renotify_max": 0})).await;
    let duty = f.person("On", Role::Hand, Some("+15155550101"), true, None).await;
    f.prefs(&duty, json!({"on_duty": true})).await;
    let off = f.person("Off", Role::Hand, Some("+15155550102"), true, None).await;
    let manager = f.person("Mgr", Role::Manager, Some("+15155550103"), true, None).await;
    let owner = f.person("Own", Role::Owner, Some("+15155550104"), true, None).await;
    let id = escaped(&f).await;
    f.route(t0() + secs(10)).await;
    assert_eq!(tiers(&f, &id).await, vec![(duty.clone(), 0)]);
    f.route(t0() + mins(10)).await;
    assert_eq!(tiers(&f, &id).await.len(), 1, "not before 15 minutes");
    f.route(t0() + mins(15) + secs(10)).await;
    assert_eq!(tiers(&f, &id).await, vec![(duty.clone(), 0), (manager.clone(), 1)]);
    f.route(t0() + mins(30) + secs(10)).await;
    assert_eq!(tiers(&f, &id).await, vec![(duty.clone(), 0), (manager.clone(), 1), (owner.clone(), 2)]);
    f.route(t0() + mins(45) + secs(10)).await;
    f.route(t0() + mins(60) + secs(10)).await;
    assert_eq!(tiers(&f, &id).await.len(), 3, "stops at the owner");
    assert!(f.messages_to(&off).await.is_empty(), "the hand off duty isn't next up");
    assert_eq!(f.messages_to(&manager).await.len(), 1);
}

#[tokio::test]
async fn unacked_critical_alerts_renotify_three_times_and_ack_stops_it() {
    let f = Farm::new().await;
    f.sms().await;
    let p = f.person("Cody", Role::Owner, Some("+15155550101"), true, None).await;
    let id = escaped(&f).await;
    f.route(t0() + secs(10)).await;
    f.route(t0() + mins(20)).await;
    assert_eq!(f.messages_to(&p).await.len(), 1);
    for k in 1..=4 {
        f.route(t0() + mins(30 * k) + secs(10 + k)).await;
    }
    assert_eq!(f.messages_to(&p).await.len(), 4, "first send and three more");

    let g = Farm::new().await;
    g.sms().await;
    let p = g.person("Cody", Role::Owner, Some("+15155550101"), true, None).await;
    let id2 = escaped(&g).await;
    g.route(t0() + secs(10)).await;
    op_alerts::engine::store::ack(&g.ctx, &id2, &Identity::owner(Via::Local).actor(), t0() + mins(1)).await.unwrap();
    g.route(t0() + mins(31)).await;
    g.route(t0() + mins(61)).await;
    assert_eq!(g.messages_to(&p).await.len(), 1, "acked: no more");
    let _ = id;
}

#[tokio::test]
async fn quiet_hours_hold_warnings_and_pass_critical() {
    let f = Farm::new().await;
    f.sms().await;
    f.policy(json!({"quiet_start": "22:00", "quiet_end": "06:00", "renotify_max": 0})).await;
    let farm_quiet = f.person("A", Role::Owner, Some("+15155550101"), true, None).await;
    let strict = f.person("B", Role::Manager, Some("+15155550102"), true, None).await;
    f.prefs(&strict, json!({"critical_in_quiet": false})).await;
    let own_hours = f.person("C", Role::Hand, Some("+15155550103"), true, None).await;
    f.prefs(&own_hours, json!({"quiet_start": "01:00", "quiet_end": "02:00"})).await;
    // 23:00 farm time.
    let night = t("2026-09-28T04:00:00.000Z");
    let stays = outside(&f, "214", night).await;
    let goes = outside(&f, "215", night).await;
    f.route(night + secs(60)).await;
    let first: Vec<String> = f.messages().await.into_iter().filter_map(|m| m.user_id).collect();
    assert_eq!(first, vec![own_hours.clone()], "only the person outside their own quiet hours");
    // A critical one passes quiet hours, unless someone turned that off.
    escaped(&f).await; // opened at t0, routed now
    f.route(night + secs(70)).await;
    assert_eq!(f.messages_to(&farm_quiet).await.len(), 1);
    assert!(f.messages_to(&strict).await.is_empty());
    // One warning clears in the night; at 06:00 the other goes out.
    op_alerts::engine::store::resolve(&f.ctx, &goes, None, None, night + mins(30)).await.unwrap();
    f.route(night + mins(60)).await;
    assert_eq!(f.messages_to(&farm_quiet).await.len(), 1);
    let morning = t("2026-09-28T11:00:00.000Z");
    f.route(morning).await;
    let a = f.messages_to(&farm_quiet).await;
    assert_eq!(a.len(), 2);
    assert!(a[1].text.starts_with("214 outside P1"), "{}", a[1].text);
    assert_eq!(a[1].alert_id.as_deref(), Some(stays.as_str()));
    // The strict person gets the held critical one then too.
    assert_eq!(f.messages_to(&strict).await.len(), 2);
}

#[tokio::test]
async fn warnings_in_one_window_are_one_text() {
    let f = Farm::new().await;
    f.sms().await;
    let p = f.person("Cody", Role::Owner, Some("+15155550101"), true, None).await;
    f.policy(json!({"rollup_min": 10})).await;
    let t = t0();
    let a = outside(&f, "214", t).await;
    let b = outside(&f, "031", t + secs(20)).await;
    let c = outside(&f, "118", t + secs(40)).await;
    let late = outside(&f, "007", t + secs(70)).await;
    f.route(t + secs(60)).await;
    let m = f.messages_to(&p).await;
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].text, "3 outside P1: 214 031 118");
    let rows: Vec<(String,)> =
        sqlx::query_as("SELECT alert_id FROM alert_notifications WHERE message_id = ? ORDER BY alert_id").bind(&m[0].id).fetch_all(f.ctx.db()).await.unwrap();
    let mut want = vec![a, b, c];
    want.sort();
    assert_eq!(rows.into_iter().map(|r| r.0).collect::<Vec<_>>(), want);
    // The one after the window gets its own.
    f.route(t + secs(130)).await;
    let m = f.messages_to(&p).await;
    assert_eq!((m.len(), m[1].alert_id.as_deref()), (2, Some(late.as_str())));
}

#[tokio::test]
async fn the_webhook_gets_every_notified_alert_and_nothing_else() {
    let f = Farm::new().await;
    f.channels(json!({"webhook": {"url": "https://example.com/hook"}})).await;
    let id = escaped(&f).await;
    let c = f.collar(Some("031"), t0()).await;
    f.set(&c, "battery = 0.1", &[]).await; // notify off
    f.eval(t0()).await;
    f.route(t0() + secs(10)).await;
    f.route(t0() + mins(40)).await;
    let m = f.messages().await;
    assert_eq!(m.len(), 1, "{m:#?}");
    assert_eq!((m[0].channel.as_str(), m[0].address.as_str(), m[0].alert_id.as_deref()), ("webhook", "https://example.com/hook", Some(id.as_str())));
    assert_eq!(m[0].user_id, None);
}

#[tokio::test]
async fn the_decision_text_is_the_approval_prompt_with_its_code() {
    let f = Farm::new().await;
    f.sms().await;
    let p = f.person("Cody", Role::Owner, Some("+15155550101"), true, None).await;
    let d = f.decision("MOVE", "proposed", t0() - mins(31), None).await;
    f.eval(t0()).await;
    let a = &f.open_kind("decision_waiting").await[0];
    let code = a.data["code"].as_str().unwrap().to_owned();
    f.route(t0() + secs(60)).await;
    let m = f.messages_to(&p).await;
    assert_eq!(m.len(), 1);
    assert_eq!(m[0].text, format!("Cows: move to P2 (16.6 ha, 5 d)? Reply Y or N. Code {code}"));
    assert_eq!(m[0].decision_id.as_deref(), Some(d.as_str()));
    // In acres on an imperial farm.
    let (s, _) = f.core("PUT", "/api/settings", Some(json!({"units": "imperial"}))).await;
    assert_eq!(s, StatusCode::OK);
    let tctx = op_alerts::text::TextCtx::of(&f.ctx, t0()).await.unwrap();
    assert_eq!(op_alerts::text::alert_text(a, None, &tctx), format!("Cows: move to P2 (40.9 ac, 5 d)? Reply Y or N. Code {code}"));
}

#[tokio::test]
async fn prefs_routes_check_roles_and_values() {
    let f = Farm::new().await;
    f.sms().await;
    let hand = f.person("Sam", Role::Hand, Some("+15155550102"), true, None).await;
    let me = person_identity(Role::Hand, &hand);
    let (s, v) = f.api(me.clone(), "GET", "/api/alerts/prefs/me", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((v["channels"].clone(), v["min_severity"].clone(), v["on_duty"].clone()), (json!(["sms"]), json!("warning"), json!(false)));
    let (s, v) = f.api(me.clone(), "PUT", "/api/alerts/prefs/me", Some(json!({"on_duty": true, "quiet_start": "21:00", "quiet_end": "05:30"}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["on_duty"].clone(), v["quiet_end"].clone(), v["name"].clone()), (json!(true), json!("05:30"), json!("Sam")));
    for bad in [
        json!({"channels": ["pigeon"]}),
        json!({"herds": ["herd_nope"]}),
        json!({"quiet_start": "21:00", "quiet_end": null}),
        json!({"muted_kinds": ["weather"]}),
    ] {
        let (s, v) = f.api(me.clone(), "PUT", "/api/alerts/prefs/me", Some(bad.clone())).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{bad} → {v}");
    }
    // null puts the quiet hours back to the farm's.
    let (_, v) = f.api(me.clone(), "PUT", "/api/alerts/prefs/me", Some(json!({"quiet_start": null, "quiet_end": null}))).await;
    assert!(v.get("quiet_start").is_none());
    // Everyone's prefs are for managers; someone else's are the owner's to change.
    let (s, _) = f.api(me.clone(), "GET", "/api/alerts/prefs", None).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = f.api(person_identity(Role::Manager, "usr_m"), "GET", "/api/alerts/prefs", None).await;
    assert_eq!((s, v.as_array().unwrap().len()), (StatusCode::OK, 1));
    let (s, _) = f.api(person_identity(Role::Manager, "usr_m"), "PUT", &format!("/api/alerts/prefs/{hand}"), Some(json!({"on_duty": false}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, v) = f.owner("PUT", &format!("/api/alerts/prefs/{hand}"), Some(json!({"on_duty": false}))).await;
    assert_eq!((s, v["on_duty"].clone()), (StatusCode::OK, json!(false)));
    let (s, _) = f.owner("PUT", "/api/alerts/prefs/usr_nope", Some(json!({"on_duty": false}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // A viewer may read their own; a sign-in that isn't a person has none.
    let (s, _) = f.api(person_identity(Role::Viewer, &hand), "PUT", "/api/alerts/prefs/me", Some(json!({"on_duty": true}))).await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = f.owner("GET", "/api/alerts/prefs/me", None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    // Person channels follow what is set up.
    let (_, v) = f.owner("GET", "/api/alerts/rules", None).await;
    assert_eq!((v["configured"].clone(), v["person_channels"].clone()), (json!(["sms"]), json!(["sms"])));
}

#[tokio::test]
async fn the_brief_lists_what_doesnt_text() {
    let f = Farm::new().await;
    op_alerts::text::brief::register(&f.ctx);
    op_alerts::text::brief::register(&f.ctx); // idempotent
    let t = t0();
    let cs = f.collars(3, t).await;
    f.set(&cs[0], "battery = 0.14", &[]).await;
    f.set(&cs[1], "battery = 0.16", &[]).await;
    for i in 0..9 {
        f.fix(&cs[2], t - mins(i), f.spot(10.0, 10.0), 25.0).await;
    }
    f.eval(t).await;
    let lines = f.ctx.brief_lines().collect(&f.ctx, &f.herd, t).await;
    assert_eq!(lines, vec!["Battery low: 101 14%, 102 16%. GPS weak: 103".to_owned()]);
    let none = f.ctx.brief_lines().collect(&f.ctx, "herd_other", t).await;
    assert!(none.is_empty());
}
