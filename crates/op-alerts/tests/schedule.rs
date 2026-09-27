//! S's alert and texts: `schedule_not_stored` (the next strip due within two
//! hours isn't on every collar that should hold it) and the approval prompt
//! for a decision about a strip schedule.

mod a_engine_fixture;

use a_engine_fixture::{Farm, mins, t0};
use op_alerts::rules::{Rule, ScheduleNotStored, rules};
use op_alerts::text::{TextCtx, alert_text, is_gsm7, septets};
use op_core::Severity;
use op_core::alert::{Alert, AlertStatus};
use op_core::time::to_db;
use op_core::units::{Fmt, Units};
use serde_json::{Value, json};

async fn schedule(f: &Farm, at: chrono::DateTime<chrono::Utc>, staged: chrono::DateTime<chrono::Utc>, version: Option<u32>) -> String {
    let id = "sch_1".to_owned();
    sqlx::query(
        "INSERT INTO schedules (id, herd_id, paddock_id, strips, next_index, cadence, starts_at, back_fence, status, created_by, created_at, updated_at)
         VALUES (?, ?, ?, '[]', 1, '{\"every_days\":1,\"at\":\"07:00\"}', ?, '{}', 'active', '{\"via\":\"local\"}', ?, ?)",
    )
    .bind(&id)
    .bind(&f.herd)
    .bind(&f.paddocks[0])
    .bind(to_db(&at))
    .bind(to_db(&staged))
    .bind(to_db(&staged))
    .execute(f.ctx.db())
    .await
    .unwrap();
    let g = json!({"type": "Polygon", "coordinates": [[[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]]]});
    sqlx::query(
        "INSERT INTO schedule_moves (schedule_id, strip, step, occurrence, at, geometry, state, boundary_version, updated_at) VALUES (?, 1, 0, 0, ?, ?, ?, ?, ?)",
    )
    .bind(&id)
    .bind(to_db(&at))
    .bind(g.to_string())
    .bind(if version.is_some() { "staged" } else { "planned" })
    .bind(version.map(i64::from))
    .bind(to_db(&staged))
    .execute(f.ctx.db())
    .await
    .unwrap();
    id
}

async fn stores(f: &Farm, collar: &str, version: u32) {
    sqlx::query("INSERT INTO collar_slots (collar_id, version, status, reported_at) VALUES (?, ?, 'received', ?)")
        .bind(collar)
        .bind(i64::from(version))
        .bind(to_db(&t0()))
        .execute(f.ctx.db())
        .await
        .unwrap();
}

async fn evaluate(f: &Farm, now: chrono::DateTime<chrono::Utc>) -> Vec<op_alerts::rules::Candidate> {
    let r = ScheduleNotStored;
    r.evaluate(&f.ctx, &r.descriptor().default, now).await.unwrap()
}

#[tokio::test]
async fn the_next_strip_missing_on_a_collar_within_two_hours_is_an_alert() {
    let f = Farm::new().await;
    let now = t0();
    let collars = f.collars(3, now).await;
    f.boundary(5, now - mins(60), Some(now + mins(90))).await;
    schedule(&f, now + mins(90), now - mins(10), Some(5)).await;
    stores(&f, &collars[0], 5).await;
    stores(&f, &collars[1], 5).await;
    let c = evaluate(&f, now).await;
    assert_eq!(c.len(), 1);
    let c = &c[0];
    assert_eq!((c.key.as_str(), c.title.as_str()), ("schedule_not_stored:sch_1", "103 missing strip 2"));
    assert_eq!(c.targets, vec![("schedule".to_owned(), "sch_1".to_owned()), ("collar".to_owned(), collars[2].clone())]);
    assert_eq!((c.data["missing"].as_u64(), c.data["total"].as_u64(), c.data["strip"].as_u64()), (Some(1), Some(3), Some(2)));
    // Held by all three: nothing.
    stores(&f, &collars[2], 5).await;
    assert!(evaluate(&f, now).await.is_empty());
}

#[tokio::test]
async fn only_collars_expected_to_hold_it_and_only_near_its_time() {
    let f = Farm::new().await;
    let now = t0();
    let collars = f.collars(3, now).await;
    schedule(&f, now + mins(90), now - mins(10), Some(5)).await;
    stores(&f, &collars[0], 5).await;
    // Silent for an hour, and out on an escape: not expected.
    f.seen(&collars[1], now - mins(60)).await;
    f.escape(&collars[2], "returning", now - mins(5), None).await;
    assert!(evaluate(&f, now).await.is_empty());
    // More than two hours out: not yet.
    f.seen(&collars[1], now).await;
    assert!(evaluate(&f, now - mins(40)).await.is_empty());
    assert_eq!(evaluate(&f, now).await.len(), 1);
    // Staged a moment ago: collars get five minutes to fetch it.
    sqlx::query("UPDATE schedule_moves SET updated_at = ?").bind(to_db(&(now - mins(2)))).execute(f.ctx.db()).await.unwrap();
    assert!(evaluate(&f, now).await.is_empty());
    // Not staged at all, a while before it opens: every expected collar lacks it.
    sqlx::query("UPDATE schedule_moves SET updated_at = ?, state = 'planned', boundary_version = NULL")
        .bind(to_db(&(now - mins(30))))
        .execute(f.ctx.db())
        .await
        .unwrap();
    let c = evaluate(&f, now).await;
    assert_eq!(c[0].title, "2 collars missing strip 2");
}

#[tokio::test]
async fn the_rule_is_listed_with_its_sentence() {
    let r = rules().into_iter().map(|r| r.descriptor()).find(|d| d.kind == "schedule_not_stored").unwrap();
    assert_eq!(
        (r.sentence, r.unit, r.default.after_min, r.default.severity, r.default.notify),
        ("Next strip not on every collar {n} before it opens", "min", Some(120), Severity::Warning, true)
    );
    assert!(!r.wake_on.contains(&"fix") && !r.wake_on.contains(&"collar"));
}

fn alert(kind: &str, key: &str, data: Value) -> Alert {
    Alert {
        id: "alr_1".into(),
        kind: kind.into(),
        key: key.into(),
        severity: Severity::Warning,
        status: AlertStatus::Open,
        herd_id: Some("herd_1".into()),
        title: "t".into(),
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

fn ctx(units: Units, now: chrono::DateTime<chrono::Utc>) -> TextCtx {
    TextCtx { fmt: Fmt::new(units), tz: chrono_tz::America::Chicago, now }
}

#[test]
fn a_decision_about_a_schedule_asks_keep_or_hold() {
    // 06:10 on the farm; strip 4 opens at 07:00 today.
    let now = t0() - mins(50);
    let sched = json!({"strip": 4, "of": 12, "opens_at": to_db(&t0())});
    let stay =
        alert("decision_waiting", "decision_waiting:dec_1", json!({"herd": "Cows", "action": "STAY", "paddock": "P3", "code": "4821", "schedule": sched}));
    assert_eq!(alert_text(&stay, None, &ctx(Units::Imperial, now)), "Cows: strip 4 of 12 opens 07:00. Reply Y to keep, N to hold. Code 4821");
    let hold = alert("decision_waiting", "decision_waiting:dec_2", json!({"herd": "Cows", "action": "HOLD", "code": "1234", "schedule": sched}));
    assert_eq!(alert_text(&hold, None, &ctx(Units::Metric, now)), "Cows: hold today's strip? Strip 4 of 12 is due 07:00. Reply Y or N. Code 1234");
    // Another day: the weekday too.
    let later = json!({"strip": 4, "of": 12, "opens_at": to_db(&(t0() + mins(24 * 60)))});
    let stay = alert("decision_waiting", "decision_waiting:dec_3", json!({"herd": "Cows", "action": "STAY", "code": "4821", "schedule": later}));
    assert_eq!(alert_text(&stay, None, &ctx(Units::Imperial, now)), "Cows: strip 4 of 12 opens Mon 07:00. Reply Y to keep, N to hold. Code 4821");
    // Worst-case herd names still fit, in GSM-7, with the code whole.
    let long = "Cöws “Big” 🐄 ".repeat(20);
    let a = alert("decision_waiting", "decision_waiting:dec_4", json!({"herd": long, "action": "STAY", "code": "4821", "schedule": later}));
    let text = alert_text(&a, None, &ctx(Units::Imperial, now));
    assert!(septets(&text) <= 160 && is_gsm7(&text) && text.ends_with("Code 4821"), "{text}");
}

#[test]
fn the_missing_strip_text_names_the_strip_and_its_time() {
    let now = t0() - mins(50);
    let one = alert(
        "schedule_not_stored",
        "schedule_not_stored:sch_1",
        json!({"herd": "Cows", "strip": 4, "opens_at": to_db(&t0()), "missing": 1, "total": 250, "labels": ["214"]}),
    );
    assert_eq!(alert_text(&one, None, &ctx(Units::Imperial, now)), "Cows: 214 missing strip 4 (opens 07:00). Check coverage");
    let many = alert(
        "schedule_not_stored",
        "schedule_not_stored:sch_1",
        json!({"herd": "Cows", "strip": 4, "opens_at": to_db(&t0()), "missing": 12, "total": 250, "labels": ["214", "031"]}),
    );
    assert_eq!(alert_text(&many, None, &ctx(Units::Imperial, now)), "Cows: 12 of 250 collars missing strip 4 (opens 07:00). Check coverage");
    let long = "Heifers ✨ “north” — ".repeat(12);
    let worst = alert(
        "schedule_not_stored",
        "schedule_not_stored:sch_1",
        json!({"herd": long, "strip": 199, "opens_at": to_db(&t0()), "missing": 1, "labels": [long.clone()]}),
    );
    let text = alert_text(&worst, None, &ctx(Units::Metric, now));
    assert!(septets(&text) <= 160 && is_gsm7(&text), "{text}");
}
