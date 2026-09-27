//! Who is asking: roles, how they reached the server, and who did what.
//!
//! op-server's guard puts an [`Identity`] into the extensions of every
//! request: the resolved one on `/api` and `/mcp`, [`Identity::anonymous`]
//! everywhere else. Handlers take it as an extractor, which fails with 500
//! when it is missing, so a router wired without the guard fails loudly
//! instead of acting as the owner. Tests that call feature routers directly
//! wrap them with [`with_identity`].

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::domain::DbEnum;
use crate::error::{ApiError, ApiResult};
use crate::{Ctx, users};

/// Ordered: `Viewer < Hand < Manager < Owner`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Viewer,
    Hand,
    Manager,
    Owner,
}

impl DbEnum for Role {}

/// How the request reached the server.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    /// A request from this machine (see op-server's `auth`).
    Local,
    /// The app token (`openpasture token`).
    AppToken,
    /// A person's own token.
    UserToken,
    /// A brain run's MCP token.
    Brain,
    /// A text message from a known phone.
    Text,
    /// Background jobs.
    System,
    /// No credentials: collar, hosted and webhook endpoints, the UI.
    Anonymous,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Identity {
    pub role: Role,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub via: Via,
}

/// Who did something, stored on records (decision responses, alert acks,
/// schedules, imports).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Actor {
    pub via: Via,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The 403 message for a role that is too low.
pub const FORBIDDEN: &str = "Your role can't do this.";

impl Identity {
    pub fn owner(via: Via) -> Self {
        Self { role: Role::Owner, user_id: None, name: None, via }
    }

    /// Background jobs: owner, via `system`.
    pub fn system() -> Self {
        Self::owner(Via::System)
    }

    /// A brain run: viewer, via `brain`.
    pub fn brain() -> Self {
        Self { role: Role::Viewer, user_id: None, name: None, via: Via::Brain }
    }

    /// No credentials. [`Identity::can`] is always false.
    pub fn anonymous() -> Self {
        Self { role: Role::Viewer, user_id: None, name: None, via: Via::Anonymous }
    }

    pub fn is_anonymous(&self) -> bool {
        self.via == Via::Anonymous
    }

    /// False when anonymous, else `role >= need`.
    pub fn can(&self, need: Role) -> bool {
        !self.is_anonymous() && self.role >= need
    }

    /// Anonymous → 401; a role below `need` → 403 `{"error":"Your role can't do this."}`.
    pub fn require(&self, need: Role) -> ApiResult<()> {
        if self.is_anonymous() {
            return Err(ApiError::unauthorized("Sign in first."));
        }
        if !self.can(need) {
            return Err(ApiError::forbidden(FORBIDDEN));
        }
        Ok(())
    }

    pub fn actor(&self) -> Actor {
        Actor { via: self.via, user_id: self.user_id.clone(), name: self.name.clone() }
    }
}

impl<S: Send + Sync> FromRequestParts<S> for Identity {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts.extensions.get::<Identity>().cloned().ok_or_else(|| ApiError::internal("Identity missing"))
    }
}

/// Give every request through `r` this identity. For tests that call feature
/// routers without op-server's guard: `with_identity(router, Identity::owner(Via::Local))`.
pub fn with_identity<S: Clone + Send + Sync + 'static>(r: Router<S>, id: Identity) -> Router<S> {
    r.layer(axum::Extension(id))
}

// `GET /api/me`

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Me {
    pub role: Role,
    pub via: Via,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user: Option<MeUser>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct MeUser {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phone_verified: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
}

/// What `GET /api/me` answers for an identity: its role and how it came in,
/// plus the person when the identity is one.
pub async fn me(ctx: &Ctx, id: &Identity) -> anyhow::Result<Me> {
    let user = match &id.user_id {
        Some(uid) => users::get_user(ctx, uid).await?.map(|u| MeUser {
            id: u.id,
            name: u.name,
            phone_verified: u.phone.as_ref().map(|_| u.phone_verified_at.is_some()),
            phone: u.phone,
            email: u.email,
        }),
        None => None,
    };
    Ok(Me { role: id.role, via: id.via, user })
}

async fn get_me(State(ctx): State<Ctx>, id: Identity) -> ApiResult<Json<Me>> {
    Ok(Json(me(&ctx, &id).await?))
}

pub(crate) fn router() -> Router<Ctx> {
    Router::new().route("/api/me", get(get_me))
}
