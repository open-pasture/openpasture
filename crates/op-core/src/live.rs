//! What `/api/live` sends instead of one message per fix, ack and cue: every
//! 500 ms, per herd, one `positions`, one `ack_batch` and one `cue_batch`
//! message ([`crate::Event::Positions`], [`crate::Event::AckBatch`],
//! [`crate::Event::CueBatch`]). op-server's live layer builds them from the
//! bus for the WebSocket only; they are never published on the bus, so
//! server-side subscribers keep receiving the single events.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{AckStatus, FenceState, Fix};

/// The latest position and telemetry of one collar that changed in the batch
/// window: its newest fix, fence state, battery (0-1) and last contact.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PositionItem {
    pub collar_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animal_id: Option<String>,
    pub fix: Fix,
    pub state: FenceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
}

/// A collar's latest boundary ack in the batch window. A collar whose
/// reported boundary version moved without an ack shows as `applied`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AckItem {
    pub collar_id: String,
    pub version: u32,
    pub status: AckStatus,
    /// Reject code, from collars that send one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    /// The collar's own words for a rejection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// One cue, as the single `cue` event carries it. Every cue in the window is
/// kept, in the order they arrived.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CueItem {
    pub collar_id: String,
    pub at: DateTime<Utc>,
    pub level: u8,
    pub margin_m: f64,
    /// Exactly `warn` or `outside`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ring: Option<u16>,
}

/// Collar keys that change with every report. A `collar` event whose JSON
/// differs from the last one sent only in these is not sent on its own:
/// `boundary_version` rides in `ack_batch`, the rest in `positions`.
pub const COLLAR_VOLATILE_KEYS: [&str; 6] = ["last_seen", "battery", "last_fix", "state", "outside_since", "boundary_version"];

/// Every collar as stored now: where the live layer starts comparing from.
pub async fn stored_collars(store: &crate::Store) -> anyhow::Result<Vec<crate::Collar>> {
    let rows = sqlx::query("SELECT * FROM collars").fetch_all(store.pool()).await?;
    rows.iter().map(crate::store::collar_from_row).collect()
}

/// The herd a collar is in now; `None` once it is deleted.
pub async fn collar_herd(store: &crate::Store, collar_id: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT herd_id FROM collars WHERE id = ?").bind(collar_id).fetch_optional(store.pool()).await?)
}

/// Whether a person's token still opens the feed as it did: not revoked, the
/// person enabled, the role unchanged. A socket that no longer holds closes
/// (the browser reconnects as whoever it is now, or is refused).
pub async fn session_holds(store: &crate::Store, token_id: &str, role: crate::Role) -> anyhow::Result<bool> {
    use crate::domain::DbEnum;
    let now: Option<String> = sqlx::query_scalar(
        "SELECT u.role FROM user_tokens t JOIN users u ON u.id = t.user_id WHERE t.id = ? AND t.revoked_at IS NULL AND u.disabled_at IS NULL",
    )
    .bind(token_id)
    .fetch_optional(store.pool())
    .await?;
    Ok(now.is_some_and(|r| r == role.as_db()))
}
