//! People on the farm: name, role, phone and email. A person needs no
//! sign-in: people who only text are users with a phone and no token.
//!
//! PII: never add `users` to op-analytics' `SQL_TABLES`.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};
use sqlx::Row;
use sqlx::sqlite::SqliteRow;

use crate::domain::DbEnum;
use crate::error::{ApiError, ApiResult};
use crate::identity::Role;
use crate::time::{from_db, now, opt_from_db, to_db};
use crate::{Ctx, id};

/// `usr_…`
pub const USER: &str = "usr";

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct User {
    pub id: String,
    pub name: String,
    pub role: Role,
    /// E.164, e.g. `+15155550123`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    /// Set by a one-time code; cleared when the phone changes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone_verified_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NewUser {
    pub name: String,
    pub role: Role,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
}

/// A partial update. For `phone` and `email`, absent keeps the value and
/// `null` clears it.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct UserPatch {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub role: Option<Role>,
    #[serde(default, deserialize_with = "present")]
    pub phone: Option<Option<String>>,
    #[serde(default, deserialize_with = "present")]
    pub email: Option<Option<String>>,
    #[serde(default)]
    pub disabled: Option<bool>,
}

/// A key that is present (even as `null`) becomes `Some(..)`.
fn present<'de, D: Deserializer<'de>, T: Deserialize<'de>>(d: D) -> Result<Option<Option<T>>, D::Error> {
    Option::<T>::deserialize(d).map(Some)
}

pub fn user_from_row(r: &SqliteRow) -> anyhow::Result<User> {
    Ok(User {
        id: r.try_get("id")?,
        name: r.try_get("name")?,
        role: Role::from_db(&r.try_get::<String, _>("role")?)?,
        phone: r.try_get("phone")?,
        phone_verified_at: opt_from_db(r.try_get("phone_verified_at")?)?,
        email: r.try_get("email")?,
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
        disabled_at: opt_from_db(r.try_get("disabled_at")?)?,
    })
}

/// Enabled people first, then by name.
pub async fn list_users(ctx: &Ctx) -> anyhow::Result<Vec<User>> {
    let rows = sqlx::query("SELECT * FROM users ORDER BY disabled_at IS NOT NULL, name COLLATE NOCASE, id").fetch_all(ctx.db()).await?;
    rows.iter().map(user_from_row).collect()
}

pub async fn get_user(ctx: &Ctx, id: &str) -> anyhow::Result<Option<User>> {
    let row = sqlx::query("SELECT * FROM users WHERE id = ?").bind(id).fetch_optional(ctx.db()).await?;
    row.map(|r| user_from_row(&r)).transpose()
}

/// The enabled person with this phone number (any format [`normalize_phone`] reads).
pub async fn user_by_phone(ctx: &Ctx, e164: &str) -> anyhow::Result<Option<User>> {
    let Some(phone) = normalize_phone(e164) else { return Ok(None) };
    let row = sqlx::query("SELECT * FROM users WHERE phone = ? AND disabled_at IS NULL").bind(phone).fetch_optional(ctx.db()).await?;
    row.map(|r| user_from_row(&r)).transpose()
}

fn clean_name(name: &str) -> ApiResult<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ApiError::bad_request("The person needs a name."));
    }
    if name.chars().count() > 100 {
        return Err(ApiError::bad_request("The name is too long."));
    }
    Ok(name.to_owned())
}

fn clean_phone(phone: Option<&str>) -> ApiResult<Option<String>> {
    match phone.map(str::trim).filter(|p| !p.is_empty()) {
        None => Ok(None),
        Some(p) => normalize_phone(p).map(Some).ok_or_else(|| ApiError::bad_request("Phone numbers look like +15155550123.")),
    }
}

fn clean_email(email: Option<&str>) -> ApiResult<Option<String>> {
    match email.map(str::trim).filter(|e| !e.is_empty()) {
        None => Ok(None),
        Some(e) => {
            let ok = e.len() <= 254
                && !e.chars().any(char::is_whitespace)
                && e.split_once('@')
                    .is_some_and(|(local, domain)| !local.is_empty() && domain.contains('.') && !domain.starts_with('.') && !domain.ends_with('.'));
            if ok { Ok(Some(e.to_owned())) } else { Err(ApiError::bad_request("That email address doesn't look right.")) }
        }
    }
}

/// 409 when another person already has the phone or email.
async fn check_unique(ctx: &Ctx, id: Option<&str>, phone: Option<&str>, email: Option<&str>) -> ApiResult<()> {
    if let Some(p) = phone {
        let taken: Option<(String,)> =
            sqlx::query_as("SELECT name FROM users WHERE phone = ? AND id IS NOT ?").bind(p).bind(id).fetch_optional(ctx.db()).await?;
        if let Some((name,)) = taken {
            return Err(ApiError::conflict(format!("{name} already has that phone number.")));
        }
    }
    if let Some(e) = email {
        let taken: Option<(String,)> =
            sqlx::query_as("SELECT name FROM users WHERE lower(email) = lower(?) AND id IS NOT ?").bind(e).bind(id).fetch_optional(ctx.db()).await?;
        if let Some((name,)) = taken {
            return Err(ApiError::conflict(format!("{name} already has that email address.")));
        }
    }
    Ok(())
}

fn unique_violation(e: &sqlx::Error) -> bool {
    e.as_database_error().is_some_and(|d| d.is_unique_violation())
}

/// Normalizes the phone (E.164); 400 on a bad name, phone or email; 409 on a
/// phone or email another person has.
pub async fn create_user(ctx: &Ctx, u: NewUser) -> ApiResult<User> {
    let user = User {
        id: id::new_id(USER),
        name: clean_name(&u.name)?,
        role: u.role,
        phone: clean_phone(u.phone.as_deref())?,
        phone_verified_at: None,
        email: clean_email(u.email.as_deref())?,
        created_at: now(),
        disabled_at: None,
    };
    check_unique(ctx, None, user.phone.as_deref(), user.email.as_deref()).await?;
    let res = sqlx::query("INSERT INTO users (id, name, role, phone, phone_verified_at, email, created_at, disabled_at) VALUES (?, ?, ?, ?, NULL, ?, ?, NULL)")
        .bind(&user.id)
        .bind(&user.name)
        .bind(user.role.as_db())
        .bind(&user.phone)
        .bind(&user.email)
        .bind(to_db(&user.created_at))
        .execute(ctx.db())
        .await;
    match res {
        Ok(_) => Ok(user),
        Err(e) if unique_violation(&e) => Err(ApiError::conflict("Someone already has that phone number or email address.")),
        Err(e) => Err(e.into()),
    }
}

/// Changing the phone clears `phone_verified_at`. `disabled: true` sets
/// `disabled_at` (kept if already set); `false` clears it. 404 when missing.
pub async fn update_user(ctx: &Ctx, id: &str, p: UserPatch) -> ApiResult<User> {
    let mut u = get_user(ctx, id).await?.ok_or_else(|| ApiError::not_found("No such person."))?;
    if let Some(name) = &p.name {
        u.name = clean_name(name)?;
    }
    if let Some(role) = p.role {
        u.role = role;
    }
    if let Some(phone) = &p.phone {
        let phone = clean_phone(phone.as_deref())?;
        if phone != u.phone {
            u.phone = phone;
            u.phone_verified_at = None;
        }
    }
    if let Some(email) = &p.email {
        u.email = clean_email(email.as_deref())?;
    }
    match p.disabled {
        Some(true) if u.disabled_at.is_none() => u.disabled_at = Some(now()),
        Some(false) => u.disabled_at = None,
        _ => {}
    }
    check_unique(ctx, Some(&u.id), u.phone.as_deref(), u.email.as_deref()).await?;
    let res = sqlx::query("UPDATE users SET name = ?, role = ?, phone = ?, phone_verified_at = ?, email = ?, disabled_at = ? WHERE id = ?")
        .bind(&u.name)
        .bind(u.role.as_db())
        .bind(&u.phone)
        .bind(u.phone_verified_at.as_ref().map(to_db))
        .bind(&u.email)
        .bind(u.disabled_at.as_ref().map(to_db))
        .bind(&u.id)
        .execute(ctx.db())
        .await;
    match res {
        Ok(_) => Ok(u),
        Err(e) if unique_violation(&e) => Err(ApiError::conflict("Someone already has that phone number or email address.")),
        Err(e) => Err(e.into()),
    }
}

/// The person proved the phone with a one-time code at `at`.
pub async fn set_phone_verified(ctx: &Ctx, id: &str, at: DateTime<Utc>) -> anyhow::Result<()> {
    let n = sqlx::query("UPDATE users SET phone_verified_at = ? WHERE id = ? AND phone IS NOT NULL")
        .bind(to_db(&at))
        .bind(id)
        .execute(ctx.db())
        .await?
        .rows_affected();
    anyhow::ensure!(n == 1, "no person {id} with a phone");
    Ok(())
}

/// E.164 (`+` and 8 to 15 digits, no leading 0). Spaces, dashes, dots and
/// brackets are ignored; 10 digits are a US number (`+1…`), as are 11 digits
/// starting with 1; `00` stands for `+`.
pub fn normalize_phone(s: &str) -> Option<String> {
    let s = s.trim();
    let (plus, rest) = match s.strip_prefix('+') {
        Some(r) => (true, r),
        None => (false, s),
    };
    let mut digits = String::with_capacity(rest.len());
    for c in rest.chars() {
        match c {
            '0'..='9' => digits.push(c),
            ' ' | '-' | '.' | '(' | ')' => {}
            _ => return None,
        }
    }
    let e164 = if plus {
        digits
    } else if let Some(intl) = digits.strip_prefix("00") {
        intl.to_owned()
    } else if digits.len() == 10 {
        format!("1{digits}")
    } else if digits.len() == 11 && digits.starts_with('1') {
        digits
    } else {
        return None;
    };
    let ok = (8..=15).contains(&e164.len()) && !e164.starts_with('0');
    ok.then(|| format!("+{e164}"))
}
