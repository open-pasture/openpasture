//! People, sign-in links and per-person tokens (op-core side): the routes as
//! each role sees them, links accepted once, tokens resolved through the cache
//! and evicted at once, `last_used` at most once a minute, the owner person,
//! and the app token redacted for everyone but the owner.

use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::people::{self, NewInvite, SessionToken};
use op_core::users::NewUser;
use op_core::{Actor, Ctx, Identity, Role, Via, with_identity};
use serde_json::{Value, json};
use tower::ServiceExt;

async fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    (dir, ctx)
}

fn owner() -> Identity {
    Identity::owner(Via::Local)
}

fn as_role(role: Role) -> Identity {
    Identity { role, user_id: Some(format!("usr_{role:?}")), name: Some(format!("{role:?}")), via: Via::UserToken }
}

fn app(ctx: &Ctx, id: Identity) -> Router {
    with_identity(op_core::router().with_state(ctx.clone()), id)
}

async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

async fn count(ctx: &Ctx, sql: &str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(ctx.db()).await.unwrap()
}

async fn person(ctx: &Ctx, name: &str, role: Role) -> String {
    people::add_person(ctx, NewUser { name: name.into(), role, phone: None, email: None }).await.unwrap().user.id
}

/// A sign-in link for the person, accepted: their token.
async fn signed_in(ctx: &Ctx, user_id: &str) -> String {
    let (_, code) = people::create_invite(ctx, NewInvite { user_id: Some(user_id.into()), ..Default::default() }, &owner().actor()).await.unwrap();
    people::accept_invite(ctx, &code, None).await.unwrap().token
}

#[tokio::test]
async fn adding_a_person_gives_no_sign_in() {
    let (_dir, ctx) = ctx().await;
    let app = app(&ctx, owner());
    let (s, p) = call(&app, "POST", "/api/users", Some(json!({"name": "Luis", "role": "hand", "phone": "515 555 0123"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{p}");
    assert_eq!((p["name"].as_str(), p["role"].as_str(), p["phone"].as_str()), (Some("Luis"), Some("hand"), Some("+15155550123")));
    assert_eq!(p["tokens"], 0);
    assert!(p.get("invite_until").is_none());
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM user_tokens").await, 0);
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM invites").await, 0);

    let (s, list) = call(&app, "GET", "/api/users", None).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(list.as_array().unwrap().len(), 1);
    // Same phone twice is a conflict.
    let (s, _) = call(&app, "POST", "/api/users", Some(json!({"name": "Other", "role": "viewer", "phone": "+15155550123"}))).await;
    assert_eq!(s, StatusCode::CONFLICT);
}

#[tokio::test]
async fn people_routes_are_the_owners() {
    let (_dir, ctx) = ctx().await;
    for role in [Role::Viewer, Role::Hand, Role::Manager] {
        let app = app(&ctx, as_role(role));
        for (m, p, b) in [
            ("GET", "/api/users", None),
            ("POST", "/api/users", Some(json!({"name": "X", "role": "owner"}))),
            ("GET", "/api/invites", None),
            ("POST", "/api/invites", Some(json!({"name": "X", "role": "owner"}))),
            ("GET", "/api/tokens", None),
        ] {
            assert_eq!(call(&app, m, p, b).await.0, StatusCode::FORBIDDEN, "{role:?} {m} {p}");
        }
    }
    let anon = app(&ctx, Identity::anonymous());
    assert_eq!(call(&anon, "GET", "/api/users", None).await.0, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn a_link_is_accepted_once_for_a_token() {
    let (_dir, ctx) = ctx().await;
    let app = app(&ctx, owner());
    let (s, inv) = call(&app, "POST", "/api/invites", Some(json!({"name": "Ana", "role": "manager", "email": "ana@example.com"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{inv}");
    let code = inv["code"].as_str().unwrap().to_owned();
    assert_eq!(code.len(), 32, "128 bits in hex");
    assert!(inv["url"].as_str().unwrap().ends_with(&format!("/#/join/{code}")));
    assert_eq!(inv["created_by"]["via"], "local");
    let user_id = inv["user_id"].as_str().unwrap().to_owned();
    // The person exists from now, with an open link.
    let (_, p) = call(&app, "GET", &format!("/api/users/{user_id}"), None).await;
    assert_eq!((p["name"].as_str(), p["tokens"].as_u64()), (Some("Ana"), Some(0)));
    assert!(p["invite_until"].is_string());
    // Only the hash is stored.
    assert_eq!(count(&ctx, &format!("SELECT COUNT(*) FROM invites WHERE code_hash = '{code}'")).await, 0);

    let anon = app_for_anyone(&ctx);
    let (s, acc) = call(&anon, "POST", "/api/invites/accept", Some(json!({"code": code}))).await;
    assert_eq!(s, StatusCode::OK, "{acc}");
    let token = acc["token"].as_str().unwrap();
    assert!(people::is_token_shape(token), "{token}");
    assert_eq!(acc["user"]["id"], user_id);
    let session = people::session_for_token(&ctx, token).await.unwrap().expect("the token signs Ana in");
    assert_eq!(session.identity, Identity { role: Role::Manager, user_id: Some(user_id.clone()), name: Some("Ana".into()), via: Via::UserToken });
    assert_eq!(count(&ctx, &format!("SELECT COUNT(*) FROM user_tokens WHERE token_hash = '{token}'")).await, 0);

    let (s, e) = call(&anon, "POST", "/api/invites/accept", Some(json!({"code": code}))).await;
    assert_eq!(s, StatusCode::GONE, "used once: {e}");
    let (s, _) = call(&anon, "POST", "/api/invites/accept", Some(json!({"code": "00000000000000000000000000000000"}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    let (_, p) = call(&app, "GET", &format!("/api/users/{user_id}"), None).await;
    assert_eq!(p["tokens"], 1);
    assert!(p.get("invite_until").is_none(), "no open link any more");
}

/// The accept route takes no identity of its own: the guard lets anyone in.
fn app_for_anyone(ctx: &Ctx) -> Router {
    app(ctx, Identity::anonymous())
}

#[tokio::test]
async fn an_expired_link_is_gone_and_a_new_link_replaces_the_old() {
    let (_dir, ctx) = ctx().await;
    let uid = person(&ctx, "Sam", Role::Hand).await;
    let by = owner().actor();
    let (_, old) = people::create_invite(&ctx, NewInvite { user_id: Some(uid.clone()), ..Default::default() }, &by).await.unwrap();
    let (inv, new) = people::create_invite(&ctx, NewInvite { user_id: Some(uid.clone()), ..Default::default() }, &by).await.unwrap();
    assert_eq!(inv.expires_at - inv.created_at, chrono::Duration::days(7));
    assert_eq!(people::accept_invite(&ctx, &old, None).await.unwrap_err().status, StatusCode::NOT_FOUND, "the new link replaced it");
    assert_eq!(people::list_invites(&ctx).await.unwrap().len(), 1);

    sqlx::query("UPDATE invites SET expires_at = '2020-01-01T00:00:00.000Z'").execute(ctx.db()).await.unwrap();
    assert_eq!(people::accept_invite(&ctx, &new, None).await.unwrap_err().status, StatusCode::GONE);
    assert!(people::list_invites(&ctx).await.unwrap().is_empty(), "expired links aren't open");

    // A link for someone already in People can't also name someone new.
    let both = NewInvite { user_id: Some(uid.clone()), name: Some("X".into()), ..Default::default() };
    assert_eq!(people::create_invite(&ctx, both, &by).await.unwrap_err().status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn revoking_a_token_takes_effect_at_once() {
    let (_dir, ctx) = ctx().await;
    let uid = person(&ctx, "Ana", Role::Manager).await;
    let token = signed_in(&ctx, &uid).await;
    let s = people::session_for_token(&ctx, &token).await.unwrap().unwrap();
    // Cached now; revoking evicts it.
    assert!(people::revoke_token(&ctx, &s.token_id, None).await.unwrap());
    assert!(people::session_for_token(&ctx, &token).await.unwrap().is_none());
    assert!(!people::revoke_token(&ctx, &s.token_id, None).await.unwrap(), "already revoked");

    // Revoking a person's sign-in drops every token and the open link.
    let t1 = signed_in(&ctx, &uid).await;
    let t2 = signed_in(&ctx, &uid).await;
    people::create_invite(&ctx, NewInvite { user_id: Some(uid.clone()), ..Default::default() }, &owner().actor()).await.unwrap();
    assert!(people::session_for_token(&ctx, &t1).await.unwrap().is_some());
    people::revoke_sign_in(&ctx, &uid).await.unwrap();
    assert!(people::session_for_token(&ctx, &t1).await.unwrap().is_none());
    assert!(people::session_for_token(&ctx, &t2).await.unwrap().is_none());
    assert!(people::list_invites(&ctx).await.unwrap().is_empty());

    // Malformed tokens never reach the database.
    for bad in ["opu_", "opu_xyz", "nope", &format!("{}X", &t1[..t1.len() - 1])] {
        assert!(people::session_for_token(&ctx, bad).await.unwrap().is_none(), "{bad}");
    }
}

#[tokio::test]
async fn role_changes_disabling_and_removal_reach_signed_in_browsers_at_once() {
    let (_dir, ctx) = ctx().await;
    let app = app(&ctx, owner());
    let uid = person(&ctx, "Kim", Role::Viewer).await;
    let token = signed_in(&ctx, &uid).await;
    assert_eq!(people::session_for_token(&ctx, &token).await.unwrap().unwrap().identity.role, Role::Viewer);

    let (s, p) = call(&app, "PATCH", &format!("/api/users/{uid}"), Some(json!({"role": "hand", "name": "Kim B"}))).await;
    assert_eq!(s, StatusCode::OK, "{p}");
    let id = people::session_for_token(&ctx, &token).await.unwrap().unwrap().identity;
    assert_eq!((id.role, id.name.as_deref()), (Role::Hand, Some("Kim B")));

    let (s, p) = call(&app, "PATCH", &format!("/api/users/{uid}"), Some(json!({"disabled": true}))).await;
    assert_eq!(s, StatusCode::OK);
    assert!(p["disabled_at"].is_string());
    assert_eq!(p["tokens"], 0, "disabling revokes");
    assert!(people::session_for_token(&ctx, &token).await.unwrap().is_none());
    let (s, _) = call(&app, "POST", "/api/invites", Some(json!({"user_id": uid}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "no links for a disabled person");

    call(&app, "PATCH", &format!("/api/users/{uid}"), Some(json!({"disabled": false}))).await;
    let token = signed_in(&ctx, &uid).await;
    assert!(people::session_for_token(&ctx, &token).await.unwrap().is_some());
    let (s, _) = call(&app, "DELETE", &format!("/api/users/{uid}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(people::session_for_token(&ctx, &token).await.unwrap().is_none());
    assert_eq!(count(&ctx, "SELECT COUNT(*) FROM user_tokens").await, 0, "tokens go with the person");
    assert_eq!(call(&app, "DELETE", &format!("/api/users/{uid}"), None).await.0, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn resolved_tokens_are_cached_until_evicted() {
    let (_dir, ctx) = ctx().await;
    let uid = person(&ctx, "Ana", Role::Manager).await;
    let token = signed_in(&ctx, &uid).await;
    assert!(people::session_for_token(&ctx, &token).await.unwrap().is_some());
    // A change behind the cache's back (not through People) waits for the 30 s cache...
    sqlx::query("UPDATE user_tokens SET revoked_at = '2026-01-01T00:00:00.000Z'").execute(ctx.db()).await.unwrap();
    assert!(people::session_for_token(&ctx, &token).await.unwrap().is_some(), "served from the cache");
    // ...unless someone says people changed.
    people::people_changed(&ctx);
    assert!(people::session_for_token(&ctx, &token).await.unwrap().is_none());
}

#[tokio::test]
async fn last_used_is_written_at_most_once_a_minute() {
    let (_dir, ctx) = ctx().await;
    let uid = person(&ctx, "Ana", Role::Manager).await;
    let token = signed_in(&ctx, &uid).await;
    let last = || async { sqlx::query_scalar::<_, Option<String>>("SELECT last_used FROM user_tokens").fetch_one(ctx.db()).await.unwrap() };
    assert_eq!(last().await, None);
    people::session_for_token(&ctx, &token).await.unwrap().unwrap();
    assert!(last().await.is_some(), "the first use is recorded");
    sqlx::query("UPDATE user_tokens SET last_used = NULL").execute(ctx.db()).await.unwrap();
    for _ in 0..5 {
        people::session_for_token(&ctx, &token).await.unwrap().unwrap();
    }
    assert_eq!(last().await, None, "not again within the minute");

    // A fresh lookup (cache dropped) trusts a recent stored time.
    let recent = op_core::time::to_db(&(op_core::time::now() - chrono::Duration::seconds(20)));
    sqlx::query("UPDATE user_tokens SET last_used = ?").bind(&recent).execute(ctx.db()).await.unwrap();
    people::people_changed(&ctx);
    people::session_for_token(&ctx, &token).await.unwrap().unwrap();
    assert_eq!(last().await.as_deref(), Some(recent.as_str()));
    // An old one is refreshed.
    sqlx::query("UPDATE user_tokens SET last_used = '2026-01-01T00:00:00.000Z'").execute(ctx.db()).await.unwrap();
    people::people_changed(&ctx);
    people::session_for_token(&ctx, &token).await.unwrap().unwrap();
    assert_ne!(last().await.as_deref(), Some("2026-01-01T00:00:00.000Z"));
}

#[tokio::test]
async fn the_owner_acts_as_the_owner_person() {
    let (_dir, ctx) = ctx().await;
    assert_eq!(people::with_owner_person(&ctx, owner()).await, owner(), "no owner person yet");
    person(&ctx, "Hand", Role::Hand).await;
    let cody = person(&ctx, "Cody", Role::Owner).await;
    person(&ctx, "Second owner", Role::Owner).await;
    let id = people::with_owner_person(&ctx, Identity::owner(Via::AppToken)).await;
    assert_eq!((id.user_id.as_deref(), id.name.as_deref(), id.via), (Some(cody.as_str()), Some("Cody"), Via::AppToken));
    // The brain and people keep who they are.
    assert_eq!(people::with_owner_person(&ctx, Identity::brain()).await, Identity::brain());
    let me = op_core::identity::me(&ctx, &people::with_owner_person(&ctx, owner()).await).await.unwrap();
    assert_eq!(me.user.map(|u| u.name).as_deref(), Some("Cody"));

    // Disabled owners don't count.
    people::update_person(&ctx, &cody, op_core::users::UserPatch { disabled: Some(true), ..Default::default() }).await.unwrap();
    assert_eq!(people::with_owner_person(&ctx, owner()).await.name.as_deref(), Some("Second owner"));
}

#[tokio::test]
async fn the_app_token_is_the_owners_to_see() {
    let (_dir, ctx) = ctx().await;
    let token = ctx.settings().await.unwrap().server.app_token;
    for (id, shown) in
        [(owner(), true), (as_role(Role::Owner), true), (as_role(Role::Manager), false), (as_role(Role::Hand), false), (as_role(Role::Viewer), false)]
    {
        let app = app(&ctx, id.clone());
        let (_, s) = call(&app, "GET", "/api/settings", None).await;
        let (_, st) = call(&app, "GET", "/api/state", None).await;
        let want = if shown { token.as_str() } else { "" };
        assert_eq!(s["server"]["app_token"], want, "{id:?} settings");
        assert_eq!(st["settings"]["server"]["app_token"], want, "{id:?} state");
        assert_eq!(s["server"]["port"], 7878, "the rest stays");
    }
}

#[tokio::test]
async fn people_edit_their_own_profile_and_sign_out() {
    let (_dir, ctx) = ctx().await;
    let uid = person(&ctx, "Kim", Role::Viewer).await;
    let token = signed_in(&ctx, &uid).await;
    let session = people::session_for_token(&ctx, &token).await.unwrap().unwrap();
    let me = app(&ctx, session.identity.clone());
    let (s, u) = call(&me, "PATCH", "/api/me/profile", Some(json!({"name": "Kimberly", "phone": "5155550199"}))).await;
    assert_eq!(s, StatusCode::OK, "{u}");
    assert_eq!((u["name"].as_str(), u["phone"].as_str(), u["role"].as_str()), (Some("Kimberly"), Some("+15155550199"), Some("viewer")));
    assert_eq!(people::session_for_token(&ctx, &token).await.unwrap().unwrap().identity.name.as_deref(), Some("Kimberly"));
    let (s, _) = call(&me, "PATCH", "/api/me/profile", Some(json!({"role": "owner"}))).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY, "no promoting yourself");
    let (_, list) = call(&me, "GET", "/api/me/tokens", None).await;
    assert_eq!(list[0]["id"], session.token_id.as_str());

    let local = app(&ctx, owner());
    assert_eq!(call(&local, "PATCH", "/api/me/profile", Some(json!({"name": "X"}))).await.0, StatusCode::NOT_FOUND, "the owner isn't in People");
    assert_eq!(call(&local, "POST", "/api/me/signout", None).await.0, StatusCode::BAD_REQUEST, "nothing to sign out of");

    let signed = with_identity(op_core::router().layer(axum::Extension(SessionToken(session.token_id.clone()))).with_state(ctx.clone()), session.identity);
    assert_eq!(call(&signed, "POST", "/api/me/signout", None).await.0, StatusCode::NO_CONTENT);
    assert!(people::session_for_token(&ctx, &token).await.unwrap().is_none());
}

#[test]
fn actors_read_as_names() {
    let a = |via, name: Option<&str>| Actor { via, user_id: None, name: name.map(str::to_owned) };
    assert_eq!(people::actor_label(&a(Via::UserToken, Some("Ana"))).as_deref(), Some("Ana"));
    assert_eq!(people::actor_label(&a(Via::Local, None)).as_deref(), Some("owner"));
    assert_eq!(people::actor_label(&a(Via::AppToken, None)).as_deref(), Some("owner"));
    assert_eq!(people::actor_label(&a(Via::System, None)), None);
}
