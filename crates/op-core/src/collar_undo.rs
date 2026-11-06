//! Undo window for claims and same-farm herd moves.
//!
//! **24 hours is a default pending the owner's OK** of the final window.
//! Change [`UNDO_WINDOW_HOURS`] (or later a setting) once that is decided.

use chrono::Duration;

/// Hours a claim or same-farm herd move can be undone via
/// `POST /api/collars/{id}/undo`. Default pending owner confirmation.
pub const UNDO_WINDOW_HOURS: i64 = 24;

/// [`UNDO_WINDOW_HOURS`] as a [`Duration`].
pub fn undo_window() -> Duration {
    Duration::hours(UNDO_WINDOW_HOURS)
}
