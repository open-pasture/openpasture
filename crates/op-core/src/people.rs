//! People as the owner manages them (Settings > People), sign-in links,
//! per-person tokens, and how op-server's guard turns an `opu_` token into an
//! [`Identity`].
//!
//! No passwords. Adding a person gives them no sign-in: people who only text
//! are users with a phone and nothing else. A sign-in link
//! (`{base_url}/#/join/<code>`, a 128-bit code valid for 7 days) is accepted
//! once and returns an `opu_` token (64 hex characters) that the browser keeps
//! as its bearer token. Codes and tokens are shown once and stored as sha256.
//!
//! Routes: `/api/users*`, `/api/invites*` and `/api/tokens*` are the owner's
//! (op-server's policy, and each handler checks too); `/api/me/*` is every
//! role's own profile and token. `POST /api/invites/accept` needs no sign-in;
//! the guard rate-limits it.
//!
//! Tokens resolve through a 30 s cache per data dir. Revoking a token and
//! changing, disabling or removing a person evict at once. `last_used` is
//! written at most once a minute per token.
//!
//! PII: never add `user_tokens` or `invites` to op-analytics' `SQL_TABLES`.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{delete, get, patch, post};
use axum::{Extension, Json, Router};
use chrono::{DateTime, Utc};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::domain::{DbEnum, Settings};
use crate::error::{ApiError, ApiJson, ApiResult};
use crate::identity::{Actor, Identity, Role, Via};
use crate::time::{from_db, now, opt_from_db, to_db};
use crate::users::{self, NewUser, User, UserPatch};
use crate::{Ctx, id, keys};

/// `tok_…`: a token row.
pub const TOKEN: &str = "tok";
/// `inv_…`: a sign-in link.
pub const INVITE: &str = "inv";
/// A person's own token starts with this, then 64 hex characters.
pub const TOKEN_PREFIX: &str = "opu_";
/// How long a sign-in link works.
pub const INVITE_DAYS: i64 = 7;
/// Resolved tokens and the owner person are kept this long.
const CACHE_TTL: Duration = Duration::from_secs(30);
/// `user_tokens.last_used` is written at most this often per token.
const LAST_USED_EVERY: Duration = Duration::from_secs(60);

// ---- records ----------------------------------------------------------------------------

/// A person as Settings > People shows them: the user and their sign-in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Person {
    #[serde(flatten)]
    pub user: User,
    /// Browsers signed in as this person (tokens not revoked).
    pub tokens: u32,
    /// The latest use of any of those tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<DateTime<Utc>>,
    /// When the person's open sign-in link stops working.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invite_until: Option<DateTime<Utc>>,
}

/// A sign-in link. Name, role, phone and email are the person's when it was made.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Invite {
    pub id: String,
    pub user_id: String,
    pub name: String,
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_by: Option<Actor>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accepted_at: Option<DateTime<Utc>>,
}

/// `POST /api/invites`: a link for someone already in People (`user_id`), or
/// for someone new (`name`, `role`, `phone?`, `email?`), who is added now.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NewInvite {
    #[serde(default)]
    pub user_id: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: Option<Role>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// A new link. `code` and `url` are shown this once.
#[derive(Debug, Clone, Serialize)]
pub struct CreatedInvite {
    #[serde(flatten)]
    pub invite: Invite,
    pub code: String,
    /// `{base_url}/#/join/<code>`
    pub url: String,
}

/// A person's token, without the token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TokenInfo {
    pub id: String,
    pub user_id: String,
    pub label: String,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revoked_at: Option<DateTime<Utc>>,
}

/// What accepting a link returns. `token` is shown this once.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Accepted {
    pub token: String,
    pub user: User,
}

/// A request signed in with a person's own token: who, and which token.
#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub identity: Identity,
    pub token_id: String,
}

/// The token row a request came in with. The guard adds it next to the
/// [`Identity`]; `POST /api/me/signout` revokes it.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionToken(pub String);

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn random_hex(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex(&bytes)
}

/// `opu_` and 64 lowercase hex characters.
pub fn is_token_shape(token: &str) -> bool {
    token.strip_prefix(TOKEN_PREFIX).is_some_and(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

fn invite_from_row(r: &SqliteRow) -> anyhow::Result<Invite> {
    let created_by: Option<String> = r.try_get("created_by")?;
    Ok(Invite {
        id: r.try_get("id")?,
        user_id: r.try_get("user_id")?,
        name: r.try_get("name")?,
        role: Role::from_db(&r.try_get::<String, _>("role")?)?,
        phone: r.try_get("phone")?,
        email: r.try_get("email")?,
        created_by: created_by.as_deref().map(serde_json::from_str).transpose()?,
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
        expires_at: from_db(&r.try_get::<String, _>("expires_at")?)?,
        accepted_at: opt_from_db(r.try_get("accepted_at")?)?,
    })
}

fn token_from_row(r: &SqliteRow) -> anyhow::Result<TokenInfo> {
    Ok(TokenInfo {
        id: r.try_get("id")?,
        user_id: r.try_get("user_id")?,
        label: r.try_get("label")?,
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
        last_used: opt_from_db(r.try_get("last_used")?)?,
        revoked_at: opt_from_db(r.try_get("revoked_at")?)?,
    })
}

// ---- people -----------------------------------------------------------------------------

const PERSON_SQL: &str = "SELECT u.*,
    (SELECT COUNT(*) FROM user_tokens t WHERE t.user_id = u.id AND t.revoked_at IS NULL) AS tokens,
    (SELECT MAX(t.last_used) FROM user_tokens t WHERE t.user_id = u.id AND t.revoked_at IS NULL) AS last_used,
    (SELECT MAX(i.expires_at) FROM invites i WHERE i.user_id = u.id AND i.accepted_at IS NULL AND i.expires_at > ?1) AS invite_until
    FROM users u";

fn person_from_row(r: &SqliteRow) -> anyhow::Result<Person> {
    Ok(Person {
        user: users::user_from_row(r)?,
        tokens: r.try_get::<i64, _>("tokens")?.max(0) as u32,
        last_used: opt_from_db(r.try_get("last_used")?)?,
        invite_until: opt_from_db(r.try_get("invite_until")?)?,
    })
}

/// Everyone, enabled first, then by name, with their sign-in.
pub async fn list_people(ctx: &Ctx) -> anyhow::Result<Vec<Person>> {
    let sql = format!("{PERSON_SQL} ORDER BY u.disabled_at IS NOT NULL, u.name COLLATE NOCASE, u.id");
    let rows = sqlx::query(&sql).bind(to_db(&now())).fetch_all(ctx.db()).await?;
    rows.iter().map(person_from_row).collect()
}

pub async fn get_person(ctx: &Ctx, user_id: &str) -> anyhow::Result<Option<Person>> {
    let sql = format!("{PERSON_SQL} WHERE u.id = ?2");
    let row = sqlx::query(&sql).bind(to_db(&now())).bind(user_id).fetch_optional(ctx.db()).await?;
    row.map(|r| person_from_row(&r)).transpose()
}

async fn person_or_404(ctx: &Ctx, user_id: &str) -> ApiResult<Person> {
    get_person(ctx, user_id).await?.ok_or_else(|| ApiError::not_found("No such person."))
}

/// Add a person. They get no sign-in.
pub async fn add_person(ctx: &Ctx, u: NewUser) -> ApiResult<Person> {
    let user = users::create_user(ctx, u).await?;
    evict(ctx, Some(&user.id));
    person_or_404(ctx, &user.id).await
}

/// Change a person. A role or name change reaches their signed-in browsers at
/// once; disabling revokes every token and open link.
pub async fn update_person(ctx: &Ctx, user_id: &str, p: UserPatch) -> ApiResult<Person> {
    let disable = p.disabled == Some(true);
    users::update_user(ctx, user_id, p).await?;
    if disable {
        revoke_sign_in(ctx, user_id).await?;
    }
    evict(ctx, Some(user_id));
    person_or_404(ctx, user_id).await
}

/// Remove a person, their tokens and links. Records keep the name they were
/// stored with (`Actor`).
pub async fn remove_person(ctx: &Ctx, user_id: &str) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM users WHERE id = ?").bind(user_id).execute(ctx.db()).await?.rows_affected();
    evict(ctx, Some(user_id));
    Ok(n == 1)
}

/// Sign a person out everywhere: revoke their tokens and drop open links.
pub async fn revoke_sign_in(ctx: &Ctx, user_id: &str) -> anyhow::Result<()> {
    let at = to_db(&now());
    sqlx::query("UPDATE user_tokens SET revoked_at = ? WHERE user_id = ? AND revoked_at IS NULL").bind(&at).bind(user_id).execute(ctx.db()).await?;
    sqlx::query("DELETE FROM invites WHERE user_id = ? AND accepted_at IS NULL").bind(user_id).execute(ctx.db()).await?;
    evict(ctx, Some(user_id));
    Ok(())
}

// ---- sign-in links ----------------------------------------------------------------------

/// Make a sign-in link. For someone new the person is added first. A person
/// has at most one open link: a new one replaces it. Returns the link and its
/// code (shown once).
pub async fn create_invite(ctx: &Ctx, n: NewInvite, by: &Actor) -> ApiResult<(Invite, String)> {
    let user = match &n.user_id {
        Some(uid) => {
            if n.name.is_some() || n.role.is_some() || n.phone.is_some() || n.email.is_some() {
                return Err(ApiError::bad_request("Give user_id, or name and role for someone new, not both."));
            }
            let u = users::get_user(ctx, uid).await?.ok_or_else(|| ApiError::not_found("No such person."))?;
            if u.disabled_at.is_some() {
                return Err(ApiError::bad_request("This person is disabled. Enable them first."));
            }
            u
        }
        None => {
            let name = n.name.clone().ok_or_else(|| ApiError::bad_request("A sign-in link needs user_id, or a name and role."))?;
            let role = n.role.ok_or_else(|| ApiError::bad_request("A new person needs a role."))?;
            let u = users::create_user(ctx, NewUser { name, role, phone: n.phone.clone(), email: n.email.clone() }).await?;
            evict(ctx, Some(&u.id));
            u
        }
    };
    let code = random_hex(16);
    let at = now();
    let invite = Invite {
        id: id::new_id(INVITE),
        user_id: user.id.clone(),
        name: user.name.clone(),
        role: user.role,
        phone: user.phone.clone(),
        email: user.email.clone(),
        created_by: Some(by.clone()),
        created_at: at,
        expires_at: at + chrono::Duration::days(INVITE_DAYS),
        accepted_at: None,
    };
    let mut tx = crate::store::begin_immediate(ctx.db()).await?;
    sqlx::query("DELETE FROM invites WHERE user_id = ? AND accepted_at IS NULL").bind(&user.id).execute(&mut *tx).await?;
    sqlx::query(
        "INSERT INTO invites (id, code_hash, name, role, phone, email, created_by, created_at, expires_at, accepted_at, user_id)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, NULL, ?)",
    )
    .bind(&invite.id)
    .bind(keys::hash_key(&code))
    .bind(&invite.name)
    .bind(invite.role.as_db())
    .bind(&invite.phone)
    .bind(&invite.email)
    .bind(serde_json::to_string(by).map_err(anyhow::Error::from)?)
    .bind(to_db(&invite.created_at))
    .bind(to_db(&invite.expires_at))
    .bind(&invite.user_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((invite, code))
}

/// The link's URL: `{base_url}/#/join/<code>`. The code rides in the fragment,
/// which browsers never send to the server.
pub fn invite_url(ctx: &Ctx, code: &str) -> String {
    format!("{}/#/join/{code}", ctx.base_url())
}

/// Open links (not accepted, not expired), newest first.
pub async fn list_invites(ctx: &Ctx) -> anyhow::Result<Vec<Invite>> {
    let rows = sqlx::query("SELECT * FROM invites WHERE accepted_at IS NULL AND expires_at > ? ORDER BY created_at DESC, rowid DESC")
        .bind(to_db(&now()))
        .fetch_all(ctx.db())
        .await?;
    rows.iter().map(invite_from_row).collect()
}

/// Drop an open link. False when there is none by that id.
pub async fn delete_invite(ctx: &Ctx, invite_id: &str) -> anyhow::Result<bool> {
    let n = sqlx::query("DELETE FROM invites WHERE id = ? AND accepted_at IS NULL").bind(invite_id).execute(ctx.db()).await?.rows_affected();
    Ok(n == 1)
}

/// Accept a link once: 404 for a code that never existed (or whose link was
/// dropped), 410 when used, expired or its person disabled. Returns a new
/// `opu_` token for the person.
pub async fn accept_invite(ctx: &Ctx, code: &str, label: Option<&str>) -> ApiResult<Accepted> {
    const NO_LINK: &str = "That sign-in link doesn't work. Ask for a new one.";
    let code = code.trim();
    if code.is_empty() || code.len() > 128 {
        return Err(ApiError::not_found(NO_LINK));
    }
    let label: String = label.unwrap_or_default().trim().chars().take(80).collect();
    let at = now();
    let mut tx = crate::store::begin_immediate(ctx.db()).await?;
    let row = sqlx::query("SELECT * FROM invites WHERE code_hash = ?").bind(keys::hash_key(code)).fetch_optional(&mut *tx).await?;
    let invite = match row {
        Some(r) => invite_from_row(&r)?,
        None => return Err(ApiError::not_found(NO_LINK)),
    };
    if invite.accepted_at.is_some() {
        return Err(ApiError::new(StatusCode::GONE, "That sign-in link was already used. Ask for a new one."));
    }
    if invite.expires_at <= at {
        return Err(ApiError::new(StatusCode::GONE, "That sign-in link has expired. Ask for a new one."));
    }
    let user = match sqlx::query("SELECT * FROM users WHERE id = ?").bind(&invite.user_id).fetch_optional(&mut *tx).await? {
        Some(r) => users::user_from_row(&r)?,
        None => return Err(ApiError::not_found(NO_LINK)),
    };
    if user.disabled_at.is_some() {
        return Err(ApiError::new(StatusCode::GONE, "This person can't sign in any more."));
    }
    let n = sqlx::query("UPDATE invites SET accepted_at = ? WHERE id = ? AND accepted_at IS NULL")
        .bind(to_db(&at))
        .bind(&invite.id)
        .execute(&mut *tx)
        .await?
        .rows_affected();
    if n != 1 {
        return Err(ApiError::new(StatusCode::GONE, "That sign-in link was already used. Ask for a new one."));
    }
    let token = format!("{TOKEN_PREFIX}{}", random_hex(32));
    sqlx::query("INSERT INTO user_tokens (id, user_id, label, token_hash, created_at, last_used, revoked_at) VALUES (?, ?, ?, ?, ?, NULL, NULL)")
        .bind(id::new_id(TOKEN))
        .bind(&user.id)
        .bind(&label)
        .bind(keys::hash_key(&token))
        .bind(to_db(&at))
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    tracing::info!(user = %user.id, invite = %invite.id, "sign-in link accepted");
    Ok(Accepted { token, user })
}

// ---- tokens -----------------------------------------------------------------------------

/// Tokens not revoked, newest first; one person's when `user_id` is given.
pub async fn list_tokens(ctx: &Ctx, user_id: Option<&str>) -> anyhow::Result<Vec<TokenInfo>> {
    let rows = sqlx::query(
        "SELECT id, user_id, label, created_at, last_used, revoked_at FROM user_tokens
         WHERE revoked_at IS NULL AND (?1 IS NULL OR user_id = ?1) ORDER BY created_at DESC, rowid DESC",
    )
    .bind(user_id)
    .fetch_all(ctx.db())
    .await?;
    rows.iter().map(token_from_row).collect()
}

/// Revoke one token; its browser gets 401 on its next request. False when
/// there is no such live token (for `user_id`: of that person).
pub async fn revoke_token(ctx: &Ctx, token_id: &str, user_id: Option<&str>) -> anyhow::Result<bool> {
    let row: Option<(String,)> =
        sqlx::query_as("UPDATE user_tokens SET revoked_at = ? WHERE id = ? AND revoked_at IS NULL AND (?3 IS NULL OR user_id = ?3) RETURNING user_id")
            .bind(to_db(&now()))
            .bind(token_id)
            .bind(user_id)
            .fetch_optional(ctx.db())
            .await?;
    match row {
        Some((uid,)) => {
            evict(ctx, Some(&uid));
            Ok(true)
        }
        None => Ok(false),
    }
}

// ---- resolving tokens (the guard) -------------------------------------------------------

#[derive(Default)]
struct DirCache {
    /// Bumped by every eviction, so a lookup that raced one isn't cached.
    generation: u64,
    /// token sha256 → session
    tokens: HashMap<String, Cached>,
    /// The first enabled owner person (id, name), and when it was read.
    owner: Option<(Instant, Option<(String, String)>)>,
}

struct Cached {
    session: Session,
    at: Instant,
    /// When `last_used` was last written (or how old the stored one was).
    used: Option<Instant>,
}

fn caches() -> &'static Mutex<HashMap<PathBuf, DirCache>> {
    static CACHES: OnceLock<Mutex<HashMap<PathBuf, DirCache>>> = OnceLock::new();
    CACHES.get_or_init(Default::default)
}

fn with_cache<R>(ctx: &Ctx, f: impl FnOnce(&mut DirCache) -> R) -> R {
    let mut map = caches().lock().unwrap_or_else(|e| e.into_inner());
    f(map.entry(ctx.data_dir().to_path_buf()).or_default())
}

/// Forget cached sessions (one person's, or everyone's) and the owner person.
fn evict(ctx: &Ctx, user_id: Option<&str>) {
    with_cache(ctx, |c| {
        c.generation += 1;
        c.owner = None;
        match user_id {
            Some(uid) => c.tokens.retain(|_, e| e.session.identity.user_id.as_deref() != Some(uid)),
            None => c.tokens.clear(),
        }
    });
}

/// Someone outside this module changed people (name, role, disabled): forget
/// what is cached so the change applies to the next request.
pub fn people_changed(ctx: &Ctx) {
    evict(ctx, None);
}

async fn touch_token(ctx: &Ctx, token_id: &str) {
    if let Err(e) = sqlx::query("UPDATE user_tokens SET last_used = ? WHERE id = ?").bind(to_db(&now())).bind(token_id).execute(ctx.db()).await {
        tracing::warn!("recording token use: {e:#}");
    }
}

/// The person an `opu_` token signs in, as a `user_token` identity. `None`
/// for a malformed, unknown or revoked token, or a disabled person.
pub async fn session_for_token(ctx: &Ctx, token: &str) -> anyhow::Result<Option<Session>> {
    let token = token.trim();
    if !is_token_shape(token) {
        return Ok(None);
    }
    let hash = keys::hash_key(token);
    let t0 = Instant::now();
    let hit = with_cache(ctx, |c| {
        let e = c.tokens.get_mut(&hash)?;
        if t0.duration_since(e.at) >= CACHE_TTL {
            return None;
        }
        let touch = e.used.is_none_or(|u| t0.duration_since(u) >= LAST_USED_EVERY);
        if touch {
            e.used = Some(t0);
        }
        Some((e.session.clone(), touch))
    });
    if let Some((session, touch)) = hit {
        if touch {
            touch_token(ctx, &session.token_id).await;
        }
        return Ok(Some(session));
    }

    let generation = with_cache(ctx, |c| c.generation);
    let row = sqlx::query(
        "SELECT t.id AS token_id, t.last_used, u.id AS user_id, u.name, u.role FROM user_tokens t JOIN users u ON u.id = t.user_id
         WHERE t.token_hash = ? AND t.revoked_at IS NULL AND u.disabled_at IS NULL",
    )
    .bind(&hash)
    .fetch_optional(ctx.db())
    .await?;
    let Some(row) = row else {
        with_cache(ctx, |c| c.tokens.remove(&hash));
        return Ok(None);
    };
    let session = Session {
        identity: Identity {
            role: Role::from_db(&row.try_get::<String, _>("role")?)?,
            user_id: Some(row.try_get("user_id")?),
            name: Some(row.try_get("name")?),
            via: Via::UserToken,
        },
        token_id: row.try_get("token_id")?,
    };
    let age = opt_from_db(row.try_get("last_used")?)?.map(|t| (now() - t).to_std().unwrap_or_default());
    let touch = age.is_none_or(|a| a >= LAST_USED_EVERY);
    if touch {
        touch_token(ctx, &session.token_id).await;
    }
    let used = if touch { Some(t0) } else { age.and_then(|a| t0.checked_sub(a)) };
    with_cache(ctx, |c| {
        if c.generation == generation {
            c.tokens.insert(hash, Cached { session: session.clone(), at: t0, used });
        }
    });
    Ok(Some(session))
}

/// The first enabled owner person (id, name), if the owner added themselves
/// to People. Cached like tokens.
pub async fn owner_person(ctx: &Ctx) -> anyhow::Result<Option<(String, String)>> {
    let t0 = Instant::now();
    let hit = with_cache(ctx, |c| c.owner.as_ref().filter(|(at, _)| t0.duration_since(*at) < CACHE_TTL).map(|(_, o)| o.clone()));
    if let Some(owner) = hit {
        return Ok(owner);
    }
    let generation = with_cache(ctx, |c| c.generation);
    let owner: Option<(String, String)> =
        sqlx::query_as("SELECT id, name FROM users WHERE role = 'owner' AND disabled_at IS NULL ORDER BY created_at, rowid LIMIT 1")
            .fetch_optional(ctx.db())
            .await?;
    with_cache(ctx, |c| {
        if c.generation == generation {
            c.owner = Some((t0, owner.clone()));
        }
    });
    Ok(owner)
}

/// The app token and local requests are the owner. When the owner is also a
/// person in People (so they can get texts), they act as that person.
pub async fn with_owner_person(ctx: &Ctx, mut identity: Identity) -> Identity {
    if identity.role == Role::Owner && identity.user_id.is_none() && matches!(identity.via, Via::Local | Via::AppToken) {
        match owner_person(ctx).await {
            Ok(Some((uid, name))) => {
                identity.user_id = Some(uid);
                identity.name = Some(name);
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("reading the owner person: {e:#}"),
        }
    }
    identity
}

/// `GET /api/settings` and `/api/state` show the app token to owners only.
pub fn redact_settings(identity: &Identity, s: &mut Settings) {
    if !identity.can(Role::Owner) {
        s.server.app_token.clear();
    }
}

/// How an actor reads on a record: their name; the owner for the app token or
/// a local request without a person; nothing for background jobs.
pub fn actor_label(a: &Actor) -> Option<String> {
    match (&a.name, a.via) {
        (Some(n), _) => Some(n.clone()),
        (None, Via::Local | Via::AppToken) => Some("owner".into()),
        _ => None,
    }
}

// ---- routes -----------------------------------------------------------------------------

pub(crate) fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/users", get(get_people).post(post_person))
        .route("/api/users/{id}", get(get_one_person).patch(patch_person).delete(delete_person))
        .route("/api/users/{id}/revoke", post(post_revoke))
        .route("/api/invites", get(get_invites).post(post_invite))
        .route("/api/invites/accept", post(post_accept))
        .route("/api/invites/{id}", delete(delete_one_invite))
        .route("/api/tokens", get(get_tokens))
        .route("/api/tokens/{id}", delete(delete_token))
        .route("/api/me/profile", patch(patch_profile))
        .route("/api/me/tokens", get(get_my_tokens))
        .route("/api/me/tokens/{id}", delete(delete_my_token))
        .route("/api/me/signout", post(post_signout))
}

async fn get_people(State(ctx): State<Ctx>, me: Identity) -> ApiResult<Json<Vec<Person>>> {
    me.require(Role::Owner)?;
    Ok(Json(list_people(&ctx).await?))
}

async fn post_person(State(ctx): State<Ctx>, me: Identity, ApiJson(body): ApiJson<NewUser>) -> ApiResult<(StatusCode, Json<Person>)> {
    me.require(Role::Owner)?;
    Ok((StatusCode::CREATED, Json(add_person(&ctx, body).await?)))
}

async fn get_one_person(State(ctx): State<Ctx>, me: Identity, Path(uid): Path<String>) -> ApiResult<Json<Person>> {
    me.require(Role::Owner)?;
    Ok(Json(person_or_404(&ctx, &uid).await?))
}

async fn patch_person(State(ctx): State<Ctx>, me: Identity, Path(uid): Path<String>, ApiJson(body): ApiJson<UserPatch>) -> ApiResult<Json<Person>> {
    me.require(Role::Owner)?;
    Ok(Json(update_person(&ctx, &uid, body).await?))
}

async fn delete_person(State(ctx): State<Ctx>, me: Identity, Path(uid): Path<String>) -> ApiResult<StatusCode> {
    me.require(Role::Owner)?;
    if !remove_person(&ctx, &uid).await? {
        return Err(ApiError::not_found("No such person."));
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn post_revoke(State(ctx): State<Ctx>, me: Identity, Path(uid): Path<String>) -> ApiResult<Json<Person>> {
    me.require(Role::Owner)?;
    person_or_404(&ctx, &uid).await?;
    revoke_sign_in(&ctx, &uid).await?;
    Ok(Json(person_or_404(&ctx, &uid).await?))
}

async fn get_invites(State(ctx): State<Ctx>, me: Identity) -> ApiResult<Json<Vec<Invite>>> {
    me.require(Role::Owner)?;
    Ok(Json(list_invites(&ctx).await?))
}

async fn post_invite(State(ctx): State<Ctx>, me: Identity, ApiJson(body): ApiJson<NewInvite>) -> ApiResult<(StatusCode, Json<CreatedInvite>)> {
    me.require(Role::Owner)?;
    let (invite, code) = create_invite(&ctx, body, &me.actor()).await?;
    let url = invite_url(&ctx, &code);
    Ok((StatusCode::CREATED, Json(CreatedInvite { invite, code, url })))
}

async fn delete_one_invite(State(ctx): State<Ctx>, me: Identity, Path(iid): Path<String>) -> ApiResult<StatusCode> {
    me.require(Role::Owner)?;
    if !delete_invite(&ctx, &iid).await? {
        return Err(ApiError::not_found("No open sign-in link by that id."));
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct AcceptBody {
    code: String,
    #[serde(default)]
    label: Option<String>,
}

/// No sign-in needed (the code is the credential); the guard rate-limits it.
async fn post_accept(State(ctx): State<Ctx>, ApiJson(body): ApiJson<AcceptBody>) -> ApiResult<Json<Accepted>> {
    Ok(Json(accept_invite(&ctx, &body.code, body.label.as_deref()).await?))
}

async fn get_tokens(State(ctx): State<Ctx>, me: Identity) -> ApiResult<Json<Vec<TokenInfo>>> {
    me.require(Role::Owner)?;
    Ok(Json(list_tokens(&ctx, None).await?))
}

async fn delete_token(State(ctx): State<Ctx>, me: Identity, Path(tid): Path<String>) -> ApiResult<StatusCode> {
    me.require(Role::Owner)?;
    if !revoke_token(&ctx, &tid, None).await? {
        return Err(ApiError::not_found("No such token."));
    }
    Ok(StatusCode::NO_CONTENT)
}

fn own_user_id(me: &Identity) -> ApiResult<&str> {
    me.require(Role::Viewer)?;
    me.user_id.as_deref().ok_or_else(|| ApiError::not_found("You aren't in People yet."))
}

/// What a person may change about themselves.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfilePatch {
    #[serde(default)]
    name: Option<String>,
    #[serde(default, deserialize_with = "present")]
    phone: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    email: Option<Option<String>>,
}

/// A key that is present (even as `null`) becomes `Some(..)`.
fn present<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

async fn patch_profile(State(ctx): State<Ctx>, me: Identity, ApiJson(body): ApiJson<ProfilePatch>) -> ApiResult<Json<User>> {
    let uid = own_user_id(&me)?;
    let user = users::update_user(&ctx, uid, UserPatch { name: body.name, phone: body.phone, email: body.email, ..Default::default() }).await?;
    evict(&ctx, Some(uid));
    Ok(Json(user))
}

async fn get_my_tokens(State(ctx): State<Ctx>, me: Identity) -> ApiResult<Json<Vec<TokenInfo>>> {
    let uid = own_user_id(&me)?;
    Ok(Json(list_tokens(&ctx, Some(uid)).await?))
}

async fn delete_my_token(State(ctx): State<Ctx>, me: Identity, Path(tid): Path<String>) -> ApiResult<StatusCode> {
    let uid = own_user_id(&me)?;
    if !revoke_token(&ctx, &tid, Some(uid)).await? {
        return Err(ApiError::not_found("No such token."));
    }
    Ok(StatusCode::NO_CONTENT)
}

/// Revoke the token this request came with.
async fn post_signout(State(ctx): State<Ctx>, me: Identity, token: Option<Extension<SessionToken>>) -> ApiResult<StatusCode> {
    me.require(Role::Viewer)?;
    let Some(Extension(SessionToken(tid))) = token else {
        return Err(ApiError::bad_request("This browser isn't signed in with its own token."));
    };
    revoke_token(&ctx, &tid, None).await?;
    Ok(StatusCode::NO_CONTENT)
}
