//! Alert and message records. The alert engine (op-alerts) opens, updates and
//! resolves alerts; `crate::messages` is the outbox and inbox every channel
//! shares.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::LonLat;
use crate::domain::DbEnum;
use crate::identity::Actor;
use crate::severity::Severity;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AlertStatus {
    Open,
    Acked,
    Resolved,
}

impl DbEnum for AlertStatus {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Alert {
    /// `alr_…`
    pub id: String,
    /// The rule kind, e.g. `outside`, `silent`.
    pub kind: String,
    /// Dedupe key: `<kind>:<subject id>`; rollups `<kind>:herd:<herd id>`.
    pub key: String,
    pub severity: Severity,
    pub status: AlertStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
    /// "214 outside P3", "31 outside P3".
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// Where to ring it on the map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub at: Option<LonLat>,
    /// `("collar" | "animal" | "decision" | "move" | "paddock" | "schedule", id)`.
    #[serde(default)]
    pub targets: Vec<(String, String)>,
    /// Rule data, e.g. `decision_waiting`: `{"code": "4821"}`.
    #[serde(default)]
    pub data: Value,
    pub opened_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acked_by: Option<Actor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_at: Option<DateTime<Utc>>,
    /// `None` with `resolved_at` set: it cleared by itself.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolved_by: Option<Actor>,
    /// Resolved because it joined a herd rollup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rolled_into: Option<String>,
}

/// One row of the `messages` table: a text, email, webhook or push going out,
/// or a text that came in.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MessageLog {
    /// `ntf_…`
    pub id: String,
    /// `out` | `in`
    pub direction: String,
    /// `sms` | `whatsapp` | `email` | `webhook` | `relay` | `push`
    pub channel: String,
    /// Phone, email, URL or push endpoint id.
    pub address: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_id: Option<String>,
    /// `alert` | `brief` | `reply` | `test` | `verify` | `inbound`
    pub kind: String,
    pub text: String,
    /// Email subject.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    /// `queued` | `sending` | `sent` | `delivered` | `failed` | `received` | `ignored`
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    /// Send attempts so far (each `claim` counts one).
    #[serde(default)]
    pub attempts: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}
