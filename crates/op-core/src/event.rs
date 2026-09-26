use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{AckStatus, Boundary, Collar, Decision, FenceState, Fix, Move};

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
    /// Brain progress, one line at a time.
    DecisionLog {
        decision_id: String,
        line: String,
    },
    /// Sent to one `/api/live` client that fell behind and missed events:
    /// refetch state instead of trusting the stream.
    Resync,
}
