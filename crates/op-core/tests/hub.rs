//! HUB foundations: migrations, identity, users, the tool and brief
//! registries, features, messages, units, place phrases, notify channels and
//! the new record columns. Collar report behaviour (outside_since, parked
//! collars, collar_boundary_state) is tested in op-ingest's `tests/hub.rs`.

use std::sync::atomic::{AtomicUsize, Ordering};

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{Duration, NaiveDate};
use http_body_util::BodyExt;
use op_core::alert::{Alert, AlertStatus};
use op_core::brief::{BriefLine, BriefRegistry};
use op_core::features::{FeatureGeometry, FeatureKind, NewFeature, exclusions_for, insert_feature, list_features};
use op_core::messages::{self, Inbound, Outbound};
use op_core::tools::{ToolCall, ToolRegistry, ToolScope, ToolSpec};
use op_core::units::{Fmt, Units, units_for_timezone};
use op_core::users::{self, NewUser, UserPatch, normalize_phone};
use op_core::*;
use serde_json::{Value, json};
use tower::ServiceExt;

async fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    (dir, ctx)
}

async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut b = Request::builder().method(method).uri(path);
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// About `side` metres square, `east` metres east of the farm centre, as a ring.
fn ring(east: f64, north: f64, side: f64) -> Vec<LonLat> {
    let p = op_geo::Projection::new([-93.62, 42.03]);
    vec![p.offset(east, north), p.offset(east + side, north), p.offset(east + side, north + side), p.offset(east, north + side), p.offset(east, north)]
}

fn poly(east: f64, north: f64, side: f64) -> Polygon {
    Polygon::from_ring(ring(east, north, side))
}

/// Farm plus paddocks P1 (0..400 m) and P2 (1000..1400 m east).
async fn farm(ctx: &Ctx) -> (Paddock, Paddock) {
    let app = op_core::router().with_state(ctx.clone());
    let (s, _) = call(&app, "POST", "/api/farm", Some(json!({"name": "Home", "center": [-93.62, 42.03]}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let (s, p1) = call(&app, "POST", "/api/paddocks", Some(json!({"name": "P1", "geometry": poly(0.0, 0.0, 400.0)}))).await;
    assert_eq!(s, StatusCode::CREATED);
    let (_, p2) = call(&app, "POST", "/api/paddocks", Some(json!({"name": "P2", "geometry": poly(1000.0, 0.0, 400.0)}))).await;
    (serde_json::from_value(p1).unwrap(), serde_json::from_value(p2).unwrap())
}

// Migrations

#[tokio::test]
async fn migrations_have_unique_prefixes_and_run_on_a_fresh_db() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("migrations");
    let mut prefixes = std::collections::BTreeMap::new();
    for e in std::fs::read_dir(&dir).unwrap() {
        let name = e.unwrap().file_name().into_string().unwrap();
        assert!(name.ends_with(".sql"), "{name}");
        let (prefix, rest) = name.split_at(4);
        assert!(prefix.bytes().all(|b| b.is_ascii_digit()), "{name} needs a 4-digit prefix");
        assert!(rest.starts_with('_'), "{name}");
        assert!(prefixes.insert(prefix.to_owned(), name.clone()).is_none(), "prefix {prefix} is used twice");
    }
    assert_eq!(store::MIGRATOR.iter().count(), prefixes.len());
    let (_dir, ctx) = ctx().await;
    let applied: Vec<(i64,)> = sqlx::query_as("SELECT version FROM _sqlx_migrations WHERE success = 1 ORDER BY version").fetch_all(ctx.db()).await.unwrap();
    let want: Vec<i64> = prefixes.keys().map(|p| p.parse().unwrap()).collect();
    assert_eq!(applied.into_iter().map(|(v,)| v).collect::<Vec<_>>(), want);
    for table in ["users", "features", "messages", "collar_boundary_state", "imported_paddock_days", "paddock_days"] {
        let n: (i64,) = sqlx::query_as(&format!("SELECT COUNT(*) FROM {table}")).fetch_one(ctx.db()).await.unwrap();
        assert_eq!(n.0, 0, "{table}");
    }
}

// Identity

#[test]
fn roles_are_ordered_and_anonymous_can_nothing() {
    assert!(Role::Viewer < Role::Hand && Role::Hand < Role::Manager && Role::Manager < Role::Owner);
    assert_eq!(serde_json::to_value(Role::Manager).unwrap(), "manager");
    assert_eq!(Role::from_db("hand").unwrap(), Role::Hand);
    assert_eq!(serde_json::to_value(Via::AppToken).unwrap(), "app_token");

    let anon = Identity::anonymous();
    for r in [Role::Viewer, Role::Hand, Role::Manager, Role::Owner] {
        assert!(!anon.can(r));
    }
    assert_eq!(anon.require(Role::Viewer).unwrap_err().status, StatusCode::UNAUTHORIZED);

    let owner = Identity::owner(Via::Local);
    assert!(owner.can(Role::Owner));
    let brain = Identity::brain();
    assert!(brain.can(Role::Viewer) && !brain.can(Role::Hand));
    let e = brain.require(Role::Manager).unwrap_err();
    assert_eq!(e.status, StatusCode::FORBIDDEN);
    assert_eq!(e.message, "Your role can't do this.");
    assert_eq!(Identity::system(), Identity::owner(Via::System));

    let hand = Identity { role: Role::Hand, user_id: Some("usr_1".into()), name: Some("Sam".into()), via: Via::Text };
    assert_eq!(hand.actor(), Actor { via: Via::Text, user_id: Some("usr_1".into()), name: Some("Sam".into()) });
    assert_eq!(serde_json::to_value(Identity::owner(Via::Local).actor()).unwrap(), json!({"via": "local"}));
    assert_eq!(ApiError::forbidden("no").status, StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn identity_extractor_fails_loudly_without_the_guard() {
    let (_dir, ctx) = ctx().await;
    let bare = op_core::router().with_state(ctx.clone());
    let (s, v) = call(&bare, "GET", "/api/me", None).await;
    assert_eq!(s, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(v, json!({"error": "Identity missing"}));

    let app = with_identity(op_core::router().with_state(ctx.clone()), Identity::owner(Via::Local));
    let (s, v) = call(&app, "GET", "/api/me", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(v, json!({"role": "owner", "via": "local"}));

    // A person's identity comes back with the person.
    let u = users::create_user(&ctx, NewUser { name: "Sam".into(), role: Role::Hand, phone: Some("515 555 0123".into()), email: None }).await.unwrap();
    let id = Identity { role: Role::Hand, user_id: Some(u.id.clone()), name: Some(u.name.clone()), via: Via::UserToken };
    let app = with_identity(op_core::router().with_state(ctx.clone()), id);
    let (_, v) = call(&app, "GET", "/api/me", None).await;
    assert_eq!(v, json!({"role": "hand", "via": "user_token", "user": {"id": u.id, "name": "Sam", "phone": "+15155550123", "phone_verified": false}}));
}

// Users

#[test]
fn phone_numbers_normalize_to_e164() {
    assert_eq!(normalize_phone("515-555-0123").as_deref(), Some("+15155550123"));
    assert_eq!(normalize_phone("(515) 555 0123").as_deref(), Some("+15155550123"));
    assert_eq!(normalize_phone("1 515 555 0123").as_deref(), Some("+15155550123"));
    assert_eq!(normalize_phone("+1 515.555.0123").as_deref(), Some("+15155550123"));
    assert_eq!(normalize_phone("+44 20 7946 0958").as_deref(), Some("+442079460958"));
    assert_eq!(normalize_phone("0044 20 7946 0958").as_deref(), Some("+442079460958"));
    for bad in ["", "555-0123", "+0 515 555 0123", "515555012a", "+1234567", "+1234567890123456", "2 515 555 0123"] {
        assert_eq!(normalize_phone(bad), None, "{bad}");
    }
}

#[tokio::test]
async fn users_read_and_write() {
    let (_dir, ctx) = ctx().await;
    let new = |name: &str, role, phone: Option<&str>, email: Option<&str>| NewUser {
        name: name.into(),
        role,
        phone: phone.map(str::to_owned),
        email: email.map(str::to_owned),
    };
    let cody = users::create_user(&ctx, new("Cody", Role::Owner, Some("5155550123"), Some("cody@example.com"))).await.unwrap();
    assert!(cody.id.starts_with("usr_"));
    assert_eq!(cody.phone.as_deref(), Some("+15155550123"));
    let sam = users::create_user(&ctx, new("sam", Role::Hand, Some("+1 515 555 0199"), None)).await.unwrap();
    let ann = users::create_user(&ctx, new("Ann", Role::Viewer, None, None)).await.unwrap();

    // Duplicates and bad input.
    let e = users::create_user(&ctx, new("Other", Role::Hand, Some("(515) 555-0123"), None)).await.unwrap_err();
    assert_eq!(e.status, StatusCode::CONFLICT);
    assert!(e.message.contains("Cody"), "{}", e.message);
    let e = users::create_user(&ctx, new("Other", Role::Hand, None, Some("CODY@example.com"))).await.unwrap_err();
    assert_eq!(e.status, StatusCode::CONFLICT);
    assert_eq!(users::create_user(&ctx, new("Other", Role::Hand, Some("123"), None)).await.unwrap_err().status, StatusCode::BAD_REQUEST);
    assert_eq!(users::create_user(&ctx, new("Other", Role::Hand, None, Some("nope"))).await.unwrap_err().status, StatusCode::BAD_REQUEST);
    assert_eq!(users::create_user(&ctx, new("  ", Role::Hand, None, None)).await.unwrap_err().status, StatusCode::BAD_REQUEST);

    // Enabled first, then by name.
    let names = |us: Vec<users::User>| us.into_iter().map(|u| u.name).collect::<Vec<_>>();
    assert_eq!(names(users::list_users(&ctx).await.unwrap()), ["Ann", "Cody", "sam"]);
    users::update_user(&ctx, &ann.id, UserPatch { disabled: Some(true), ..Default::default() }).await.unwrap();
    assert_eq!(names(users::list_users(&ctx).await.unwrap()), ["Cody", "sam", "Ann"]);
    assert_eq!(users::get_user(&ctx, &cody.id).await.unwrap().unwrap(), cody);
    assert!(users::get_user(&ctx, "usr_nope").await.unwrap().is_none());

    // By phone: any format, enabled only.
    assert_eq!(users::user_by_phone(&ctx, "+15155550199").await.unwrap().unwrap().id, sam.id);
    assert_eq!(users::user_by_phone(&ctx, "515.555.0199").await.unwrap().unwrap().id, sam.id);
    users::update_user(&ctx, &sam.id, UserPatch { disabled: Some(true), ..Default::default() }).await.unwrap();
    assert!(users::user_by_phone(&ctx, "+15155550199").await.unwrap().is_none());
    let sam = users::update_user(&ctx, &sam.id, UserPatch { disabled: Some(false), ..Default::default() }).await.unwrap();
    assert!(sam.disabled_at.is_none());

    // Verification holds until the phone changes.
    let at = time::now();
    users::set_phone_verified(&ctx, &sam.id, at).await.unwrap();
    assert_eq!(users::get_user(&ctx, &sam.id).await.unwrap().unwrap().phone_verified_at, Some(at));
    let same = users::update_user(&ctx, &sam.id, UserPatch { phone: Some(Some("515-555-0199".into())), name: Some("Sam".into()), ..Default::default() })
        .await
        .unwrap();
    assert_eq!(same.phone_verified_at, Some(at), "same number, still verified");
    assert_eq!(same.name, "Sam");
    let moved = users::update_user(&ctx, &sam.id, UserPatch { phone: Some(Some("515-555-0200".into())), ..Default::default() }).await.unwrap();
    assert_eq!(moved.phone.as_deref(), Some("+15155550200"));
    assert_eq!(moved.phone_verified_at, None);
    assert_eq!(users::get_user(&ctx, &sam.id).await.unwrap().unwrap(), moved);
    let e = users::update_user(&ctx, &sam.id, UserPatch { phone: Some(Some("+15155550123".into())), ..Default::default() }).await.unwrap_err();
    assert_eq!(e.status, StatusCode::CONFLICT);
    // null clears; absent keeps.
    let patch: UserPatch = serde_json::from_value(json!({"phone": null, "role": "manager"})).unwrap();
    let cleared = users::update_user(&ctx, &sam.id, patch).await.unwrap();
    assert_eq!((cleared.phone, cleared.role), (None, Role::Manager));
    let patch: UserPatch = serde_json::from_value(json!({"name": "Samuel"})).unwrap();
    assert_eq!(patch.phone, None);
    assert!(users::set_phone_verified(&ctx, &sam.id, at).await.is_err(), "no phone to verify");
    assert_eq!(users::update_user(&ctx, "usr_nope", UserPatch::default()).await.unwrap_err().status, StatusCode::NOT_FOUND);
}

// Tool registry

fn spec(name: &'static str, read: bool, brain: bool, min_role: Role) -> ToolSpec {
    ToolSpec {
        name,
        description: "A test tool.",
        input_schema: json!({"type": "object", "properties": {}, "additionalProperties": false}),
        read,
        brain,
        min_role,
        run: ToolSpec::run_fn(move |c: ToolCall| async move {
            Ok(json!({"tool": name, "args": c.args, "client": c.client, "via": c.identity.via, "role": c.identity.role}))
        }),
    }
}

fn names(specs: Vec<ToolSpec>) -> Vec<&'static str> {
    specs.into_iter().map(|t| t.name).collect()
}

#[test]
#[should_panic(expected = "registered twice")]
fn registry_refuses_a_duplicate_name() {
    let r = ToolRegistry::default();
    r.register(spec("get_farm", true, true, Role::Viewer));
    r.register(spec("get_farm", true, false, Role::Viewer));
}

#[tokio::test]
async fn registry_lists_scopes_and_calls() {
    let (_dir, ctx) = ctx().await;
    let r = ctx.tools();
    r.register(spec("get_farm", true, true, Role::Viewer));
    r.register(spec("list_alerts", true, false, Role::Viewer));
    r.register(spec("ack_alert", false, false, Role::Hand));
    r.register(spec("propose_boundary", false, false, Role::Manager));
    r.register(spec("run_sql", true, true, Role::Viewer));
    assert!(r.has("ack_alert") && !r.has("nope"));
    assert_eq!(names(r.list()), ["get_farm", "list_alerts", "ack_alert", "propose_boundary", "run_sql"]);
    assert_eq!(r.brain_tools(), ["get_farm", "run_sql"]);

    // register_once: a group registers once.
    let made = AtomicUsize::new(0);
    for _ in 0..3 {
        r.register_once("test-group", || {
            made.fetch_add(1, Ordering::SeqCst);
            vec![spec("get_fleet", true, false, Role::Viewer)]
        });
    }
    assert_eq!(made.load(Ordering::SeqCst), 1);
    assert!(r.has("get_fleet"));

    let as_role = |role| Identity { role, user_id: None, name: None, via: Via::UserToken };
    assert_eq!(names(r.listed_for(&as_role(Role::Viewer), &ToolScope::Full)), ["get_farm", "list_alerts", "run_sql", "get_fleet"]);
    assert_eq!(names(r.listed_for(&as_role(Role::Hand), &ToolScope::Full)), ["get_farm", "list_alerts", "ack_alert", "run_sql", "get_fleet"]);
    assert_eq!(r.listed_for(&as_role(Role::Manager), &ToolScope::Full).len(), 6);
    assert!(r.listed_for(&Identity::anonymous(), &ToolScope::Full).is_empty());
    // Only: those names, read tools only.
    let only = ToolScope::Only(vec!["get_farm".into(), "propose_boundary".into(), "nope".into()]);
    assert_eq!(names(r.listed_for(&Identity::owner(Via::Local), &only)), ["get_farm"]);
    assert_eq!(names(r.listed_for(&Identity::brain(), &ToolScope::Only(r.brain_tools()))), ["get_farm", "run_sql"]);

    // Calls.
    let owner = Identity::owner(Via::AppToken);
    let v = r.call(&ctx, "get_farm", json!({"x": 1}), Some("agent".into()), owner.clone(), &ToolScope::Full).await.unwrap();
    assert_eq!(v, json!({"tool": "get_farm", "args": {"x": 1}, "client": "agent", "via": "app_token", "role": "owner"}));
    let e = r.call(&ctx, "nope", json!({}), None, owner.clone(), &ToolScope::Full).await.unwrap_err();
    assert_eq!((e.status, e.message.as_str()), (StatusCode::BAD_REQUEST, "Unknown tool nope."));
    let e = r.call(&ctx, "propose_boundary", json!({}), None, owner.clone(), &only).await.unwrap_err();
    assert_eq!(e.status, StatusCode::BAD_REQUEST, "outside the scope");
    let e = r.call(&ctx, "ack_alert", json!({}), None, as_role(Role::Viewer), &ToolScope::Full).await.unwrap_err();
    assert_eq!(e.status, StatusCode::FORBIDDEN);
    assert!(r.call(&ctx, "ack_alert", json!({}), None, as_role(Role::Hand), &ToolScope::Full).await.is_ok());
    let e = r.call(&ctx, "get_farm", json!({}), None, Identity::anonymous(), &ToolScope::Full).await.unwrap_err();
    assert_eq!(e.status, StatusCode::UNAUTHORIZED);

    // A runner: read tools minus the excluded, as the given identity.
    let runner = ctx.tool_runner(as_role(Role::Hand), &["run_sql"]);
    let listed: Vec<String> = runner.tools().into_iter().map(|t| t.name).collect();
    assert_eq!(listed, ["get_farm", "list_alerts", "get_fleet"]);
    assert_eq!(runner.tools()[0].input_schema["additionalProperties"], false);
    assert_eq!(runner.call("list_alerts", json!({})).await.unwrap()["role"], "hand");
    assert!(runner.call("run_sql", json!({})).await.unwrap_err().contains("Unknown tool"));
    assert!(runner.call("ack_alert", json!({})).await.is_err(), "writes are never in a runner");

    // Brain tokens carry their allowlist.
    let token = ctx.mint_brain_token(std::time::Duration::from_secs(60), r.brain_tools());
    assert_eq!(ctx.check_brain_token(token.as_str()), Some(vec!["get_farm".to_string(), "run_sql".to_string()]));
}

// Brief registry

fn line(name: &'static str, order: u32, text: &'static str, fail: bool) -> BriefLine {
    BriefLine {
        name,
        order,
        run: BriefLine::run_fn(move |_ctx, herd_id, _at| async move { if fail { anyhow::bail!("broken") } else { Ok(vec![format!("{text} {herd_id}")]) } }),
    }
}

#[tokio::test]
async fn brief_lines_run_in_order() {
    let (_dir, ctx) = ctx().await;
    let r = ctx.brief_lines();
    r.register(line("attention", 50, "Battery low", false));
    r.register(line("schedule", 20, "Strip 4 of 12", false));
    r.register(line("broken", 30, "", true));
    r.register(line("late", 50, "Later", false));
    assert_eq!(r.list().iter().map(|l| l.name).collect::<Vec<_>>(), ["schedule", "broken", "attention", "late"]);
    assert!(r.has("schedule"));
    assert_eq!(r.collect(&ctx, "herd_1", time::now()).await, ["Strip 4 of 12 herd_1", "Battery low herd_1", "Later herd_1"]);
    let dup = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let r = BriefRegistry::default();
        r.register(line("a", 1, "", false));
        r.register(line("a", 2, "", false));
    }));
    assert!(dup.is_err());
}

// Features

fn feature(kind: FeatureKind, geometry: FeatureGeometry, paddock_id: Option<&str>) -> NewFeature {
    NewFeature { kind, name: None, geometry, paddock_id: paddock_id.map(str::to_owned), notes: None, props: Value::Null, active_from: None, active_until: None }
}

#[tokio::test]
async fn features_insert_per_kind_and_filter_by_time() {
    let (_dir, ctx) = ctx().await;
    let (p1, p2) = farm(&ctx).await;
    let pt = FeatureGeometry::Point([-93.619, 42.031]);
    let line = FeatureGeometry::LineString(vec![[-93.62, 42.03], [-93.61, 42.03]]);
    let area = FeatureGeometry::Polygon(vec![ring(100.0, 100.0, 50.0)]);

    // Right geometry per kind.
    for (kind, g) in [
        (FeatureKind::Exclusion, &area),
        (FeatureKind::Water, &pt),
        (FeatureKind::Water, &area),
        (FeatureKind::Gate, &pt),
        (FeatureKind::Shade, &area),
        (FeatureKind::Hazard, &area),
        (FeatureKind::Road, &line),
        (FeatureKind::NeighbourLine, &line),
        (FeatureKind::FarmBoundary, &area),
    ] {
        let f = insert_feature(&ctx, feature(kind, g.clone(), None)).await.unwrap_or_else(|e| panic!("{kind:?}: {e}"));
        assert!(f.id.starts_with("fea_"));
        assert_eq!(f.props, json!({}));
    }
    // Wrong geometry per kind.
    for (kind, g) in [(FeatureKind::Exclusion, &pt), (FeatureKind::Gate, &area), (FeatureKind::Road, &pt), (FeatureKind::FarmBoundary, &line)] {
        let e = insert_feature(&ctx, feature(kind, g.clone(), None)).await.unwrap_err();
        assert_eq!(e.status, StatusCode::BAD_REQUEST, "{kind:?}");
    }
    let e = insert_feature(&ctx, feature(FeatureKind::Hazard, pt.clone(), None)).await.unwrap_err();
    assert_eq!(e.message, "A hazard point needs radius_m.");
    let mut hazard = feature(FeatureKind::Hazard, pt.clone(), None);
    hazard.props = json!({"radius_m": 15.0});
    insert_feature(&ctx, hazard).await.unwrap();
    let holed = FeatureGeometry::Polygon(vec![ring(0.0, 0.0, 100.0), ring(40.0, 40.0, 10.0)]);
    assert_eq!(insert_feature(&ctx, feature(FeatureKind::Exclusion, holed, None)).await.unwrap_err().status, StatusCode::BAD_REQUEST);
    assert_eq!(insert_feature(&ctx, feature(FeatureKind::Water, pt.clone(), Some("pad_nope"))).await.unwrap_err().status, StatusCode::BAD_REQUEST);
    // One farm boundary.
    let e = insert_feature(&ctx, feature(FeatureKind::FarmBoundary, area.clone(), None)).await.unwrap_err();
    assert_eq!(e.status, StatusCode::CONFLICT);

    // Active windows: current, future, expired.
    let now = time::now();
    let window = |from: Option<i64>, until: Option<i64>, paddock: &str| NewFeature {
        active_from: from.map(|h| now + Duration::hours(h)),
        active_until: until.map(|h| now + Duration::hours(h)),
        name: Some("wet spot".into()),
        ..feature(FeatureKind::Exclusion, FeatureGeometry::Polygon(vec![ring(50.0, 50.0, 60.0)]), Some(paddock))
    };
    let current = insert_feature(&ctx, window(Some(-1), Some(1), &p1.id)).await.unwrap();
    let future = insert_feature(&ctx, window(Some(2), None, &p1.id)).await.unwrap();
    let expired = insert_feature(&ctx, window(None, Some(-1), &p1.id)).await.unwrap();
    let far = insert_feature(&ctx, window(None, None, &p2.id)).await.unwrap();
    assert_eq!(insert_feature(&ctx, window(Some(2), Some(1), &p1.id)).await.unwrap_err().status, StatusCode::BAD_REQUEST);

    let ids = |fs: Vec<features::MapFeature>| fs.into_iter().map(|f| f.id).collect::<Vec<_>>();
    let in_p1 = ids(list_features(&ctx, Some(FeatureKind::Exclusion), Some(&p1.id), None).await.unwrap());
    assert_eq!(in_p1, [current.id.clone(), future.id.clone(), expired.id.clone()]);
    assert_eq!(ids(list_features(&ctx, Some(FeatureKind::Exclusion), Some(&p1.id), Some(now)).await.unwrap()), [current.id.clone()]);
    assert_eq!(ids(list_features(&ctx, None, Some(&p1.id), Some(now + Duration::hours(3))).await.unwrap()), [future.id.clone()]);
    assert_eq!(list_features(&ctx, None, None, None).await.unwrap().len(), 14);
    assert_eq!(list_features(&ctx, Some(FeatureKind::Gate), None, Some(now)).await.unwrap().len(), 1);
    assert!(current.active_at(now) && !future.active_at(now) && !expired.active_at(now));

    // Exclusions for a boundary inside P1: farm-wide ones plus P1's active
    // ones, not P2's.
    let farm_wide = list_features(&ctx, Some(FeatureKind::Exclusion), None, None).await.unwrap().into_iter().find(|f| f.paddock_id.is_none()).unwrap();
    let got = ids(exclusions_for(&ctx, &poly(10.0, 10.0, 200.0), now).await.unwrap());
    assert_eq!(got, [farm_wide.id.clone(), current.id.clone()]);
    let got = ids(exclusions_for(&ctx, &poly(1100.0, 10.0, 100.0), now + Duration::hours(3)).await.unwrap());
    assert_eq!(got, [farm_wide.id.clone(), far.id.clone()]);

    // Round trip, and a deleted paddock takes its features with it.
    assert_eq!(features::get_feature(&ctx, &current.id).await.unwrap().unwrap(), current);
    assert!(ctx.store().delete_paddock(&p1.id).await.unwrap());
    assert!(features::get_feature(&ctx, &current.id).await.unwrap().is_none());
    assert!(features::get_feature(&ctx, &far.id).await.unwrap().is_some());
}

// Messages

fn out(key: &str, channel: &str, to: &str) -> Outbound {
    Outbound { idempotency_key: key.into(), channel: channel.into(), to: to.into(), text: "214 outside P3".into(), kind: "alert".into(), ..Default::default() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn messages_queue_claim_mark_and_inbound() {
    let (_dir, ctx) = ctx().await;
    let mut rx = ctx.subscribe();
    let first = messages::enqueue(&ctx, out("alert:1:usr_1", "sms", "+15155550123")).await.unwrap();
    assert!(first.id.starts_with("ntf_"));
    assert_eq!((first.status.as_str(), first.direction.as_str(), first.attempts), ("queued", "out", 0));
    let again = messages::enqueue(&ctx, out("alert:1:usr_1", "sms", "+15155550123")).await.unwrap();
    assert_eq!(again, first, "same key, same row");
    match rx.try_recv().unwrap() {
        Event::Message { message } => assert_eq!(message.id, first.id),
        other => panic!("{other:?}"),
    }
    assert!(rx.try_recv().is_err(), "a duplicate enqueue publishes nothing");
    let mut email = out("brief:1", "email", "cody@example.com");
    email.subject = Some("Morning brief".into());
    let email = messages::enqueue(&ctx, email).await.unwrap();
    assert_eq!(email.subject.as_deref(), Some("Morning brief"));

    // Claims: only the asked channels, each row once, even when racing.
    for i in 0..20 {
        messages::enqueue(&ctx, out(&format!("k{i}"), "sms", "+15155550123")).await.unwrap();
    }
    let now = time::now();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let ctx = ctx.clone();
            tokio::spawn(async move { messages::claim(&ctx, &["sms"], now, 10).await.unwrap() })
        })
        .collect();
    let mut claimed = Vec::new();
    for h in handles {
        claimed.extend(h.await.unwrap());
    }
    assert_eq!(claimed.len(), 21);
    let mut ids: Vec<_> = claimed.iter().map(|m| m.id.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 21, "no row claimed twice");
    assert!(claimed.iter().all(|m| m.status == "sending" && m.attempts == 1 && m.channel == "sms"));
    assert!(messages::claim(&ctx, &["sms"], now, 10).await.unwrap().is_empty());
    assert!(messages::claim(&ctx, &[], now, 10).await.unwrap().is_empty());

    // Mark sent; retry later; fail.
    messages::mark(&ctx, &first.id, "sent", Some("SM123"), None, None).await.unwrap();
    let m = messages::get_message(&ctx, &first.id).await.unwrap().unwrap();
    assert_eq!((m.status.as_str(), m.provider_id.as_deref()), ("sent", Some("SM123")));
    messages::mark(&ctx, &first.id, "delivered", None, None, None).await.unwrap();
    assert_eq!(messages::get_message(&ctx, &first.id).await.unwrap().unwrap().provider_id.as_deref(), Some("SM123"), "kept");
    let retry = &claimed.iter().find(|m| m.id != first.id).unwrap().id;
    messages::mark(&ctx, retry, "queued", None, Some("Twilio 503"), Some(now + Duration::seconds(30))).await.unwrap();
    assert!(messages::claim(&ctx, &["sms"], now, 10).await.unwrap().is_empty(), "not due yet");
    let due = messages::claim(&ctx, &["sms"], now + Duration::seconds(31), 10).await.unwrap();
    assert_eq!((due.len(), due[0].attempts, due[0].error.as_deref()), (1, 2, Some("Twilio 503")));
    assert!(messages::mark(&ctx, retry, "bogus", None, None, None).await.is_err());
    assert!(messages::mark(&ctx, "ntf_nope", "sent", None, None, None).await.is_err());
    let claimed_email = messages::claim(&ctx, &["email", "webhook"], now, 10).await.unwrap();
    assert_eq!(claimed_email.iter().map(|m| m.id.clone()).collect::<Vec<_>>(), [email.id.clone()]);

    // Inbound, deduped per channel on the provider's id.
    let text = |sid: &str, channel: &str| Inbound {
        channel: channel.into(),
        from: "+15155550123".into(),
        text: "Y 4821".into(),
        provider_id: Some(sid.into()),
        at: now,
    };
    let got = messages::record_inbound(&ctx, text("SM9", "sms"), Some("usr_1"), "received").await.unwrap().unwrap();
    assert_eq!((got.direction.as_str(), got.kind.as_str(), got.user_id.as_deref(), got.created_at), ("in", "inbound", Some("usr_1"), now));
    assert!(messages::record_inbound(&ctx, text("SM9", "sms"), None, "received").await.unwrap().is_none());
    assert!(messages::record_inbound(&ctx, text("SM9", "whatsapp"), None, "ignored").await.unwrap().is_some());
    let no_sid = Inbound { provider_id: None, ..text("", "sms") };
    assert!(messages::record_inbound(&ctx, no_sid.clone(), None, "received").await.unwrap().is_some());
    assert!(messages::record_inbound(&ctx, no_sid, None, "received").await.unwrap().is_some());

    // Last text out: sent ones only.
    assert!(messages::last_out_to(&ctx, "+15155550123").await.unwrap().is_some());
    assert!(messages::last_out_to(&ctx, "+19999999999").await.unwrap().is_none());
    messages::enqueue(&ctx, out("q", "sms", "+15155550100")).await.unwrap();
    assert!(messages::last_out_to(&ctx, "+15155550100").await.unwrap().is_none(), "queued isn't sent");
}

// Units

#[test]
fn units_format_in_both_systems() {
    let m = Fmt::new(Units::Metric);
    let i = Fmt::new(Units::Imperial);
    assert_eq!((m.area(12.4), i.area(12.4)), ("12.4 ha".into(), "30.6 ac".into()));
    assert_eq!((m.area(412.0), i.area(0.1), m.area(0.03), m.area(1500.0)), ("412.0 ha".into(), "0.2 ac".into(), "0.03 ha".into(), "1,500 ha".into()));
    assert_eq!((m.len(60.0), i.len(60.0), i.len(5.0)), ("60 m".into(), "200 ft".into(), "16 ft".into()));
    assert_eq!((m.len(1504.0), i.len(1000.0)), ("1,500 m".into(), "3,280 ft".into()));
    assert_eq!((m.height(10.0), i.height(10.0), i.height(7.5)), ("10 cm".into(), "4 in".into(), "3 in".into()));
    assert_eq!((m.mass(545.0), i.mass(545.0), m.mass(12.4)), ("545 kg".into(), "1,200 lb".into(), "12 kg".into()));
    assert_eq!((m.per_head(496.0), i.per_head(496.0)), ("496 m²/hd".into(), "5,340 ft²/hd".into()));
    assert_eq!((m.density(250.0, 12.4), i.density(250.0, 12.4)), ("20.2 AU/ha".into(), "8.2 AU/ac".into()));
    assert_eq!((m.len(-12.0), m.area(0.0)), ("-12 m".into(), "0.0 ha".into()));
    assert_eq!((m.unit_label("area"), i.unit_label("area"), i.unit_label("head")), ("ha", "ac", ""));
    assert!((i.convert("area", 1.0) - 2.4710538).abs() < 1e-6);
    assert_eq!(m.convert("area", 1.0), 1.0);

    for tz in ["America/Chicago", "America/New_York", "America/Denver", "America/Phoenix", "America/Los_Angeles", "America/Anchorage"] {
        assert_eq!(units_for_timezone(tz), Units::Imperial, "{tz}");
    }
    for tz in ["Pacific/Honolulu", "America/Puerto_Rico", "America/Indiana/Knox", "US/Central"] {
        assert_eq!(units_for_timezone(tz), Units::Imperial, "{tz}");
    }
    for tz in ["Europe/London", "America/Toronto", "America/Mexico_City", "Australia/Sydney", "UTC", ""] {
        assert_eq!(units_for_timezone(tz), Units::Metric, "{tz}");
    }
}

#[tokio::test]
async fn units_follow_the_farm_setting() {
    let (_dir, ctx) = ctx().await;
    assert_eq!(Fmt::of(&ctx).await.unwrap().units, Units::Metric);
    ctx.update_settings(&json!({"units": "imperial"})).await.unwrap();
    assert_eq!(Fmt::of(&ctx).await.unwrap().area(1.0), "2.5 ac");
}

// Place phrases

#[tokio::test]
async fn place_phrases_name_the_paddock() {
    let (_dir, ctx) = ctx().await;
    assert_eq!(place::describe(&ctx, [-93.62, 42.03]).await.unwrap(), None, "no paddocks");
    farm(&ctx).await;
    let p = op_geo::Projection::new([-93.62, 42.03]);
    assert_eq!(place::describe(&ctx, p.offset(200.0, 200.0)).await.unwrap().as_deref(), Some("in P1"));
    assert_eq!(place::describe(&ctx, p.offset(200.0, 460.0)).await.unwrap().as_deref(), Some("60 m N of P1"));
    assert_eq!(place::describe(&ctx, p.offset(650.0, 200.0)).await.unwrap().as_deref(), Some("250 m E of P1"));
    assert_eq!(place::describe(&ctx, p.offset(1450.0, -50.0)).await.unwrap().as_deref(), Some("71 m SE of P2"));
    ctx.update_settings(&json!({"units": "imperial"})).await.unwrap();
    assert_eq!(place::describe(&ctx, p.offset(200.0, 460.0)).await.unwrap().as_deref(), Some("200 ft N of P1"));
}

// Notify channels

#[tokio::test]
async fn configured_channels_need_settings_and_secrets() {
    let (_dir, ctx) = ctx().await;
    assert!(notify_config::configured_channels(&ctx).await.unwrap().is_empty());
    ctx.store()
        .set_setting_json(
            notify_config::CHANNELS_KEY,
            &json!({
                "sms": { "from": "+15155550100" },
                "whatsapp": { "from": "" },
                "email": { "host": "smtp.example.com", "port": 587, "user": "farm", "from": "farm@example.com" },
                "webhook": { "url": "https://example.com/hook" },
                "relay": { "enabled": true }
            }),
        )
        .await
        .unwrap();
    assert!(notify_config::configured_channels(&ctx).await.unwrap().is_empty(), "no secrets yet");
    ctx.secrets().set("twilio_account_sid", "AC1").unwrap();
    assert!(notify_config::configured_channels(&ctx).await.unwrap().is_empty(), "both Twilio secrets");
    ctx.secrets().set("twilio_auth_token", "tok").unwrap();
    ctx.secrets().set("webhook_secret", "whsec").unwrap();
    ctx.secrets().set("hosted_api_key", "oph_x").unwrap();
    assert_eq!(notify_config::configured_channels(&ctx).await.unwrap(), ["sms", "webhook", "relay"]);
    ctx.secrets().set("smtp_password", "pw").unwrap();
    assert_eq!(notify_config::configured_channels(&ctx).await.unwrap(), ["sms", "email", "webhook", "relay"]);
    ctx.store()
        .set_setting_json(
            notify_config::CHANNELS_KEY,
            &json!({ "whatsapp": { "from": "+15155550101" }, "email": { "host": "h", "from": "f" }, "relay": { "enabled": false } }),
        )
        .await
        .unwrap();
    assert_eq!(notify_config::configured_channels(&ctx).await.unwrap(), ["whatsapp", "email"]);
}

// Records: new columns round-trip

#[tokio::test]
async fn new_columns_round_trip_through_mappers_and_patches() {
    let (_dir, ctx) = ctx().await;
    let (p1, _) = farm(&ctx).await;
    let app = with_identity(op_core::router().with_state(ctx.clone()), Identity::owner(Via::Local));
    let (_, herd) = call(&app, "POST", "/api/herds", Some(json!({"name": "Cows", "species": "cattle", "count": 3, "paddock_id": p1.id}))).await;
    let herd_id = herd["id"].as_str().unwrap().to_owned();

    // Paddock props: stored, kept by other patches, patchable.
    let mut pad = ctx.store().get_paddock(&p1.id).await.unwrap().unwrap();
    assert!(pad.props.is_empty());
    assert!(serde_json::to_value(&pad).unwrap().get("props").is_none(), "empty props are left out");
    pad.props.insert("fsa_farm".into(), json!("1234"));
    ctx.store().update_paddock(&pad).await.unwrap();
    let (_, v) = call(&app, "PATCH", &format!("/api/paddocks/{}", p1.id), Some(json!({"notes": "creek side"}))).await;
    assert_eq!(v["props"], json!({"fsa_farm": "1234"}));
    let (_, v) = call(&app, "PATCH", &format!("/api/paddocks/{}", p1.id), Some(json!({"props": {"fsa_tract": "567"}}))).await;
    assert_eq!(v["props"], json!({"fsa_farm": "1234", "fsa_tract": "567"}));
    assert_eq!(serde_json::to_value(ctx.store().get_paddock(&p1.id).await.unwrap().unwrap()).unwrap()["props"], v["props"]);

    // Animal fields.
    let animal = Animal {
        id: id::new_id(id::ANIMAL),
        tag: "214".into(),
        herd_id: herd_id.clone(),
        eid: Some("982000123456789".into()),
        breed: Some("Angus".into()),
        sex: Some(Sex::Female),
        born: Some(NaiveDate::from_ymd_opt(2022, 4, 1).unwrap()),
        notes: Some("calm".into()),
        ..Default::default()
    };
    ctx.store().insert_animal(&animal).await.unwrap();
    assert_eq!(ctx.store().get_animal(&animal.id).await.unwrap().unwrap(), animal);
    let (s, v) = call(&app, "PATCH", &format!("/api/animals/{}", animal.id), Some(json!({"name": "Daisy", "removed_at": "2026-09-01T00:00:00Z"}))).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["eid"], "982000123456789");
    assert_eq!(v["born"], "2022-04-01");
    assert_eq!(v["sex"], "female");
    assert!(v.get("removed_at").is_none(), "removal has its own route");
    let (_, v) = call(&app, "PATCH", &format!("/api/animals/{}", animal.id), Some(json!({"breed": "Hereford", "sex": "castrated"}))).await;
    assert_eq!((v["breed"].as_str(), v["name"].as_str(), v["notes"].as_str()), (Some("Hereford"), Some("Daisy"), Some("calm")));
    let removed =
        Animal { removed_at: Some(time::now()), removed_reason: Some(RemovedReason::MovedOff), ..ctx.store().get_animal(&animal.id).await.unwrap().unwrap() };
    ctx.store().update_animal(&removed).await.unwrap();
    let back = ctx.store().get_animal(&animal.id).await.unwrap().unwrap();
    assert_eq!(back, removed);
    assert_eq!(serde_json::to_value(&back).unwrap()["removed_reason"], "moved_off");
    // A second animal with the same EID is refused by the database.
    let twin = Animal { id: id::new_id(id::ANIMAL), tag: "215".into(), herd_id: herd_id.clone(), eid: animal.eid.clone(), ..Default::default() };
    assert!(ctx.store().insert_animal(&twin).await.is_err());

    // Collar columns (written by op-ingest).
    let t = time::now();
    sqlx::query(
        "INSERT INTO collars (id, name, herd_id, state, created_at, fw, caps, limits, outside_since, parked_at, parked_reason)
         VALUES ('col_1', 'Collar 1', ?, 'outside', ?, '0.2.0', '[\"holes\",\"slots\"]', '{\"outer\":128}', ?, ?, 'charging')",
    )
    .bind(&herd_id)
    .bind(time::to_db(&t))
    .bind(time::to_db(&t))
    .bind(time::to_db(&t))
    .execute(ctx.db())
    .await
    .unwrap();
    let row = sqlx::query("SELECT * FROM collars WHERE id = 'col_1'").fetch_one(ctx.db()).await.unwrap();
    let c = store::collar_from_row(&row).unwrap();
    assert_eq!(c.fw.as_deref(), Some("0.2.0"));
    assert_eq!(c.caps, ["holes", "slots"]);
    assert_eq!((c.outside_since, c.parked_at, c.parked_reason), (Some(t), Some(t), Some(ParkReason::Charging)));
    let v = serde_json::to_value(&c).unwrap();
    assert!(v.get("limits").is_none(), "limits stay off the struct");
    assert_eq!(v["parked_reason"], "charging");
    let bare = Collar { id: "col_2".into(), name: "C".into(), herd_id: herd_id.clone(), ..Default::default() };
    assert_eq!(serde_json::to_value(&bare).unwrap(), json!({"id": "col_2", "name": "C", "herd_id": herd_id, "state": "unknown"}));

    // Boundary status and alerts serialize without the empty new fields.
    let status = BoundaryStatus::default();
    assert_eq!(serde_json::to_value(&status).unwrap(), json!({"acks": []}));
    let alert = Alert {
        id: "alr_1".into(),
        kind: "outside".into(),
        key: "outside:col_1".into(),
        severity: Severity::Warning,
        status: AlertStatus::Open,
        herd_id: Some(herd_id.clone()),
        title: "214 outside P1".into(),
        body: None,
        at: Some([-93.62, 42.03]),
        targets: vec![("collar".into(), "col_1".into())],
        data: json!({}),
        opened_at: t,
        updated_at: t,
        acked_at: None,
        acked_by: None,
        resolved_at: None,
        resolved_by: None,
        rolled_into: None,
    };
    let v = serde_json::to_value(&alert).unwrap();
    assert_eq!(v["severity"], "warning");
    assert_eq!(v["targets"], json!([["collar", "col_1"]]));
    assert_eq!(serde_json::from_value::<Alert>(v).unwrap(), alert);
}

// Events

#[test]
fn events_carry_their_audience() {
    let t = time::from_db("2026-09-26T12:00:00.000Z").unwrap();
    let cue = Event::Cue { collar_id: "col_1".into(), at: t, level: 2, margin_m: 1.5, kind: None, ring: None };
    assert_eq!(
        serde_json::to_value(&cue).unwrap(),
        json!({"type": "cue", "collar_id": "col_1", "at": "2026-09-26T12:00:00Z", "level": 2, "margin_m": 1.5}),
        "old shape when kind and ring are unknown"
    );
    let cue = Event::Cue { collar_id: "col_1".into(), at: t, level: 2, margin_m: -0.5, kind: Some("outside".into()), ring: Some(1) };
    assert_eq!(serde_json::to_value(&cue).unwrap()["kind"], "outside");
    let msg = Event::Message { message: alert::MessageLog { id: "ntf_1".into(), ..Default::default() } };
    assert_eq!(msg.min_role(), Role::Manager);
    assert_eq!(serde_json::to_value(&msg).unwrap()["type"], "message");
    assert_eq!(cue.min_role(), Role::Viewer);
    let animals = Event::AnimalsChanged { herd_id: None };
    assert_eq!(serde_json::to_value(&animals).unwrap(), json!({"type": "animals_changed"}));
    assert_eq!(animals.min_role(), Role::Viewer);
    let f = features::MapFeature {
        id: "fea_1".into(),
        kind: FeatureKind::Gate,
        name: Some("east gate".into()),
        geometry: FeatureGeometry::Point([-93.62, 42.03]),
        paddock_id: None,
        notes: None,
        props: json!({}),
        active_from: None,
        active_until: None,
        created_at: t,
        updated_at: t,
    };
    let v = serde_json::to_value(Event::Feature { feature: f.clone(), deleted: false }).unwrap();
    assert!(v.get("deleted").is_none());
    assert_eq!(v["feature"]["geometry"], json!({"type": "Point", "coordinates": [-93.62, 42.03]}));
    assert_eq!(v["feature"]["kind"], "gate");
    assert_eq!(serde_json::to_value(Event::Feature { feature: f, deleted: true }).unwrap()["deleted"], true);
    assert_eq!(serde_json::to_value(FeatureKind::NeighbourLine).unwrap(), "neighbour_line");
}
