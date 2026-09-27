//! Paddock areas stored before op-geo measured rings whichever way they wind
//! are measured again when the data dir opens: the paddock row and its
//! geometry history, once, and never a row that is already right.

use op_core::Ctx;
use op_core::area_repair::{Repaired, repair_paddock_areas};
use serde_json::{Value, json};

/// §6's P1 near Ames, counter-clockwise.
const P1: [[f64; 2]; 5] = [[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]];
/// A pond inside P1, counter-clockwise too (the winding the old code got wrong for a hole).
const POND: [[f64; 2]; 5] = [[-93.6235, 42.031], [-93.6215, 42.031], [-93.6215, 42.0326], [-93.6235, 42.0326], [-93.6235, 42.031]];

fn cw(ring: &[[f64; 2]]) -> Vec<[f64; 2]> {
    ring.iter().rev().copied().collect()
}

fn geometry(rings: Vec<Vec<[f64; 2]>>) -> Value {
    json!({ "type": "Polygon", "coordinates": rings })
}

fn measured(g: &Value) -> f64 {
    let p: op_geo::Polygon = serde_json::from_value(g.clone()).unwrap();
    (p.area_ha() * 1000.0).round() / 1000.0
}

async fn insert(ctx: &Ctx, id: &str, g: &Value, area_ha: f64) {
    sqlx::query("INSERT INTO paddocks (id, name, geometry, area_ha, status, created_at) VALUES (?, ?, ?, ?, 'resting', '2026-09-01T00:00:00.000Z')")
        .bind(id)
        .bind(id)
        .bind(g.to_string())
        .bind(area_ha)
        .execute(ctx.db())
        .await
        .unwrap();
}

async fn area(ctx: &Ctx, id: &str) -> f64 {
    sqlx::query_scalar("SELECT area_ha FROM paddocks WHERE id = ?").bind(id).fetch_one(ctx.db()).await.unwrap()
}

async fn history(ctx: &Ctx, id: &str) -> Vec<f64> {
    sqlx::query_scalar("SELECT area_ha FROM paddock_geometry_history WHERE paddock_id = ? ORDER BY id").bind(id).fetch_all(ctx.db()).await.unwrap()
}

#[tokio::test]
async fn areas_stored_by_the_old_measure_are_right_after_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let clockwise = geometry(vec![cw(&P1)]);
    let holed = geometry(vec![P1.to_vec(), POND.to_vec()]);
    let right = geometry(vec![P1.to_vec(), cw(&POND)]);
    {
        let ctx = Ctx::open(dir.path()).await.unwrap();
        // What the old op-geo stored: the rest of the Earth for a clockwise outer
        // ring, less than nothing for a hole wound like its outer ring.
        insert(&ctx, "pad_cw", &clockwise, 51_006_562_155.4).await;
        insert(&ctx, "pad_hole", &holed, -51_006_562_154.013).await;
        insert(&ctx, "pad_right", &right, measured(&right)).await;
        insert(&ctx, "pad_stale", &geometry(vec![P1.to_vec()]), 12.0).await;
        // A reshape the old code measured wrong: the history keeps both shapes.
        sqlx::query("UPDATE paddocks SET geometry = ?, area_ha = ? WHERE id = 'pad_right'")
            .bind(holed.to_string())
            .bind(-51_006_562_154.013)
            .execute(ctx.db())
            .await
            .unwrap();
        assert_eq!(history(&ctx, "pad_right").await.len(), 2, "the trigger kept the reshape");
    }

    // The next start measures them again.
    let ctx = Ctx::open(dir.path()).await.unwrap();
    assert_eq!(area(&ctx, "pad_cw").await, 16.556, "clockwise P1 is P1");
    let with_pond = measured(&holed);
    assert_eq!(with_pond, 13.613, "P1 less a 2.9 ha pond");
    assert_eq!(area(&ctx, "pad_hole").await, with_pond);
    assert_eq!(area(&ctx, "pad_right").await, with_pond);
    assert_eq!(area(&ctx, "pad_stale").await, 16.556);
    assert_eq!(history(&ctx, "pad_cw").await, [16.556]);
    assert_eq!(history(&ctx, "pad_hole").await, [with_pond]);
    assert_eq!(history(&ctx, "pad_right").await, [with_pond, with_pond], "the shape it had, then the one it has");
    // Fixing the area adds no history row (only shape and name changes do).
    assert_eq!(history(&ctx, "pad_stale").await.len(), 1);

    // Once right, nothing is rewritten.
    assert_eq!(repair_paddock_areas(ctx.db()).await.unwrap(), Repaired::default());
    drop(ctx);
    let ctx = Ctx::open(dir.path()).await.unwrap();
    assert_eq!(repair_paddock_areas(ctx.db()).await.unwrap(), Repaired::default());
}

#[tokio::test]
async fn a_repair_pass_counts_what_it_rewrote_and_skips_what_it_cant_read() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    insert(&ctx, "pad_cw", &geometry(vec![cw(&P1)]), 51_006_562_155.4).await;
    insert(&ctx, "pad_ok", &geometry(vec![P1.to_vec()]), 16.556).await;
    // Not a polygon op-geo can read: left as it is, the rest still repaired.
    insert(&ctx, "pad_junk", &json!({ "type": "Point", "coordinates": [0.0, 0.0] }), 3.0).await;
    let n = repair_paddock_areas(ctx.db()).await.unwrap();
    assert_eq!(n, Repaired { paddocks: 1, history: 1 });
    assert_eq!(area(&ctx, "pad_cw").await, 16.556);
    assert_eq!(area(&ctx, "pad_junk").await, 3.0);
    // Paddocks created through the API are right from the start.
    let routes = op_core::with_identity(op_core::router().with_state(ctx.clone()), op_core::Identity::owner(op_core::Via::Local));
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/farm")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]}).to_string()))
        .unwrap();
    use tower::ServiceExt;
    assert!(routes.clone().oneshot(req).await.unwrap().status().is_success());
    let body = json!({ "name": "P9", "geometry": geometry(vec![cw(&P1), POND.to_vec()]) }).to_string();
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/api/paddocks")
        .header("content-type", "application/json")
        .body(axum::body::Body::from(body))
        .unwrap();
    assert!(routes.oneshot(req).await.unwrap().status().is_success());
    assert_eq!(repair_paddock_areas(ctx.db()).await.unwrap(), Repaired::default());
}
