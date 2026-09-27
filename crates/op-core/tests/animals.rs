//! K-animals in op-core: the new animal fields through the routes, tags and
//! EIDs that name one animal, the head count that follows a herd's animals,
//! and the `list_animals` tool.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::tools::ToolScope;
use op_core::*;
use serde_json::{Value, json};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: Router,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let router = op_core::router().with_state(ctx.clone());
        let app = Self { _dir: dir, ctx, router };
        let (s, _) = app.call("POST", "/api/farm", Some(json!({"name": "Home", "center": [-93.62, 42.03]}))).await;
        assert_eq!(s, StatusCode::CREATED);
        app
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        let body = match body {
            Some(v) => {
                b = b.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let res = self.router.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn herd(&self, name: &str, count: u32) -> String {
        let (s, h) = self.call("POST", "/api/herds", Some(json!({"name": name, "species": "cattle", "count": count}))).await;
        assert_eq!(s, StatusCode::CREATED, "{h}");
        h["id"].as_str().unwrap().to_owned()
    }

    async fn count(&self, herd: &str) -> u32 {
        self.ctx.store().get_herd(herd).await.unwrap().unwrap().count
    }

    async fn animal(&self, herd: &str, tag: &str) -> String {
        let (s, a) = self.call("POST", "/api/animals", Some(json!({"tag": tag, "herd_id": herd}))).await;
        assert_eq!(s, StatusCode::CREATED, "{a}");
        a["id"].as_str().unwrap().to_owned()
    }
}

#[tokio::test]
async fn animal_fields_go_through_create_and_patch() {
    let app = App::new().await;
    let herd = app.herd("Cows", 30).await;
    let (s, a) = app
        .call(
            "POST",
            "/api/animals",
            Some(
                json!({"tag": " 214 ", "herd_id": herd, "eid": "982 000 123 456 789", "breed": " Angus ", "sex": "female", "born": "2022-04-01", "notes": ""}),
            ),
        )
        .await;
    assert_eq!(s, StatusCode::CREATED, "{a}");
    assert_eq!((a["tag"].as_str(), a["eid"].as_str(), a["breed"].as_str()), (Some("214"), Some("982000123456789"), Some("Angus")));
    assert_eq!((a["sex"].as_str(), a["born"].as_str()), (Some("female"), Some("2022-04-01")));
    assert!(a.get("notes").is_none(), "blank notes are left out");
    let id = a["id"].as_str().unwrap();
    let (_, got) = app.call("GET", &format!("/api/animals/{id}"), None).await;
    assert_eq!(got, a);

    let (s, p) = app.call("PATCH", &format!("/api/animals/{id}"), Some(json!({"breed": "Hereford", "sex": "castrated", "notes": "calm"}))).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    assert_eq!(
        (p["breed"].as_str(), p["sex"].as_str(), p["notes"].as_str(), p["eid"].as_str()),
        (Some("Hereford"), Some("castrated"), Some("calm"), Some("982000123456789"))
    );
    let (_, p) = app.call("PATCH", &format!("/api/animals/{id}"), Some(json!({"breed": null}))).await;
    assert!(p.get("breed").is_none());

    // Shapes that aren't an EID, a birth date or a sex.
    let (s, e) = app.call("POST", "/api/animals", Some(json!({"tag": "215", "herd_id": herd, "eid": "9.82E+14"}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("EID 9.82E+14 isn't 15 digits.")));
    let (s, e) = app.call("PATCH", &format!("/api/animals/{id}"), Some(json!({"born": "2999-01-01"}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("The birth date is in the future.")));
    let (s, _) = app.call("POST", "/api/animals", Some(json!({"tag": "215", "herd_id": herd, "sex": "heifer"}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = app.call("POST", "/api/animals", Some(json!({"tag": "   ", "herd_id": herd}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_tag_names_one_animal_per_herd_and_an_eid_one_animal() {
    let app = App::new().await;
    let cows = app.herd("Cows", 30).await;
    let heifers = app.herd("Heifers", 12).await;
    let (s, _) = app.call("POST", "/api/animals", Some(json!({"tag": "214", "herd_id": cows, "eid": "982000123456789"}))).await;
    assert_eq!(s, StatusCode::CREATED);

    let (s, e) = app.call("POST", "/api/animals", Some(json!({"tag": "214", "herd_id": cows}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::CONFLICT, Some("Tag 214 is already in this herd.")));
    // Another herd may use the tag.
    let other = app.animal(&heifers, "214").await;
    // Moving it into the first herd would make two.
    let (s, _) = app.call("PATCH", &format!("/api/animals/{other}"), Some(json!({"herd_id": cows}))).await;
    assert_eq!(s, StatusCode::CONFLICT);

    let (s, e) = app.call("POST", "/api/animals", Some(json!({"tag": "215", "herd_id": cows, "eid": "982-000123456789"}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::CONFLICT, Some("That EID is on 214.")));
    let (s, e) = app.call("PATCH", &format!("/api/animals/{other}"), Some(json!({"eid": "982000123456789"}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::CONFLICT, Some("That EID is on 214.")));

    // A removed animal frees its tag but keeps its EID.
    let first = app.ctx.store().list_animals(Some(&cows)).await.unwrap().remove(0);
    app.ctx.store().update_animal(&Animal { removed_at: Some(time::now()), removed_reason: Some(RemovedReason::Sold), ..first }).await.unwrap();
    let (s, _) = app.call("POST", "/api/animals", Some(json!({"tag": "214", "herd_id": cows}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, _) = app.call("POST", "/api/animals", Some(json!({"tag": "216", "herd_id": cows, "eid": "982000123456789"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);

    // Twins stored before the rule stay editable; only a changed tag is checked.
    let twin = Animal { id: id::new_id(id::ANIMAL), tag: "214".into(), herd_id: cows.clone(), ..Default::default() };
    app.ctx.store().insert_animal(&twin).await.unwrap();
    let (s, v) = app.call("PATCH", &format!("/api/animals/{}", twin.id), Some(json!({"name": "Dot"}))).await;
    assert_eq!((s, v["name"].as_str()), (StatusCode::OK, Some("Dot")));
}

#[tokio::test]
async fn the_head_count_follows_the_animals_once_there_are_any() {
    let app = App::new().await;
    let cows = app.herd("Cows", 30).await;
    let heifers = app.herd("Heifers", 12).await;
    let mut events = app.ctx.subscribe();

    // No animals yet: the count is the farmer's.
    let (s, h) = app.call("PATCH", &format!("/api/herds/{cows}"), Some(json!({"count": 40}))).await;
    assert_eq!((s, h["count"].as_u64()), (StatusCode::OK, Some(40)));

    let a = app.animal(&cows, "214").await;
    assert_eq!(app.count(&cows).await, 1);
    assert_eq!(events.recv().await.unwrap(), Event::AnimalsChanged { herd_id: Some(cows.clone()) });
    let b = app.animal(&cows, "215").await;
    assert_eq!(app.count(&cows).await, 2);

    // Now it isn't.
    let (s, e) = app.call("PATCH", &format!("/api/herds/{cows}"), Some(json!({"count": 250}))).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("Count follows the animals in this herd.")));
    // Sending the same count with other changes is fine.
    let (s, h) = app.call("PATCH", &format!("/api/herds/{cows}"), Some(json!({"count": 2, "name": "Cow herd"}))).await;
    assert_eq!((s, h["name"].as_str()), (StatusCode::OK, Some("Cow herd")));

    // Moving an animal moves the count; so does deleting one.
    let (s, _) = app.call("PATCH", &format!("/api/animals/{b}"), Some(json!({"herd_id": heifers}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!((app.count(&cows).await, app.count(&heifers).await), (1, 1));
    // A removed animal stops counting (op-import's remove route sets it; the store is enough here).
    let row = app.ctx.store().get_animal(&a).await.unwrap().unwrap();
    app.ctx.store().update_animal(&Animal { removed_at: Some(time::now()), removed_reason: Some(RemovedReason::Died), ..row }).await.unwrap();
    assert_eq!(animals::sync_herd_count(app.ctx.db(), &cows).await.unwrap(), Some(0));
    // Unchanged: nothing written.
    assert_eq!(animals::sync_herd_count(app.ctx.db(), &cows).await.unwrap(), None);
    let (s, _) = app.call("DELETE", &format!("/api/animals/{b}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    // Its last animal gone, Heifers has none: 0, and the count is the farmer's again.
    assert_eq!(app.count(&heifers).await, 0);
    let (s, _) = app.call("PATCH", &format!("/api/herds/{heifers}"), Some(json!({"count": 12}))).await;
    assert_eq!(s, StatusCode::OK);
}

/// (count, paddock) of the herd's history rows, oldest first.
async fn history(app: &App, herd: &str) -> Vec<(i64, Option<String>)> {
    sqlx::query_as("SELECT count, paddock_id FROM herd_history WHERE herd_id = ? ORDER BY at, id").bind(herd).fetch_all(app.ctx.db()).await.unwrap()
}

#[tokio::test]
async fn moving_the_last_animal_out_leaves_the_herd_at_zero() {
    // The training flow: new animals go into Training in P2, then all back to Cows.
    let app = App::new().await;
    let ring = json!([[[-93.62, 42.03], [-93.615, 42.03], [-93.615, 42.0336], [-93.62, 42.0336], [-93.62, 42.03]]]);
    let (_, p2) = app.call("POST", "/api/paddocks", Some(json!({"name": "P2", "geometry": {"type": "Polygon", "coordinates": ring}}))).await;
    let p2 = p2["id"].as_str().unwrap().to_owned();
    let cows = app.herd("Cows", 219).await;
    let (_, t) = app.call("POST", "/api/herds", Some(json!({"name": "Training", "species": "cattle", "count": 0, "paddock_id": p2}))).await;
    let training = t["id"].as_str().unwrap().to_owned();
    let mut ids = Vec::new();
    for tag in ["301", "302", "303"] {
        ids.push(app.animal(&training, tag).await);
    }
    assert_eq!(app.count(&training).await, 3);
    for id in &ids {
        let (s, v) = app.call("PATCH", &format!("/api/animals/{id}"), Some(json!({"herd_id": cows}))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
    // Cows follows its 3 animal rows; Training has none left and no phantom head.
    assert_eq!((app.count(&cows).await, app.count(&training).await), (3, 0));
    let p2 = Some(p2);
    assert_eq!(history(&app, &training).await, [(0, p2.clone()), (1, p2.clone()), (2, p2.clone()), (3, p2.clone()), (2, p2.clone()), (1, p2.clone()), (0, p2)]);
    // Once empty, the farmer sets its count again.
    let (s, _) = app.call("PATCH", &format!("/api/herds/{training}"), Some(json!({"count": 5}))).await;
    assert_eq!(s, StatusCode::OK);
}

#[tokio::test]
async fn a_removal_dated_earlier_takes_the_head_off_the_history_from_then() {
    let app = App::new().await;
    let cows = app.herd("Cows", 0).await;
    let (a, b) = (app.animal(&cows, "214").await, app.animal(&cows, "215").await);
    app.animal(&cows, "216").await;
    // Date the history: created Sep 1 with 0, the three animals Sep 1 (1, 2, 3 head).
    let rows: Vec<i64> = sqlx::query_scalar("SELECT id FROM herd_history WHERE herd_id = ? ORDER BY id").bind(&cows).fetch_all(app.ctx.db()).await.unwrap();
    assert_eq!(rows.len(), 4);
    for (id, t) in rows.iter().zip(["2026-09-01T12:00:00.000Z", "2026-09-01T12:00:00.001Z", "2026-09-01T12:00:00.002Z", "2026-09-01T12:00:00.003Z"]) {
        sqlx::query("UPDATE herd_history SET at = ? WHERE id = ?").bind(t).bind(id).execute(app.ctx.db()).await.unwrap();
    }
    sqlx::query("UPDATE animals SET created_at = '2026-09-01T12:00:00.000Z' WHERE herd_id = ?").bind(&cows).execute(app.ctx.db()).await.unwrap();
    // A move on Sep 10 (3 head), recorded as it happens.
    let ring = json!([[[-93.62, 42.03], [-93.615, 42.03], [-93.615, 42.0336], [-93.62, 42.0336], [-93.62, 42.03]]]);
    let (_, p2) = app.call("POST", "/api/paddocks", Some(json!({"name": "P2", "geometry": {"type": "Polygon", "coordinates": ring}}))).await;
    let p2 = p2["id"].as_str().unwrap().to_owned();
    let (s, _) = app.call("PATCH", &format!("/api/herds/{cows}"), Some(json!({"paddock_id": p2}))).await;
    assert_eq!(s, StatusCode::OK);
    sqlx::query("UPDATE herd_history SET at = '2026-09-10T12:00:00.000Z' WHERE herd_id = ? AND paddock_id = ?")
        .bind(&cows)
        .bind(&p2)
        .execute(app.ctx.db())
        .await
        .unwrap();

    // 214 sold Sep 5, entered now; 215 died Sep 12, entered now.
    let remove = |id: String, at: &'static str| {
        let ctx = app.ctx.clone();
        let cows = cows.clone();
        async move {
            let at = time::from_db(at).unwrap();
            let row = ctx.store().get_animal(&id).await.unwrap().unwrap();
            ctx.store().update_animal(&Animal { removed_at: Some(at), removed_reason: Some(RemovedReason::Sold), ..row }).await.unwrap();
            animals::count_removal(&ctx, &cows, &id, at).await.unwrap();
        }
    };
    remove(a, "2026-09-05T17:00:00.000Z").await;
    remove(b, "2026-09-12T17:00:00.000Z").await;
    assert_eq!(app.count(&cows).await, 1);
    let rows: Vec<(String, i64, Option<String>)> =
        sqlx::query_as("SELECT at, count, paddock_id FROM herd_history WHERE herd_id = ? ORDER BY at, id").bind(&cows).fetch_all(app.ctx.db()).await.unwrap();
    let rows: Vec<(&str, i64, bool)> = rows.iter().map(|(t, c, p)| (t.get(..10).unwrap(), *c, p.as_deref() == Some(p2.as_str()))).collect();
    let today = time::to_db(&time::now());
    let today = today.get(..10).unwrap();
    assert_eq!(
        rows,
        [
            ("2026-09-01", 0, false),
            ("2026-09-01", 1, false),
            ("2026-09-01", 2, false),
            ("2026-09-01", 3, false),
            // 214 left on Sep 5: 2 head from then, and at the Sep 10 move.
            ("2026-09-05", 2, false),
            ("2026-09-10", 2, true),
            // 215 left on Sep 12: 1 head from then.
            ("2026-09-12", 1, true),
            // The rows the count changes wrote when each was entered.
            (today, 1, true),
            (today, 1, true),
        ]
    );
    // 217 entered today and removed as of 2020: it never counted, so the
    // rows from its record on drop back to 1 and nothing earlier changes.
    let c = app.animal(&cows, "217").await;
    remove(c, "2020-01-01T00:00:00.000Z").await;
    let counts: Vec<i64> =
        sqlx::query_scalar("SELECT count FROM herd_history WHERE herd_id = ? ORDER BY at, id").bind(&cows).fetch_all(app.ctx.db()).await.unwrap();
    assert_eq!(counts, [0, 1, 2, 3, 2, 2, 1, 1, 1, 1, 1]);
}

#[tokio::test]
async fn two_removals_marked_before_either_is_counted_both_count_from_their_dates() {
    // Two removals racing: both animals are marked removed before either
    // one's count runs. Each still takes its head off the history from its date.
    let app = App::new().await;
    let cows = app.herd("Cows", 0).await;
    let (a, b) = (app.animal(&cows, "214").await, app.animal(&cows, "215").await);
    app.animal(&cows, "216").await;
    let rows: Vec<i64> = sqlx::query_scalar("SELECT id FROM herd_history WHERE herd_id = ? ORDER BY id").bind(&cows).fetch_all(app.ctx.db()).await.unwrap();
    for (i, id) in rows.iter().enumerate() {
        sqlx::query("UPDATE herd_history SET at = ? WHERE id = ?").bind(format!("2026-09-01T12:00:00.00{i}Z")).bind(id).execute(app.ctx.db()).await.unwrap();
    }
    sqlx::query("UPDATE animals SET created_at = '2026-09-01T12:00:00.000Z' WHERE herd_id = ?").bind(&cows).execute(app.ctx.db()).await.unwrap();

    let (at_a, at_b) = (time::from_db("2026-09-05T17:00:00.000Z").unwrap(), time::from_db("2026-09-12T17:00:00.000Z").unwrap());
    for (id, at) in [(&a, at_a), (&b, at_b)] {
        let row = app.ctx.store().get_animal(id).await.unwrap().unwrap();
        app.ctx.store().update_animal(&Animal { removed_at: Some(at), removed_reason: Some(RemovedReason::Sold), ..row }).await.unwrap();
    }
    animals::count_removal(&app.ctx, &cows, &a, at_a).await.unwrap();
    animals::count_removal(&app.ctx, &cows, &b, at_b).await.unwrap();
    assert_eq!(app.count(&cows).await, 1);
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT at, count FROM herd_history WHERE herd_id = ? ORDER BY at, id").bind(&cows).fetch_all(app.ctx.db()).await.unwrap();
    let rows: Vec<(&str, i64)> = rows.iter().map(|(t, c)| (t.get(..10).unwrap(), *c)).collect();
    let today = time::to_db(&time::now());
    let today = today.get(..10).unwrap();
    // 3 head from Sep 1, 2 from Sep 5 (214 sold), 1 from Sep 12 (215 sold),
    // and the two count changes as they were entered, both at 1 by now.
    assert_eq!(
        rows,
        [("2026-09-01", 0), ("2026-09-01", 1), ("2026-09-01", 2), ("2026-09-01", 3), ("2026-09-05", 2), ("2026-09-12", 1), (today, 1), (today, 1)]
    );
    // A third count for an animal already taken off changes nothing.
    animals::count_removal(&app.ctx, &cows, &b, at_b).await.unwrap();
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM herd_history WHERE herd_id = ?").bind(&cows).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!((n, app.count(&cows).await), (8, 1));
}

#[tokio::test]
async fn list_animals_shows_each_animal_with_its_collar() {
    let app = App::new().await;
    let cows = app.herd("Cows", 30).await;
    let heifers = app.herd("Heifers", 12).await;
    let a = app.animal(&cows, "214").await;
    app.animal(&cows, "215").await;
    app.animal(&heifers, "31").await;
    let t = time::now();
    sqlx::query(
        "INSERT INTO collars (id, name, herd_id, animal_id, state, created_at, battery, last_seen, last_fix, parked_reason, parked_at)
         VALUES ('col_1', 'C-0001', ?, ?, 'inside', ?, 0.8123, ?, ?, NULL, NULL)",
    )
    .bind(&cows)
    .bind(&a)
    .bind(time::to_db(&t))
    .bind(time::to_db(&t))
    .bind(json!({"at": time::to_db(&t), "point": [-93.621, 42.031], "accuracy_m": 2.0, "sats": 9}).to_string())
    .execute(app.ctx.db())
    .await
    .unwrap();
    sqlx::query("UPDATE animals SET collar_id = 'col_1' WHERE id = ?").bind(&a).execute(app.ctx.db()).await.unwrap();

    op_core::register_tools(&app.ctx);
    let viewer = Identity { role: Role::Viewer, user_id: None, name: None, via: Via::UserToken };
    assert!(app.ctx.tools().listed_for(&viewer, &ToolScope::Full).iter().any(|t| t.name == "list_animals"));
    let spec = app.ctx.tools().get("list_animals").unwrap();
    assert!(spec.read && !spec.brain);
    let call = |args: Value| {
        let ctx = app.ctx.clone();
        let viewer = viewer.clone();
        async move { ctx.tools().call(&ctx, "list_animals", args, None, viewer, &ToolScope::Full).await }
    };

    let v = call(json!({"herd_id": cows})).await.unwrap();
    assert_eq!(v["count"], 2);
    let first = &v["animals"][0];
    assert_eq!((first["tag"].as_str(), first["herd"].as_str()), (Some("214"), Some("Cows")));
    assert_eq!(first["collar"]["name"], "C-0001");
    assert_eq!(first["collar"]["battery"], 0.81);
    assert_eq!(first["collar"]["position"], json!([-93.621, 42.031]));
    assert_eq!(first["collar"]["state"], "inside");
    assert!(v["animals"][1].get("collar").is_none());

    assert_eq!(call(json!({})).await.unwrap()["count"], 3);
    assert_eq!(call(json!({"q": "21"})).await.unwrap()["count"], 2);
    assert_eq!(call(json!({"q": "31"})).await.unwrap()["animals"][0]["herd"], "Heifers");

    let row = app.ctx.store().get_animal(&a).await.unwrap().unwrap();
    app.ctx.store().update_animal(&Animal { removed_at: Some(time::now()), removed_reason: Some(RemovedReason::Sold), collar_id: None, ..row }).await.unwrap();
    assert_eq!(call(json!({"herd_id": cows})).await.unwrap()["count"], 1);
    let gone = call(json!({"herd_id": cows, "removed": true})).await.unwrap();
    assert_eq!((gone["count"].as_u64(), gone["animals"][0]["removed_reason"].as_str()), (Some(1), Some("sold")));

    assert_eq!(call(json!({"herd_id": "herd_nope"})).await.unwrap_err().status, StatusCode::BAD_REQUEST);
    assert_eq!(call(json!({"tag": "214"})).await.unwrap_err().status, StatusCode::BAD_REQUEST);
}
