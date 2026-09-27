use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{AckStatus, Boundary, Collar, Decision, Escape, FenceState, Fix, Move};
use crate::identity::Role;

/// Live events, streamed as JSON on `/api/live`. Publish with
/// [`crate::Ctx::publish`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    Fix {
        collar_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        animal_id: Option<String>,
        herd_id: String,
        fix: Fix,
        state: FenceState,
    },
    Cue {
        collar_id: String,
        at: DateTime<Utc>,
        level: u8,
        margin_m: f64,
        /// Exactly `warn` or `outside` (a string: op-geo's `CueKind` is newer than this crate's users).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
        /// Ring the cue was about: 0 outer, 1.. holes.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ring: Option<u16>,
    },
    Ack {
        collar_id: String,
        herd_id: String,
        version: u32,
        status: AckStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Collar {
        collar: Collar,
    },
    Boundary {
        herd_id: String,
        boundary: Boundary,
    },
    Decision {
        decision: Decision,
    },
    /// A move started, stepped, dropped a straggler, finished or stopped.
    Move {
        #[serde(rename = "move")]
        r#move: Move,
    },
    /// An escape started, stepped, ended or was stopped.
    Escape {
        escape: Escape,
    },
    /// Brain progress, one line at a time.
    DecisionLog {
        decision_id: String,
        line: String,
    },
    /// Sent to one `/api/live` client that fell behind and missed events:
    /// refetch state instead of trusting the stream.
    Resync,
    // @HUB
    /// An alert opened, changed, was acked or resolved.
    Alert {
        alert: crate::alert::Alert,
    },
    /// A message was queued, sent, failed or came in. Managers and up only.
    Message {
        message: crate::alert::MessageLog,
    },
    /// A map feature was created, changed or (`deleted`) removed.
    Feature {
        feature: crate::features::MapFeature,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        deleted: bool,
    },
    /// Animals were added, changed, removed or imported.
    AnimalsChanged {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        herd_id: Option<String>,
    },
    // @HUB-UI
    // @E-lib
    // @E-srv
    // @J
    // @A-engine
    // @A-notify
    // @D
    // @K-animals
    // @K-files
    // @I
    // @B
    // @G
    // @P
    /// Per herd, every 500 ms: the collars whose position or telemetry changed.
    /// Built by op-server for `/api/live` only, never published on the bus.
    Positions {
        herd_id: String,
        items: Vec<crate::live::PositionItem>,
    },
    /// Per herd, every 500 ms: each collar's latest boundary ack. `/api/live` only.
    AckBatch {
        herd_id: String,
        items: Vec<crate::live::AckItem>,
    },
    /// Per herd, every 500 ms: every cue in the window. `/api/live` only.
    CueBatch {
        herd_id: String,
        items: Vec<crate::live::CueItem>,
    },
    // @Q
    // @C
    // @F
    // @S
    // @A3
    // @H
    // @L
    // @M
    // @Z
}

impl Event {
    /// Who may see this event on `/api/live`: `message` events carry phone
    /// numbers and go to managers and up; everything else to any role.
    pub fn min_role(&self) -> Role {
        match self {
            Event::Message { .. } => Role::Manager,
            _ => Role::Viewer,
        }
    }
}
