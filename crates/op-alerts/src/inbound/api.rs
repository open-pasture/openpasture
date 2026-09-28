//! `GET/PUT /api/texting` and `PUT /api/texting/people/{id}` (owner).
//!
//! The view is the `texting` setting plus, read-only: `inbound_mode`
//! (`webhook | polling | relay | off`), the webhook URLs to give Twilio
//! (webhook mode), the last check of Twilio or the relay (`checked`), and
//! each person's brief and STOP state (`people`).

use axum::extract::{Path, State};
use axum::routing::{get, put};
use axum::{Json, Router};
use op_core::notify_config::configured_channels;
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Identity, Role, patch};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::state::{self, State as LoopState};
use super::{Mode, TextingConfig, hhmm, load, mode, public_url, save};
use crate::brief_send;

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/texting", get(get_texting).put(put_texting)).route("/api/texting/people/{id}", put(put_person))
}

/// Where Twilio should post texts (Messaging → "A message comes in").
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Hooks {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sms: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub whatsapp: Option<String>,
}

/// A person's texting state.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PersonTexting {
    pub user_id: String,
    /// Gets the morning brief by text.
    pub brief: bool,
    /// Texted STOP: gets no texts until START.
    pub sms_opt_out: bool,
    /// A browser of theirs takes notifications (push reaches them).
    pub push: bool,
}

#[derive(Debug, Serialize)]
pub struct TextingView {
    #[serde(flatten)]
    pub config: TextingConfig,
    pub inbound_mode: Mode,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hooks: Option<Hooks>,
    /// The last check of Twilio (polling) or the relay's inbox (relay).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked: Option<LoopState>,
    pub people: Vec<PersonTexting>,
}

async fn person(ctx: &Ctx, user_id: &str) -> anyhow::Result<PersonTexting> {
    let row: Option<(bool, bool)> =
        sqlx::query_as("SELECT brief, sms_opt_out FROM alert_prefs WHERE user_id = ?").bind(user_id).fetch_optional(ctx.db()).await?;
    let (brief, sms_opt_out) = row.unwrap_or((false, false));
    let push: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM push_subscriptions WHERE user_id = ?)").bind(user_id).fetch_one(ctx.db()).await?;
    Ok(PersonTexting { user_id: user_id.to_owned(), brief, sms_opt_out, push })
}

pub async fn view(ctx: &Ctx) -> ApiResult<TextingView> {
    let config = load(ctx).await?;
    let inbound_mode = mode(ctx).await?;
    let hooks = match (inbound_mode, public_url(ctx).await?) {
        (Mode::Webhook, Some(base)) => {
            let own = configured_channels(ctx).await?;
            Some(Hooks {
                sms: own.contains(&"sms").then(|| format!("{base}/hooks/twilio/sms")),
                whatsapp: own.contains(&"whatsapp").then(|| format!("{base}/hooks/twilio/whatsapp")),
            })
        }
        _ => None,
    };
    let checked = match inbound_mode {
        Mode::Polling => {
            let mut last: Option<LoopState> = None;
            for c in super::twilio_channels(ctx).await? {
                if let Some(s) = state::get(ctx, &format!("poll:{c}")).await? {
                    // A failing number shows over one that works.
                    if last.as_ref().is_none_or(|l| l.error.is_none() && (s.error.is_some() || s.ran_at > l.ran_at)) {
                        last = Some(s);
                    }
                }
            }
            last
        }
        Mode::Relay => state::get(ctx, super::relay::KEY).await?,
        _ => None,
    };
    let mut people = Vec::new();
    for u in op_core::users::list_users(ctx).await? {
        people.push(person(ctx, &u.id).await?);
    }
    Ok(TextingView { config, inbound_mode, hooks, checked, people })
}

async fn get_texting(State(ctx): State<Ctx>, id: Identity) -> ApiResult<Json<TextingView>> {
    id.require(Role::Owner)?;
    Ok(Json(view(&ctx).await?))
}

/// A merge patch over the `texting` setting. The view's read-only parts may
/// be sent back and are ignored.
async fn put_texting(State(ctx): State<Ctx>, id: Identity, ApiJson(body): ApiJson<Value>) -> ApiResult<Json<TextingView>> {
    id.require(Role::Owner)?;
    let next: TextingConfig = patch::apply(&load(&ctx).await?, &body, &["inbound_mode", "hooks", "checked", "people"])?;
    if !(5..=300).contains(&next.poll_s) {
        return Err(ApiError::bad_request("poll_s is 5 to 300 seconds."));
    }
    if !(1..=72).contains(&next.approve_window_h) {
        return Err(ApiError::bad_request("approve_window_h is 1 to 72 hours."));
    }
    if hhmm(&next.brief.time).is_none() {
        return Err(ApiError::bad_request("brief.time is HH:MM."));
    }
    save(&ctx, &next).await?;
    Ok(Json(view(&ctx).await?))
}

#[derive(Debug, Deserialize)]
struct PersonBody {
    brief: bool,
}

async fn put_person(State(ctx): State<Ctx>, id: Identity, Path(user_id): Path<String>, ApiJson(b): ApiJson<PersonBody>) -> ApiResult<Json<PersonTexting>> {
    id.require(Role::Owner)?;
    if op_core::users::get_user(&ctx, &user_id).await?.is_none() {
        return Err(ApiError::not_found("No such person."));
    }
    brief_send::set_brief(&ctx, &user_id, b.brief).await?;
    Ok(Json(person(&ctx, &user_id).await?))
}
