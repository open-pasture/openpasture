//! Paddock areas stored through the REST API are right whichever way the
//! GeoJSON rings wind.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::Ctx;
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
        Self { router: op_core::router().with_state(ctx), _dir: dir }
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

    /// Create a paddock and return its stored area, read back from the store.
    async fn area(&self, name: &str, rings: &[Vec<[f64; 2]>]) -> f64 {
        let (s, pad) = self.call("POST", "/api/paddocks", Some(json!({"name": name, "geometry": {"type": "Polygon", "coordinates": rings}}))).await;
        assert_eq!(s, StatusCode::CREATED, "{pad}");
        let (_, stored) = self.call("GET", &format!("/api/paddocks/{}", pad["id"].as_str().unwrap()), None).await;
        assert_eq!(stored["area_ha"], pad["area_ha"]);
        pad["area_ha"].as_f64().unwrap()
    }
}

/// The live-check paddock P1 near Ames, counter-clockwise, closed.
fn p1() -> Vec<[f64; 2]> {
    vec![[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]]
}

/// A pond inside P1, counter-clockwise, closed.
fn pond() -> Vec<[f64; 2]> {
    vec![[-93.6235, 42.0315], [-93.6215, 42.0315], [-93.6215, 42.0325], [-93.6235, 42.0325], [-93.6235, 42.0315]]
}

/// A small triangle near P1's south-west corner, counter-clockwise, closed.
fn barn() -> Vec<[f64; 2]> {
    vec![[-93.6245, 42.0305], [-93.624, 42.0305], [-93.6242, 42.031], [-93.6245, 42.0305]]
}

fn cw(ring: &[[f64; 2]]) -> Vec<[f64; 2]> {
    ring.iter().rev().copied().collect()
}

#[tokio::test]
async fn a_paddock_with_a_hole_stores_the_same_area_in_every_winding() {
    let app = App::new().await;
    let (s, _) = app.call("POST", "/api/farm", Some(json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]}))).await;
    assert_eq!(s, StatusCode::CREATED);

    let whole = app.area("Whole", &[p1()]).await;
    assert!((whole - 16.5).abs() < 0.3, "{whole}");
    assert_eq!(app.area("Whole cw", &[cw(&p1())]).await, whole);
    let pond_ha = app.area("Pond", &[pond()]).await;
    let barn_ha = app.area("Barn", &[barn()]).await;

    for (outer, hole, label) in [
        (p1(), pond(), "outer ccw, hole ccw"),
        (p1(), cw(&pond()), "outer ccw, hole cw"),
        (cw(&p1()), pond(), "outer cw, hole ccw"),
        (cw(&p1()), cw(&pond()), "outer cw, hole cw"),
    ] {
        let a = app.area(label, &[outer, hole]).await;
        assert!((a - (whole - pond_ha)).abs() <= 0.002, "{label}: {a} ha, want {}", whole - pond_ha);
    }

    for holes in [[pond(), barn()], [cw(&pond()), barn()], [pond(), cw(&barn())]] {
        let a = app.area("Two holes", &[cw(&p1()), holes[0].clone(), holes[1].clone()]).await;
        assert!((a - (whole - pond_ha - barn_ha)).abs() <= 0.003, "{a} ha, want {}", whole - pond_ha - barn_ha);
    }
}

#[tokio::test]
async fn patching_in_a_hole_of_either_winding_recomputes_the_area() {
    let app = App::new().await;
    app.call("POST", "/api/farm", Some(json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]}))).await;
    let (_, pad) = app.call("POST", "/api/paddocks", Some(json!({"name": "P1", "geometry": {"type": "Polygon", "coordinates": [p1()]}}))).await;
    let id = pad["id"].as_str().unwrap().to_owned();
    let whole = pad["area_ha"].as_f64().unwrap();
    let pond_ha = app.area("Pond", &[pond()]).await;

    for hole in [pond(), cw(&pond())] {
        let (s, patched) = app.call("PATCH", &format!("/api/paddocks/{id}"), Some(json!({"geometry": {"type": "Polygon", "coordinates": [p1(), hole]}}))).await;
        assert_eq!(s, StatusCode::OK, "{patched}");
        let a = patched["area_ha"].as_f64().unwrap();
        assert!((a - (whole - pond_ha)).abs() <= 0.002, "{a} ha, want {}", whole - pond_ha);
        let (_, stored) = app.call("GET", &format!("/api/paddocks/{id}"), None).await;
        assert_eq!(stored["area_ha"], patched["area_ha"]);
    }
}
