//! A farm's units follow its time zone when it is created: US zones read
//! imperial, the rest metric. Later changes to the farm leave them alone.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Ctx, Identity, Via};
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
        // The owner on this machine, as op-server's guard would resolve it.
        let router = op_core::with_identity(op_core::router().with_state(ctx), Identity::owner(Via::Local));
        Self { router, _dir: dir }
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
        (status, if bytes.is_empty() { Value::Null } else { serde_json::from_slice(&bytes).unwrap() })
    }

    async fn create_farm(&self, center: [f64; 2], timezone: &str) -> Value {
        let (s, farm) = self.call("POST", "/api/farm", Some(json!({ "name": "Test farm", "timezone": timezone, "center": center }))).await;
        assert_eq!(s, StatusCode::CREATED, "{farm}");
        farm
    }

    async fn units(&self) -> String {
        let (s, settings) = self.call("GET", "/api/settings", None).await;
        assert_eq!(s, StatusCode::OK);
        settings["units"].as_str().unwrap().to_owned()
    }
}

#[tokio::test]
async fn a_farm_in_america_chicago_starts_imperial() {
    let app = App::new().await;
    assert_eq!(app.units().await, "metric");
    let farm = app.create_farm([-93.62, 42.03], "America/Chicago").await;
    assert_eq!(farm["timezone"], "America/Chicago");
    assert_eq!(app.units().await, "imperial");
    let (_, state) = app.call("GET", "/api/state", None).await;
    assert_eq!(state["settings"]["units"], "imperial");
}

#[tokio::test]
async fn a_farm_in_europe_london_starts_metric() {
    let app = App::new().await;
    let farm = app.create_farm([-1.26, 51.75], "Europe/London").await;
    assert_eq!(farm["timezone"], "Europe/London");
    assert_eq!(app.units().await, "metric");
}

#[tokio::test]
async fn the_zone_comes_from_where_the_farm_is_not_the_browser() {
    // Set up from a browser in London, the farm is in Iowa.
    let app = App::new().await;
    let farm = app.create_farm([-93.62, 42.03], "Europe/London").await;
    assert_eq!(farm["timezone"], "America/Chicago");
    assert_eq!(app.units().await, "imperial");
}

#[tokio::test]
async fn units_chosen_later_stay_when_the_farm_moves() {
    let app = App::new().await;
    app.create_farm([-93.62, 42.03], "America/Chicago").await;
    let (s, _) = app.call("PUT", "/api/settings", Some(json!({ "units": "metric" }))).await;
    assert_eq!(s, StatusCode::OK);
    // Moving the farm's centre to England changes its zone, not its units.
    let (s, farm) = app.call("PATCH", "/api/farm", Some(json!({ "center": [-1.26, 51.75] }))).await;
    assert_eq!(s, StatusCode::OK, "{farm}");
    assert_eq!(farm["timezone"], "Europe/London");
    assert_eq!(app.units().await, "metric");
    let (s, _) = app.call("PUT", "/api/settings", Some(json!({ "units": "imperial" }))).await;
    assert_eq!(s, StatusCode::OK);
    app.call("PATCH", "/api/farm", Some(json!({ "center": [-93.62, 42.03] }))).await;
    assert_eq!(app.units().await, "imperial");
}
