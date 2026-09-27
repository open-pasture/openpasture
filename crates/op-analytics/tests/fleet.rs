//! `GET /api/fleet`, fit checks, fleet settings and `get_fleet`
//! (field-ready G). Battery trends come from `battery_days` only.

mod g_support;

use axum::http::StatusCode;
use chrono::{DateTime, Duration, Utc};
use g_support::*;
use op_core::{Identity, Role, Via, time};
use serde_json::{Value, json};

fn ts(v: &Value) -> DateTime<Utc> {
    time::from_db(v.as_str().unwrap_or_else(|| panic!("not a time: {v}"))).unwrap()
}

/// col_1 has drained 3 points a day in a straight line for four days and
/// today so far (a reading an hour); col_2 has only today's readings.
async fn fleet() -> App {
    let app = App::new(2).await;
    let now = time::now();
    let today = midnight(now).timestamp_millis();
    let start = today - 4 * DAY;
    let level = |t: i64| 0.9 - 0.03 * (t - start) as f64 / DAY as f64;
    let mut rows = Vec::new();
    let mut t = start;
    while t <= now.timestamp_millis() {
        rows.push(("col_1", t, Some(level(t))));
        if t >= today {
            rows.push(("col_2", t, Some(0.5)));
        }
        t += HOUR;
    }
    app.health(&rows).await;
    let last = rows.iter().filter(|r| r.0 == "col_1").last().unwrap();
    sqlx::query("UPDATE collars SET battery = ?, last_seen = ? WHERE id = 'col_1'")
        .bind(last.2)
        .bind(time::to_db(&time::from_unix_ms(last.1)))
        .execute(app.ctx.db())
        .await
        .unwrap();
    op_analytics::days::aggregate(&app.ctx, now).await.unwrap();
    app
}

#[tokio::test]
async fn rows_carry_the_trend_days_left_and_daily_battery() {
    let _one = serial().lock().await;
    let app = fleet().await;
    let rows = app.get("/api/fleet").await;
    let rows = rows.as_array().unwrap();
    assert_eq!(rows.len(), 2);
    let (a, b) = (&rows[0], &rows[1]);
    assert_eq!(
        (a["collar_id"].as_str(), a["name"].as_str(), a["tag"].as_str(), a["herd_id"].as_str()),
        (Some("col_1"), Some("C-0001"), Some("101"), Some("herd_1"))
    );
    assert_eq!(a["trend_pct_day"], json!(-3.0));
    let battery = a["battery"].as_f64().unwrap();
    assert_eq!(a["days_left"].as_f64().unwrap(), (battery / 0.03 * 10.0).round() / 10.0);
    let daily = a["daily"].as_array().unwrap();
    assert_eq!(daily.len(), 14);
    assert!(daily[..9].iter().all(Value::is_null), "{daily:?}");
    let seen: Vec<f64> = daily[9..].iter().map(|v| v.as_f64().unwrap()).collect();
    assert!(seen.windows(2).all(|w| w[0] > w[1]) && (seen[0] - 0.8855).abs() < 0.001, "{seen:?}");
    assert_eq!(a["parked"], json!(false));
    assert!(a.get("fit_checked_at").is_none());
    // Never checked: due 30 days after the collar was added (40 days ago).
    let added: String = sqlx::query_scalar("SELECT created_at FROM collars WHERE id = 'col_1'").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(ts(&a["fit_due_at"]), time::from_db(&added).unwrap() + Duration::days(30));
    assert!(time::now() - ts(&a["last_seen"]) < Duration::hours(2));

    // One day of readings: its level, no trend yet.
    assert_eq!(b["daily"][13], json!(0.5));
    assert!(b.get("trend_pct_day").is_none() && b.get("days_left").is_none() && b.get("battery").is_none(), "{b}");

    assert_eq!(app.get("/api/fleet?herd_id=herd_2").await, json!([]));
    let one = app.get("/api/fleet?collar_id=col_2").await;
    assert_eq!(one.as_array().unwrap().len(), 1);
    assert_eq!(one[0]["collar_id"], "col_2");
}

#[tokio::test]
async fn fit_checks_one_at_a_time_and_on_a_chute_day() {
    let _one = serial().lock().await;
    let app = App::new(3).await;

    let (status, first) = app.call("POST", "/api/fleet/col_1/fit-checks", None).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert!(first["id"].as_str().unwrap().starts_with("fit_"));
    assert_eq!(first["by"], json!({ "via": "local" }));
    assert!(time::now() - ts(&first["checked_at"]) < Duration::seconds(5));

    let two_days = time::now() - Duration::days(2);
    let sam = Identity { role: Role::Hand, user_id: Some("usr_1".into()), name: Some("Sam".into()), via: Via::UserToken };
    let (status, older) = app
        .call_as(sam, "POST", "/api/fleet/col_1/fit-checks", Some(json!({ "checked_at": time::to_db(&two_days), "notes": "  loose, took a notch in  " })))
        .await;
    assert_eq!(status, StatusCode::CREATED, "{older}");
    assert_eq!(older["notes"], "loose, took a notch in");
    assert_eq!(older["by"], json!({ "via": "user_token", "user_id": "usr_1", "name": "Sam" }));
    assert_eq!(ts(&older["checked_at"]), two_days);

    let history = app.get("/api/fleet/col_1/fit-checks").await;
    assert_eq!(history.as_array().unwrap().iter().map(|c| c["id"].clone()).collect::<Vec<_>>(), vec![first["id"].clone(), older["id"].clone()]);

    // The newest check sets the due date, 30 days on; then 14.
    let row = app.get("/api/fleet?collar_id=col_1").await[0].clone();
    assert_eq!(row["fit_checked_at"], first["checked_at"]);
    assert_eq!(ts(&row["fit_due_at"]), ts(&first["checked_at"]) + Duration::days(30));
    assert_eq!(app.get("/api/fleet/settings").await, json!({ "fit_check_days": 30 }));
    let (status, s) = app.call("PUT", "/api/fleet/settings", Some(json!({ "fit_check_days": 14 }))).await;
    assert_eq!((status, s), (StatusCode::OK, json!({ "fit_check_days": 14 })));
    assert_eq!(ts(&app.get("/api/fleet?collar_id=col_1").await[0]["fit_due_at"]), ts(&first["checked_at"]) + Duration::days(14));
    for bad in [0, 366] {
        let (status, _) = app.call("PUT", "/api/fleet/settings", Some(json!({ "fit_check_days": bad }))).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }
    assert_eq!(app.get("/api/fleet/settings").await, json!({ "fit_check_days": 14 }));

    // Chute day: every collar through at once, each named once.
    let (status, many) = app.call("POST", "/api/fleet/fit-checks", Some(json!({ "collar_ids": ["col_2", "col_3", "col_2"], "notes": "chute" }))).await;
    assert_eq!(status, StatusCode::CREATED, "{many}");
    let many = many.as_array().unwrap();
    assert_eq!(many.iter().map(|c| c["collar_id"].as_str().unwrap()).collect::<Vec<_>>(), ["col_2", "col_3"]);
    assert_eq!(many[0]["checked_at"], many[1]["checked_at"]);
    let rows = app.get("/api/fleet").await;
    assert!(rows.as_array().unwrap().iter().all(|r| r.get("fit_checked_at").is_some()));

    // Refused, with nothing recorded.
    let before = app.count("SELECT COUNT(*) FROM collar_fit_checks").await;
    let future = time::to_db(&(time::now() + Duration::hours(1)));
    for (path, body, want) in [
        ("/api/fleet/fit-checks", json!({ "collar_ids": ["col_1", "col_9"] }), StatusCode::NOT_FOUND),
        ("/api/fleet/fit-checks", json!({ "collar_ids": [] }), StatusCode::BAD_REQUEST),
        ("/api/fleet/fit-checks", json!({ "collar_ids": ["col_1"], "checked_at": future }), StatusCode::BAD_REQUEST),
        ("/api/fleet/fit-checks", json!({ "ids": ["col_1"] }), StatusCode::UNPROCESSABLE_ENTITY),
        ("/api/fleet/col_9/fit-checks", json!({}), StatusCode::NOT_FOUND),
        ("/api/fleet/col_1/fit-checks", json!({ "notes": "x".repeat(501) }), StatusCode::BAD_REQUEST),
        ("/api/fleet/col_1/fit-checks", json!({ "note": "typo" }), StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let (status, v) = app.call("POST", path, Some(body)).await;
        assert_eq!(status, want, "{path}: {v}");
        assert!(v["error"].is_string());
    }
    assert_eq!(app.count("SELECT COUNT(*) FROM collar_fit_checks").await, before);
    assert_eq!(app.call("GET", "/api/fleet/col_9/fit-checks", None).await.0, StatusCode::NOT_FOUND);

    // Each check is a line in the activity log, by whoever made it.
    let log: Vec<(String, String)> =
        sqlx::query_as("SELECT title, payload FROM events WHERE kind = 'fleet.fit_checked' ORDER BY recorded_at, id").fetch_all(app.ctx.db()).await.unwrap();
    let titles: Vec<&str> = log.iter().map(|l| l.0.as_str()).collect();
    assert_eq!(titles.len(), 3);
    assert!(titles.contains(&"Fit checked: 101") && titles.contains(&"Fit checked: 2 collars"), "{titles:?}");
    assert!(log.iter().any(|l| serde_json::from_str::<Value>(&l.1).unwrap()["by"] == "Sam"));
}

#[tokio::test]
async fn parked_collars_say_so() {
    let _one = serial().lock().await;
    let app = App::new(2).await;
    sqlx::query("UPDATE collars SET parked_at = ?, parked_reason = 'charging' WHERE id = 'col_2'")
        .bind(time::to_db(&time::now()))
        .execute(app.ctx.db())
        .await
        .unwrap();
    let rows = app.get("/api/fleet").await;
    assert_eq!(rows[0]["parked"], json!(false));
    assert_eq!(rows[1]["parked"], json!(true));
    // Nothing reported yet: no battery, no trend, fourteen empty days.
    assert_eq!(rows[1]["daily"], json!(vec![Value::Null; 14]));
}

#[tokio::test]
async fn the_fleet_never_reads_raw_telemetry() {
    let _one = serial().lock().await;
    count_queries();
    let app = fleet().await;
    take_statements();
    for path in ["/api/fleet", "/api/fleet?herd_id=herd_1", "/api/fleet?collar_id=col_1", "/api/fleet/settings", "/api/fleet/col_1/fit-checks"] {
        assert_eq!(app.call("GET", path, None).await.0, StatusCode::OK, "{path}");
    }
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let seen = take_statements();
    assert!(seen.iter().any(|s| names_table(s, "battery_days")), "the counter saw nothing: {seen:?}");
    let raw: Vec<&String> = seen.iter().filter(|s| names_table(s, "fixes") || names_table(s, "health")).collect();
    assert!(raw.is_empty(), "{raw:?}");
    app.count("SELECT COUNT(*) FROM health").await;
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert!(take_statements().iter().any(|s| names_table(s, "health")));
}

#[tokio::test]
async fn get_fleet_through_the_tool_registry() {
    let _one = serial().lock().await;
    let app = fleet().await;
    op_analytics::register_tools(&app.ctx);
    let spec = app.ctx.tools().get("get_fleet").unwrap();
    assert!(spec.read && !spec.brain && spec.min_role == Role::Viewer);
    let viewer = Identity { role: Role::Viewer, user_id: None, name: None, via: Via::UserToken };
    let scope = op_core::tools::ToolScope::Full;
    let v = app.ctx.tools().call(&app.ctx, "get_fleet", json!({ "herd_id": "herd_1" }), None, viewer.clone(), &scope).await.unwrap();
    assert_eq!(v["fit_check_days"], json!(30));
    assert_eq!(v["collars"].as_array().unwrap().len(), 2);
    assert_eq!(v["collars"][0]["trend_pct_day"], json!(-3.0));
    let e = app.ctx.tools().call(&app.ctx, "get_fleet", json!({ "herd_id": "herd_9" }), None, viewer, &scope).await.unwrap_err();
    assert_eq!(e.status, StatusCode::NOT_FOUND);
    // Both tools are listed for a viewer and in a text question's runner.
    let names: Vec<&str> = app.ctx.tools().listed_for(&Identity::brain(), &op_core::tools::ToolScope::Full).iter().map(|t| t.name).collect();
    assert!(names.contains(&"get_fleet") && names.contains(&"get_coverage"), "{names:?}");
}
