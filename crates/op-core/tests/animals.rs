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
    // Heifers has no animal rows left, so its count is the farmer's again (left where it was).
    assert_eq!(app.count(&heifers).await, 1);
    let (s, _) = app.call("PATCH", &format!("/api/herds/{heifers}"), Some(json!({"count": 12}))).await;
    assert_eq!(s, StatusCode::OK);
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
