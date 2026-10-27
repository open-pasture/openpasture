//! Settings the server can't read are never replaced with defaults (that
//! would mint a new app token); `settings::repair` keeps what reads.

use op_core::Ctx;
use op_core::settings::{self, UnreadableSettings};
use op_core::store::DB_FILE;
use sqlx::sqlite::SqliteConnectOptions;

async fn raw(dir: &std::path::Path) -> sqlx::SqlitePool {
    sqlx::SqlitePool::connect_with(SqliteConnectOptions::new().filename(dir.join(DB_FILE))).await.unwrap()
}

async fn stored(pool: &sqlx::SqlitePool) -> String {
    sqlx::query_scalar("SELECT value FROM settings WHERE key = 'settings'").fetch_one(pool).await.unwrap()
}

#[tokio::test]
async fn unreadable_settings_stop_the_server_and_are_left_as_they_are() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    let token = ctx.settings().await.unwrap().server.app_token;
    drop(ctx);

    // A brain this build doesn't know (written by a newer release).
    let pool = raw(dir.path()).await;
    let mut v: serde_json::Value = serde_json::from_str(&stored(&pool).await).unwrap();
    v["brain"]["id"] = "telepathy".into();
    v["decision_time"] = "05:15".into();
    sqlx::query("UPDATE settings SET value = ? WHERE key = 'settings'").bind(v.to_string()).execute(&pool).await.unwrap();
    let before = stored(&pool).await;

    let err = Ctx::open(dir.path()).await.err().expect("unreadable settings are an error");
    let unreadable = err.downcast_ref::<UnreadableSettings>().expect("typed error");
    assert!(unreadable.detail.contains("telepathy"), "{}", unreadable.detail);
    assert!(err.to_string().contains("openpasture settings repair"), "{err}");
    assert_eq!(stored(&pool).await, before, "nothing written");

    // Repair keeps the token and every field that reads; the bad one is reset.
    let lock = op_core::instance::lock(dir.path()).unwrap();
    let done = settings::repair(&pool).await.unwrap().expect("repaired");
    assert_eq!(done.reset, ["brain.id"]);
    let kept: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = ?").bind(&done.kept_as).fetch_one(&pool).await.unwrap();
    assert_eq!(kept, before, "the unreadable text is kept");
    assert_eq!(settings::repair(&pool).await.unwrap(), None, "nothing to do the second time");
    drop(lock);
    pool.close().await;

    let ctx = Ctx::open(dir.path()).await.unwrap();
    let s = ctx.settings().await.unwrap();
    assert_eq!(s.server.app_token, token);
    assert_eq!(s.decision_time, "05:15");
    assert_eq!(s.brain.id, op_core::BrainId::default());
}

#[tokio::test]
async fn settings_that_are_not_json_are_an_error_too() {
    let dir = tempfile::tempdir().unwrap();
    drop(Ctx::open(dir.path()).await.unwrap());
    let pool = raw(dir.path()).await;
    sqlx::query("UPDATE settings SET value = '{not json' WHERE key = 'settings'").execute(&pool).await.unwrap();
    let err = Ctx::open(dir.path()).await.err().expect("an error");
    assert!(err.downcast_ref::<UnreadableSettings>().is_some_and(|e| e.detail.contains("not JSON")), "{err:#}");
    assert_eq!(stored(&pool).await, "{not json");
}

#[tokio::test]
async fn the_first_read_creates_one_token_however_many_ask_at_once() {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    sqlx::query("DELETE FROM settings WHERE key = 'settings'").execute(ctx.db()).await.unwrap();
    let reads = futures::future::join_all((0..8).map(|_| ctx.settings())).await;
    let tokens: std::collections::BTreeSet<String> = reads.into_iter().map(|s| s.unwrap().server.app_token).collect();
    assert_eq!(tokens.len(), 1);
}
