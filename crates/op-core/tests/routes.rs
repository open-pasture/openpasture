use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Via, with_identity};
use serde_json::{Value, json};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    router: axum::Router,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        Self { router: with_identity(op_core::router().with_state(ctx), Identity::owner(Via::Local)), _dir: dir }
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = self.router.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let value =
            if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into())) };
        (status, value)
    }
}

fn square() -> Value {
    json!({"type": "Polygon", "coordinates": [[[-92.41, 38.12], [-92.40, 38.12], [-92.40, 38.13], [-92.41, 38.13], [-92.41, 38.12]]]})
}

#[tokio::test]
async fn farm_paddock_herd_animal_flow() {
    let app = App::new().await;

    let (s, state) = app.call("GET", "/api/state", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(state["farm"], Value::Null);
    assert_eq!(state["herds"], json!([]));
    assert_eq!(state["settings"]["server"]["port"], 7878);

    // Paddocks need a farm first.
    let (s, err) = app.call("POST", "/api/paddocks", Some(json!({"name": "North", "geometry": square()}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"], "Create the farm first.");

    // The zone comes from the centre, not the body (a browser in Denver setting up a Missouri farm).
    let (s, farm) = app.call("POST", "/api/farm", Some(json!({"name": "Home", "timezone": "America/Denver", "center": [-92.4, 38.12]}))).await;
    assert_eq!(s, StatusCode::CREATED);
    assert_eq!(farm["timezone"], "America/Chicago");
    assert!(farm["id"].as_str().unwrap().starts_with("farm_"));
    let (s, _) = app.call("POST", "/api/farm", Some(json!({"name": "Again", "timezone": "UTC", "center": [0, 0]}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
    let (s, farm) = app.call("PATCH", "/api/farm", Some(json!({"name": "Hill Farm", "id": "nope"}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(farm["name"], "Hill Farm");
    assert_eq!(farm["timezone"], "America/Chicago");
    let (_, farm) = app.call("PATCH", "/api/farm", Some(json!({"center": [-79.2, 38.2]}))).await;
    assert_eq!(farm["timezone"], "America/New_York");
    assert!(farm["id"].as_str().unwrap().starts_with("farm_"));

    let (s, pad) = app.call("POST", "/api/paddocks", Some(json!({"name": "North", "geometry": square()}))).await;
    assert_eq!(s, StatusCode::CREATED, "{pad}");
    assert_eq!(pad["status"], "resting");
    assert!((pad["area_ha"].as_f64().unwrap() - 97.0).abs() < 1.5);
    assert!(pad.get("notes").is_none());
    let pad_id = pad["id"].as_str().unwrap().to_owned();

    let bowtie = json!({"type": "Polygon", "coordinates": [[[-92.41, 38.12], [-92.40, 38.13], [-92.40, 38.12], [-92.41, 38.13], [-92.41, 38.12]]]});
    let (s, err) = app.call("POST", "/api/paddocks", Some(json!({"name": "Bad", "geometry": bowtie}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"], "The shape crosses itself.");

    let (s, pad) = app.call("PATCH", &format!("/api/paddocks/{pad_id}"), Some(json!({"status": "grazing", "notes": "wet corner"}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(pad["status"], "grazing");
    assert_eq!(pad["notes"], "wet corner");
    let (_, pad) = app.call("PATCH", &format!("/api/paddocks/{pad_id}"), Some(json!({"notes": null}))).await;
    assert!(pad.get("notes").is_none());

    let (s, herd) = app.call("POST", "/api/herds", Some(json!({"name": "Cows", "species": "cattle", "count": 30, "paddock_id": pad_id}))).await;
    assert_eq!(s, StatusCode::CREATED, "{herd}");
    assert_eq!(herd["autonomy"], "propose");
    assert_eq!(herd["timer_minutes"], 60);
    let herd_id = herd["id"].as_str().unwrap().to_owned();

    // A herd placed in a resting paddock marks it grazing.
    let (_, south) = app.call("POST", "/api/paddocks", Some(json!({"name": "South", "geometry": square()}))).await;
    assert_eq!(south["status"], "resting");
    let (s, h2) = app.call("POST", "/api/herds", Some(json!({"name": "Sheep", "species": "sheep", "count": 5, "paddock_id": south["id"]}))).await;
    assert_eq!(s, StatusCode::CREATED, "{h2}");
    let (_, south) = app.call("GET", &format!("/api/paddocks/{}", south["id"].as_str().unwrap()), None).await;
    assert_eq!(south["status"], "grazing");
    app.call("DELETE", &format!("/api/herds/{}", h2["id"].as_str().unwrap()), None).await;
    app.call("DELETE", &format!("/api/paddocks/{}", south["id"].as_str().unwrap()), None).await;

    let (s, _) = app.call("POST", "/api/herds", Some(json!({"name": "X", "species": "llamas", "count": 1}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
    let (s, _) = app.call("POST", "/api/herds", Some(json!({"name": "X", "species": "sheep", "count": 1, "paddock_id": "pad_nope"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (s, herd) = app.call("PATCH", &format!("/api/herds/{herd_id}"), Some(json!({"autonomy": "timer", "timer_minutes": 15}))).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(herd["autonomy"], "timer");

    let (s, animal) = app.call("POST", "/api/animals", Some(json!({"tag": "A1", "herd_id": herd_id}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let animal_id = animal["id"].as_str().unwrap().to_owned();
    let (s, _) = app.call("POST", "/api/animals", Some(json!({"tag": "A2", "herd_id": herd_id, "collar_id": "col_nope"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (_, animals) = app.call("GET", &format!("/api/animals?herd_id={herd_id}"), None).await;
    assert_eq!(animals.as_array().unwrap().len(), 1);
    let (_, animal) = app.call("PATCH", &format!("/api/animals/{animal_id}"), Some(json!({"name": "Daisy"}))).await;
    assert_eq!(animal["name"], "Daisy");

    let (_, state) = app.call("GET", "/api/state", None).await;
    assert_eq!(state["farm"]["name"], "Hill Farm");
    assert_eq!(state["paddocks"].as_array().unwrap().len(), 1);
    assert_eq!(state["herds"].as_array().unwrap().len(), 1);

    let (s, _) = app.call("DELETE", &format!("/api/animals/{animal_id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = app.call("DELETE", &format!("/api/herds/{herd_id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = app.call("DELETE", &format!("/api/paddocks/{pad_id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, err) = app.call("DELETE", &format!("/api/paddocks/{pad_id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    assert!(err["error"].is_string());
}

#[tokio::test]
async fn settings_and_secrets() {
    let app = App::new().await;
    let (_, s0) = app.call("GET", "/api/settings", None).await;
    let token = s0["server"]["app_token"].as_str().unwrap().to_owned();

    let (s, s1) = app.call("PUT", "/api/settings", Some(json!({"brain": {"id": "claude"}, "decision_time": "05:30"}))).await;
    assert_eq!(s, StatusCode::OK, "{s1}");
    assert_eq!(s1["brain"]["id"], "claude");
    assert_eq!(s1["decision_time"], "05:30");
    assert_eq!(s1["server"]["app_token"], token);

    let (s, err) = app.call("PUT", "/api/settings", Some(json!({"decision_time": "25:00"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(err["error"], "decision_time must be HH:MM.");
    let (s, _) = app.call("PUT", "/api/settings", Some(json!({"brain": {"id": "skynet"}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    let (_, list) = app.call("GET", "/api/secrets", None).await;
    assert!(list.as_array().unwrap().iter().all(|x| x["set"] == false && x.get("value").is_none()));
    let (s, _) = app.call("PUT", "/api/secrets/anthropic_api_key", Some(json!({"value": "sk-ant-xyz"}))).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (_, list) = app.call("GET", "/api/secrets", None).await;
    assert!(list.as_array().unwrap().iter().any(|x| x["name"] == "anthropic_api_key" && x["set"] == true));
    assert!(!list.to_string().contains("sk-ant-xyz"));
    let (s, _) = app.call("DELETE", "/api/secrets/anthropic_api_key", None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, _) = app.call("PUT", "/api/secrets/BAD", Some(json!({"value": "x"}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}
