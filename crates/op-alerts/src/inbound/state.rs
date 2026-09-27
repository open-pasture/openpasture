//! `texting_state`: one row per inbound loop (see the migration for keys).

use chrono::{DateTime, Utc};
use op_core::Ctx;
use op_core::time::{now, opt_from_db, to_db};
use serde::Serialize;
use sqlx::Row;

#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct State {
    #[serde(skip)]
    pub key: String,
    #[serde(skip)]
    pub cursor: Option<String>,
    #[serde(skip)]
    pub since: Option<DateTime<Utc>>,
    /// Last run.
    #[serde(rename = "at", skip_serializing_if = "Option::is_none")]
    pub ran_at: Option<DateTime<Utc>>,
    /// Last run that worked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ok_at: Option<DateTime<Utc>>,
    /// Why the last run failed; none after a success.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

pub async fn get(ctx: &Ctx, key: &str) -> anyhow::Result<Option<State>> {
    let row = sqlx::query("SELECT * FROM texting_state WHERE key = ?").bind(key).fetch_optional(ctx.db()).await?;
    row.map(|r| {
        Ok(State {
            key: r.try_get("key")?,
            cursor: r.try_get("cursor")?,
            since: opt_from_db(r.try_get("since")?)?,
            ran_at: opt_from_db(r.try_get("ran_at")?)?,
            ok_at: opt_from_db(r.try_get("ok_at")?)?,
            error: r.try_get("error")?,
        })
    })
    .transpose()
}

/// A run that worked: stores `cursor` and `since` as given.
pub async fn ok(ctx: &Ctx, key: &str, cursor: Option<&str>, since: Option<DateTime<Utc>>) -> anyhow::Result<()> {
    let t = to_db(&now());
    sqlx::query(
        "INSERT INTO texting_state (key, cursor, since, ran_at, ok_at, error) VALUES (?, ?, ?, ?, ?, NULL)
         ON CONFLICT(key) DO UPDATE SET cursor = excluded.cursor, since = excluded.since, ran_at = excluded.ran_at, ok_at = excluded.ok_at, error = NULL",
    )
    .bind(key)
    .bind(cursor)
    .bind(since.as_ref().map(to_db))
    .bind(&t)
    .bind(&t)
    .execute(ctx.db())
    .await?;
    Ok(())
}

/// A run that failed: the cursor stays.
pub async fn failed(ctx: &Ctx, key: &str, error: &str) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO texting_state (key, ran_at, error) VALUES (?, ?, ?)
         ON CONFLICT(key) DO UPDATE SET ran_at = excluded.ran_at, error = excluded.error",
    )
    .bind(key)
    .bind(to_db(&now()))
    .bind(error)
    .execute(ctx.db())
    .await?;
    Ok(())
}
