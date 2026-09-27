//! H's fleet rules: `fit_check_due` from `collar_fit_checks` and the
//! `fleet.fit_check_days` setting (info, no texts, counted in the brief),
//! and `drop_off` from a collar's IMU (still ≥ 45 min, tilted past 60°).

mod a_engine_fixture;

use a_engine_fixture::*;
use op_core::Severity;
use op_core::time::to_db;

async fn fit_check(f: &Farm, collar: &str, at: chrono::DateTime<chrono::Utc>) {
    sqlx::query("INSERT INTO collar_fit_checks (id, collar_id, checked_at) VALUES (?, ?, ?)")
        .bind(op_core::id::new_id("fit"))
        .bind(collar)
        .bind(to_db(&at))
        .execute(f.ctx.db())
        .await
        .unwrap();
}

#[tokio::test]
async fn a_fit_check_is_due_fit_check_days_after_the_last_one() {
    let f = Farm::new().await;
    let t = t0();
    let checked_long_ago = f.collar(Some("214"), t).await;
    let checked_lately = f.collar(Some("031"), t).await;
    let never_checked = f.collar(Some("118"), t).await;
    let spare = f.collar(None, t).await;
    let parked = f.collar(Some("207"), t).await;
    fit_check(&f, &checked_long_ago, t - chrono::Duration::days(31)).await;
    fit_check(&f, &checked_lately, t - chrono::Duration::days(3)).await;
    // Added 40 days ago and never checked; a spare, and a parked collar, the same.
    for c in [&never_checked, &spare, &parked] {
        f.set(c, "created_at = ?", &[&to_db(&(t - chrono::Duration::days(40)))]).await;
    }
    f.set(&parked, "parked_at = ?, parked_reason = 'charging'", &[&to_db(&t)]).await;
    f.eval(t).await;
    let mut keys: Vec<String> = f.open_kind("fit_check_due").await.into_iter().map(|a| a.key).collect();
    keys.sort();
    let mut want = vec![format!("fit_check_due:{checked_long_ago}"), format!("fit_check_due:{never_checked}")];
    want.sort();
    assert_eq!(keys, want, "only collars on animals, not parked");
    let a = f.open_kind("fit_check_due").await.into_iter().find(|a| a.key.ends_with(&checked_long_ago)).unwrap();
    assert_eq!((a.severity, a.title.as_str()), (Severity::Info, "214 fit check due"));
    assert_eq!(a.data["due_at"], to_db(&(t - chrono::Duration::days(1))));
    // A longer interval: nothing due.
    f.ctx.store().set_setting_json("fleet", &serde_json::json!({ "fit_check_days": 60 })).await.unwrap();
    f.eval(t + mins(1)).await;
    f.eval(t + mins(4)).await;
    assert!(f.open_kind("fit_check_due").await.is_empty());
    // No texts: info never notifies.
    f.sms().await;
    assert!(f.route(t + mins(5)).await.is_empty());
}

#[tokio::test]
async fn the_brief_counts_fit_checks_due() {
    let f = Farm::new().await;
    let t = t0();
    let cs = f.collars(5, t).await;
    for c in &cs {
        f.set(c, "created_at = ?", &[&to_db(&(t - chrono::Duration::days(45)))]).await;
    }
    f.eval(t).await;
    // Five in one herd roll up into one.
    let open = f.open_kind("fit_check_due").await;
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].title, "5 fit checks due");
    let line = op_alerts::text::brief::attention(&f.ctx, &f.herd, t).await.unwrap();
    assert_eq!(line.as_deref(), Some("Fit check due: 5"));
}

async fn health(f: &Farm, collar: &str, at: chrono::DateTime<chrono::Utc>, still_s: Option<f64>, tilt: Option<f64>) {
    sqlx::query("INSERT INTO health (collar_id, herd_id, at, t, battery, still_s, tilt_deg) VALUES (?, ?, ?, ?, 0.8, ?, ?)")
        .bind(collar)
        .bind(&f.herd)
        .bind(to_db(&at))
        .bind(at.timestamp_millis())
        .bind(still_s)
        .bind(tilt)
        .execute(f.ctx.db())
        .await
        .unwrap();
}

#[tokio::test]
async fn an_imu_collar_lying_still_on_its_side_has_come_off() {
    let f = Farm::new().await;
    let t = t0();
    let fallen = f.collar(Some("fell"), t).await;
    let lying_down = f.collar(Some("rest"), t).await;
    let short = f.collar(Some("new"), t).await;
    let grazing = f.collar(Some("up"), t).await;
    health(&f, &fallen, t - mins(1), Some(50.0 * 60.0), Some(84.0)).await;
    // Still as long, but upright on a neck: an animal lying down.
    health(&f, &lying_down, t - mins(1), Some(50.0 * 60.0), Some(20.0)).await;
    health(&f, &short, t - mins(1), Some(20.0 * 60.0), Some(88.0)).await;
    health(&f, &grazing, t - mins(1), Some(3.0), Some(35.0)).await;
    for c in [&fallen, &lying_down, &short, &grazing] {
        let fix = serde_json::json!({"at": to_db(&(t - mins(1))), "point": f.spot(40.0, 40.0), "accuracy_m": 3.0, "sats": 9});
        f.set(c, "last_fix = ?", &[&fix.to_string()]).await;
    }
    f.eval(t).await;
    let open = f.open_kind("drop_off").await;
    assert_eq!(open.iter().map(|a| a.key.as_str()).collect::<Vec<_>>(), vec![format!("drop_off:{fallen}").as_str()]);
    let a = &open[0];
    assert_eq!(a.title, "fell not moving");
    assert_eq!((a.data["imu"].as_bool(), a.data["tilt_deg"].as_f64()), (Some(true), Some(84.0)));
    assert_eq!(a.data["since"], to_db(&(t - mins(51))), "still since 50 min before its report");

    // A shorter rule time applies to the IMU too.
    f.rule("drop_off", serde_json::json!({ "after_min": 15 })).await;
    f.eval(t + mins(6)).await;
    let mut keys: Vec<String> = f.open_kind("drop_off").await.into_iter().map(|a| a.key).collect();
    keys.sort();
    let mut want = vec![format!("drop_off:{fallen}"), format!("drop_off:{short}")];
    want.sort();
    assert_eq!(keys, want);
}
