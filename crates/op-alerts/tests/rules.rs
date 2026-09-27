//! Each alert rule opens from database state and clears when the state does.

mod a_engine_fixture;

use a_engine_fixture::*;
use op_core::Severity;
use op_core::alert::AlertStatus;

/// Opens on `on`, survives a gap shorter than `clear_after_min` with the same
/// row, resolves after the full gap, and opens a new row when back.
async fn lifecycle<On, Off, F1, F2>(f: &Farm, kind: &str, on: On, off: Off)
where
    On: Fn(chrono::DateTime<chrono::Utc>) -> F1,
    Off: Fn(chrono::DateTime<chrono::Utc>) -> F2,
    F1: std::future::Future<Output = ()>,
    F2: std::future::Future<Output = ()>,
{
    let t = t0();
    on(t).await;
    f.eval(t).await;
    let first = f.open_kind(kind).await;
    assert_eq!(first.len(), 1, "{kind} opens: {first:?}");
    // Gone for a minute, then back: the same row.
    off(t + mins(1)).await;
    f.eval(t + mins(1)).await;
    assert_eq!(f.open_kind(kind).await.len(), 1, "{kind} waits clear_after_min before resolving");
    on(t + mins(2)).await;
    f.eval(t + mins(2)).await;
    let again = f.open_kind(kind).await;
    assert_eq!(again.len(), 1);
    assert_eq!(again[0].id, first[0].id, "{kind} back inside the window keeps its row");
    // Gone for two minutes: resolved, by itself.
    off(t + mins(3)).await;
    f.eval(t + mins(3)).await;
    assert_eq!(f.open_kind(kind).await.len(), 1);
    f.eval(t + mins(4) + secs(1)).await;
    assert!(f.open_kind(kind).await.is_empty(), "{kind} resolves after clear_after_min");
    let done = f.alert(&first[0].id).await;
    assert_eq!(done.status, AlertStatus::Resolved);
    assert!(done.resolved_at.is_some() && done.resolved_by.is_none());
    // Back after it resolved: a new row.
    on(t + mins(6)).await;
    f.eval(t + mins(6)).await;
    let new = f.open_kind(kind).await;
    assert_eq!(new.len(), 1);
    assert_ne!(new[0].id, first[0].id, "{kind} back after resolve opens a new row");
}

#[tokio::test]
async fn escaped_opens_on_an_open_escape_and_clears_after_it_ends() {
    let f = Farm::new().await;
    let c = f.collar(Some("214"), t0()).await;
    f.outside(&c, t0() - mins(2), t0()).await;
    let (fr, cr) = (&f, &c);
    lifecycle(
        &f,
        "escaped",
        move |_| async move {
            fr.escape(cr, "returning", t0() - mins(1), None).await;
        },
        move |t| async move {
            fr.exec(&format!(
                "UPDATE escapes SET status = 'back', ended_at = '{}' WHERE collar_id = '{cr}' AND status = 'returning'",
                op_core::time::to_db(&t)
            ))
            .await;
        },
    )
    .await;
    let a = &f.open_kind("escaped").await[0];
    assert_eq!(a.severity, Severity::Critical);
    assert_eq!(a.title, "214 outside P1");
    assert_eq!(a.key, format!("escaped:{c}"));
    assert!(a.targets.contains(&("collar".into(), c.clone())));
    assert!(a.at.is_some());
}

#[tokio::test]
async fn outside_opens_after_five_minutes_without_an_escape() {
    let f = Farm::new().await;
    let c = f.collar(Some("214"), t0()).await;
    lifecycle(&f, "outside", |t| f.outside(&c, t - mins(6), t), |t| f.inside(&c, t)).await;
    let a = &f.open_kind("outside").await[0];
    assert_eq!((a.severity, a.title.as_str()), (Severity::Warning, "214 outside P1"));
}

#[tokio::test]
async fn outside_waits_five_minutes_and_skips_escapes_and_animals_let_go() {
    let f = Farm::new().await;
    let t = t0();
    let early = f.collar(Some("1"), t).await;
    f.outside(&early, t - mins(4), t).await;
    let escaping = f.collar(Some("2"), t).await;
    f.outside(&escaping, t - mins(10), t).await;
    f.escape(&escaping, "returning", t - mins(9), None).await;
    let let_go = f.collar(Some("3"), t).await;
    f.outside(&let_go, t - mins(10), t).await;
    f.escape(&let_go, "stopped", t - mins(9), Some(t - mins(8))).await;
    // Let go on an earlier trip out doesn't count for this one.
    let earlier = f.collar(Some("4"), t).await;
    f.outside(&earlier, t - mins(10), t).await;
    f.escape(&earlier, "stopped", t - mins(60), Some(t - mins(50))).await;
    f.eval(t).await;
    let keys: Vec<String> = f.open_kind("outside").await.into_iter().map(|a| a.key).collect();
    assert_eq!(keys, vec![format!("outside:{earlier}")]);
}

#[tokio::test]
async fn outside_is_critical_when_the_collar_is_silent_too() {
    let f = Farm::new().await;
    let t = t0();
    let c = f.collar(Some("9"), t).await;
    f.outside(&c, t - mins(40), t - mins(25)).await;
    f.eval(t).await;
    let a = &f.open_kind("outside").await[0];
    assert_eq!(a.severity, Severity::Critical);
}

#[tokio::test]
async fn silent_uses_three_times_the_usual_interval() {
    let f = Farm::new().await;
    let t = t0();
    // Reports every 10 minutes: silent after 30, not 20.
    let slow = f.collar(Some("slow"), t - mins(25)).await;
    let times: Vec<_> = (0..20).map(|i| t - mins(25) - mins(10 * i)).collect();
    f.reports(&slow, &times).await;
    // Reports every minute: silent after the 20 minute floor.
    let fast = f.collar(Some("fast"), t - mins(21)).await;
    let times: Vec<_> = (0..30).map(|i| t - mins(21) - mins(i)).collect();
    f.reports(&fast, &times).await;
    // Three that report, so the herd isn't mostly silent.
    f.collars(3, t + mins(10)).await;
    f.eval(t).await;
    let keys: Vec<String> = f.open_kind("silent").await.into_iter().map(|a| a.key).collect();
    assert_eq!(keys, vec![format!("silent:{fast}")]);
    f.eval(t + mins(6)).await;
    assert_eq!(f.open_kind("silent").await.len(), 2);
    assert_eq!(f.open_kind("silent").await[0].title, "fast silent");
}

#[tokio::test]
async fn silent_clears_and_comes_back() {
    let f = Farm::new().await;
    let c = f.collar(Some("5"), t0()).await;
    lifecycle(&f, "silent", |t| f.seen(&c, t - mins(30)), |t| f.seen(&c, t)).await;
}

#[tokio::test]
async fn silent_is_held_for_the_start_grace() {
    let f = Farm::new().await;
    let t = t0();
    let c = f.collar(Some("5"), t - mins(60)).await;
    f.collars(3, t + mins(30)).await;
    // An alert from before the restart stays as it is during the grace.
    f.eval(t).await;
    let before = f.open_kind("silent").await;
    assert_eq!(before.len(), 1);
    op_alerts::engine::set_started(&f.ctx, t + mins(1));
    f.inside(&c, t + mins(2)).await; // back, but the grace holds the rule
    f.eval(t + mins(10)).await;
    assert_eq!(f.open_kind("silent").await[0].id, before[0].id);
    // A new silent collar during the grace: nothing yet.
    let quiet = f.collar(Some("6"), t - mins(60)).await;
    f.eval(t + mins(20)).await;
    assert!(!f.open_kind("silent").await.iter().any(|a| a.key.ends_with(&quiet)));
    // After 20 minutes the rule runs again.
    f.eval(t + mins(22)).await;
    let keys: Vec<String> = f.open_kind("silent").await.into_iter().map(|a| a.key).collect();
    assert!(keys.contains(&format!("silent:{quiet}")), "{keys:?}");
}

#[tokio::test]
async fn herd_silent_takes_in_the_silent_alerts_above_half() {
    let f = Farm::new().await;
    let t = t0();
    let cs = f.collars(5, t).await;
    f.seen(&cs[0], t - mins(30)).await;
    f.seen(&cs[1], t - mins(30)).await;
    f.eval(t).await;
    assert_eq!(f.open_kind("silent").await.len(), 2);
    assert!(f.open_kind("herd_silent").await.is_empty(), "2 of 5 is not more than half");
    f.seen(&cs[2], t - mins(30)).await;
    f.eval(t + secs(10)).await;
    let hs = f.open_kind("herd_silent").await;
    assert_eq!(hs.len(), 1);
    assert_eq!(hs[0].title, "3 of 5 collars silent");
    assert_eq!(hs[0].severity, Severity::Critical);
    assert_eq!(hs[0].key, format!("herd_silent:herd:{}", f.herd));
    assert!(f.open_kind("silent").await.is_empty(), "silent alerts roll into herd_silent");
    let rolled: Vec<_> = f.every_alert().await.into_iter().filter(|a| a.kind == "silent").collect();
    assert!(rolled.iter().all(|a| a.rolled_into.as_deref() == Some(hs[0].id.as_str())));
}

#[tokio::test]
async fn herd_silent_holds_until_well_under_its_share() {
    let f = Farm::new().await;
    f.sms().await;
    let p = f.person("Cody", op_core::Role::Owner, Some("+15155550101"), true, None).await;
    let t = t0();
    let cs = f.collars(10, t).await;
    let silent = |n: usize, now: chrono::DateTime<chrono::Utc>| {
        let f = &f;
        let cs = &cs;
        async move {
            for (i, c) in cs.iter().enumerate() {
                f.seen(c, if i < n { now - mins(25) } else { now }).await;
            }
        }
    };
    // Six then five of ten silent, back and forth every three minutes for half an hour.
    let mut now = t;
    while now < t + mins(30) {
        silent(if (now - t).num_minutes() / 3 % 2 == 0 { 6 } else { 5 }, now).await;
        f.eval(now).await;
        f.route(now).await;
        now += secs(10);
    }
    let rows: Vec<_> = f.every_alert().await.into_iter().filter(|a| a.kind == "herd_silent" || a.kind == "silent").collect();
    assert_eq!(rows.iter().filter(|a| a.kind == "herd_silent").count(), 1, "one alert while it hovers at half");
    assert!(rows.iter().filter(|a| a.kind == "silent").all(|a| a.rolled_into.is_some()), "{rows:#?}");
    let texts = f.messages_to(&p).await;
    assert_eq!(texts.len(), 1, "{texts:#?}");
    assert!(texts[0].text.starts_with("Cows: 6 of 10 collars silent"), "{}", texts[0].text);
    // Four of ten (well under half) for longer than clear_after_min: it clears.
    let end = now + mins(3);
    while now <= end {
        silent(4, now).await;
        f.eval(now).await;
        now += secs(10);
    }
    assert!(f.open_kind("herd_silent").await.is_empty());
}

#[tokio::test]
async fn low_battery_opens_below_twenty_percent_without_notifying() {
    let f = Farm::new().await;
    let c = f.collar(Some("031"), t0()).await;
    lifecycle(&f, "low_battery", |_| f.set(&c, "battery = 0.14", &[]), |_| f.set(&c, "battery = 0.5", &[])).await;
    let a = &f.open_kind("low_battery").await[0];
    assert_eq!(a.title, "031 battery 14%");
    let notify: bool = sqlx::query_scalar("SELECT notify FROM alerts WHERE id = ?").bind(&a.id).fetch_one(f.ctx.db()).await.unwrap();
    assert!(!notify);
}

#[tokio::test]
async fn boundary_not_applied_after_ten_minutes() {
    let f = Farm::new().await;
    let t = t0();
    let lag = f.collar(Some("7"), t).await;
    let ok = f.collar(Some("8"), t).await;
    let out = f.collar(Some("9"), t).await;
    let quiet = f.collar(Some("10"), t - mins(40)).await;
    f.boundary(2, t - mins(60), None).await;
    f.boundary(3, t - mins(12), None).await;
    for c in [&lag, &out, &quiet] {
        f.holds(c, 2, "applied").await;
    }
    f.holds(&ok, 3, "applied").await;
    f.escape(&out, "returning", t - mins(5), None).await;
    f.eval(t).await;
    let keys: Vec<String> = f.open_kind("boundary_not_applied").await.into_iter().map(|a| a.key).collect();
    assert_eq!(keys, vec![format!("boundary_not_applied:{lag}")], "not the escaped or the silent collar");
    // A newer boundary only 5 minutes old: nothing yet.
    let f = Farm::new().await;
    let c = f.collar(Some("7"), t + mins(6)).await;
    f.boundary(3, t - mins(5), None).await;
    f.holds(&c, 2, "applied").await;
    f.eval(t).await;
    assert!(f.open_kind("boundary_not_applied").await.is_empty());
    f.eval(t + mins(6)).await;
    assert_eq!(f.open_kind("boundary_not_applied").await.len(), 1);
}

#[tokio::test]
async fn boundary_not_applied_clears_and_comes_back() {
    let f = Farm::new().await;
    let c = f.collar(Some("7"), t0() + mins(10)).await;
    f.boundary(3, t0() - mins(12), None).await;
    lifecycle(&f, "boundary_not_applied", |_| f.holds(&c, 2, "applied"), |_| f.holds(&c, 3, "applied")).await;
}

#[tokio::test]
async fn decision_waiting_carries_a_code_that_stays() {
    let f = Farm::new().await;
    let t = t0();
    let fresh = f.decision("MOVE", "proposed", t - mins(10), None).await;
    f.eval(t).await;
    assert!(f.open_kind("decision_waiting").await.is_empty(), "under 30 minutes");
    f.eval(t + mins(21)).await;
    let a = f.open_kind("decision_waiting").await;
    assert_eq!(a.len(), 1);
    assert_eq!(a[0].title, "Move to P2?");
    let code = a[0].data["code"].as_str().unwrap().to_owned();
    assert!(code.len() == 4 && code.chars().all(|c| c.is_ascii_digit()), "{code}");
    assert!(a[0].targets.contains(&("decision".into(), fresh.clone())));
    assert!(a[0].data["days"].as_f64().is_some() && a[0].data["area_ha"].as_f64().is_some());
    f.eval(t + mins(25)).await;
    assert_eq!(f.open_kind("decision_waiting").await[0].data["code"], code, "the code doesn't change");
    // Answered: resolved at once.
    f.decision_status(&fresh, "applied").await;
    f.eval(t + mins(26)).await;
    assert!(f.open_kind("decision_waiting").await.is_empty());
}

#[tokio::test]
async fn a_timer_decision_opens_at_once_and_stay_asks_to_stay() {
    let f = Farm::new().await;
    let t = t0();
    f.decision("MOVE", "proposed", t, Some(t + mins(60))).await;
    f.eval(t).await;
    assert_eq!(f.open_kind("decision_waiting").await.len(), 1);
    let g = Farm::new().await;
    g.decision("STAY", "proposed", t - mins(31), None).await;
    g.eval(t).await;
    let a = &g.open_kind("decision_waiting").await[0];
    assert_eq!((a.title.as_str(), a.data["action"].as_str()), ("Stay in P1?", Some("STAY")));
}

#[tokio::test]
async fn move_stalled_after_fifteen_minutes_without_a_step() {
    let f = Farm::new().await;
    let t = t0();
    let m = f.movement("sweeping", t - mins(40), Some(t - mins(10)), &[]).await;
    f.eval(t).await;
    assert!(f.open_kind("move_stalled").await.is_empty());
    f.eval(t + mins(6)).await;
    let a = &f.open_kind("move_stalled").await[0];
    assert_eq!(a.key, format!("move_stalled:{m}"));
    assert_eq!(a.title, "Move to P2 stalled");
    assert_eq!(a.data["staged"], false);
    // Done: it clears.
    f.exec(&format!("UPDATE moves SET status = 'done' WHERE id = '{m}'")).await;
    f.eval(t + mins(7)).await;
    f.eval(t + mins(9) + secs(1)).await;
    assert!(f.open_kind("move_stalled").await.is_empty());
}

#[tokio::test]
async fn stragglers_are_info_while_the_move_runs() {
    let f = Farm::new().await;
    let t = t0();
    let cs = f.collars(3, t).await;
    let m = f.movement("sweeping", t - mins(5), Some(t - mins(1)), &[&cs[0], &cs[1]]).await;
    f.eval(t).await;
    let a = &f.open_kind("stragglers").await[0];
    assert_eq!((a.severity, a.title.as_str()), (Severity::Info, "2 behind"));
    // Done, and both back inside: it clears.
    f.exec(&format!("UPDATE moves SET status = 'done', updated_at = '{}' WHERE id = '{m}'", op_core::time::to_db(&t))).await;
    f.eval(t + mins(1)).await;
    f.eval(t + mins(3) + secs(1)).await;
    assert!(f.open_kind("stragglers").await.is_empty());
}

#[tokio::test]
async fn drop_off_when_every_fix_stays_within_four_metres_for_four_hours() {
    let f = Farm::new().await;
    let t = t0();
    let still = f.collar(Some("down"), t).await;
    let grazing = f.collar(Some("up"), t).await;
    let short = f.collar(Some("new"), t).await;
    let base = f.spot(100.0, 100.0);
    let proj = op_geo::Projection::new(base);
    for i in 0..(4 * 60 + 5) {
        let at = t - mins(4 * 60 + 5) + mins(i);
        let jitter = ((i % 7) as f64 - 3.0) * 0.4;
        f.fix(&still, at, proj.offset(jitter, -jitter), 3.0).await;
        f.fix(&grazing, at, proj.offset(i as f64 * 0.5, 0.0), 3.0).await;
        if i > 60 {
            f.fix(&short, at, base, 3.0).await;
        }
    }
    for c in [&still, &grazing, &short] {
        let fix = serde_json::json!({"at": op_core::time::to_db(&(t - mins(1))), "point": base, "accuracy_m": 3.0, "sats": 9});
        f.set(c, "last_fix = ?", &[&fix.to_string()]).await;
    }
    f.eval(t).await;
    let keys: Vec<String> = f.open_kind("drop_off").await.into_iter().map(|a| a.key).collect();
    assert_eq!(keys, vec![format!("drop_off:{still}")]);
    assert_eq!(f.open_kind("drop_off").await[0].title, "down not moving");
}

#[tokio::test]
async fn gps_degraded_on_poor_accuracy_or_no_fixes() {
    let f = Farm::new().await;
    let t = t0();
    let poor = f.collar(Some("207"), t).await;
    let good = f.collar(Some("208"), t).await;
    let blind = f.collar(Some("209"), t).await;
    for i in 0..9 {
        f.fix(&poor, t - mins(i), f.spot(10.0, 10.0), 18.0).await;
        f.fix(&good, t - mins(i), f.spot(10.0, 10.0), 3.0).await;
    }
    // Reports arrive, but its last fix is 20 minutes old.
    let old = serde_json::json!({"at": op_core::time::to_db(&(t - mins(20))), "point": f.spot(5.0, 5.0), "accuracy_m": 3.0, "sats": 4});
    f.set(&blind, "last_fix = ?", &[&old.to_string()]).await;
    f.eval(t).await;
    let open = f.open_kind("gps_degraded").await;
    let keys: Vec<&str> = open.iter().map(|a| a.key.as_str()).collect();
    assert_eq!(keys.len(), 2, "{keys:?}");
    assert!(keys.contains(&format!("gps_degraded:{poor}").as_str()) && keys.contains(&format!("gps_degraded:{blind}").as_str()));
    let p = open.iter().find(|a| a.key.ends_with(&poor)).unwrap();
    assert_eq!((p.severity, p.data["accuracy_m"].as_f64()), (Severity::Info, Some(18.0)));
}

#[tokio::test]
async fn parked_collars_and_removed_animals_never_alert() {
    let f = Farm::new().await;
    let t = t0();
    let parked = f.collar(Some("P"), t - mins(40)).await;
    let removed = f.collar(Some("R"), t - mins(40)).await;
    f.boundary(3, t - mins(30), None).await;
    for c in [&parked, &removed] {
        f.outside(c, t - mins(30), t - mins(40)).await;
        f.set(c, "battery = 0.05", &[]).await;
        f.escape(c, "returning", t - mins(20), None).await;
        f.holds(c, 1, "applied").await;
        for i in 0..9 {
            f.fix(c, t - mins(i), f.spot(1.0, 1.0), 30.0).await;
        }
    }
    let m = f.movement("sweeping", t - mins(5), Some(t - mins(1)), &[&parked, &removed]).await;
    f.set(&parked, "parked_at = ?, parked_reason = 'charging'", &[&op_core::time::to_db(&(t - mins(50)))]).await;
    f.exec(&format!("UPDATE animals SET removed_at = '{}', removed_reason = 'sold' WHERE collar_id = '{removed}'", op_core::time::to_db(&(t - mins(50)))))
        .await;
    f.eval(t).await;
    let open = f.open().await;
    assert!(open.iter().all(|a| !a.targets.iter().any(|(_, id)| *id == parked || *id == removed)), "{open:#?}");
    assert!(open.iter().all(|a| !a.key.contains(&m)), "no stragglers alert for them either");
}
