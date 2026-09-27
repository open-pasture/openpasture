//! Strip schedules (field-ready §2.14): a herd walked across a paddock's
//! strips on a cadence, each open staged on the collars ahead of time so it
//! happens without the server.
//!
//! A schedule copies its strips (a layout re-cuts when its paddock is
//! reshaped). Opening strip k stages `strips[0 ..= k]` without a back fence;
//! with one it stages `strips[k-1-lag ..= k]` (the animals keep the ground
//! they stand on) and then closes the back fence to `strips[k-lag ..= k]` in
//! `close_steps` staged steps that sweep the oldest strip. Each open and each
//! close step is a [`ScheduledMove`]; op-ingest stores them in
//! `schedule_moves` and stages them as boundaries with `effective_at`.

use chrono::{DateTime, NaiveTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{DbEnum, Polygon};
use crate::identity::Actor;

/// Id prefix of schedules.
pub const SCHEDULE: &str = "sch";

/// How far an open may run late and still be applied (an immediate sequence
/// held it up). Later than this it is marked `late` and never applied.
pub const LATE_AFTER_MIN: i64 = 30;

/// The fence behind the herd: it closes over the oldest strip after an open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BackFence {
    pub enabled: bool,
    /// Strips the herd keeps behind the one it is on.
    pub lag_strips: u32,
    /// Minutes after an open before the first close step.
    pub close_after_min: u32,
    /// Staged steps the close takes.
    pub close_steps: u32,
    /// Minutes between close steps.
    pub close_every_min: u32,
}

impl Default for BackFence {
    fn default() -> Self {
        Self { enabled: true, lag_strips: 0, close_after_min: 240, close_steps: 3, close_every_min: 10 }
    }
}

/// When opens happen: every `every_days` days at `at` farm time. Occurrence
/// times are computed in farm time for each occurrence, so "daily 07:00"
/// opens at 07:00 local on both sides of a DST change.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Cadence {
    pub every_days: u32,
    /// "HH:MM", farm time.
    pub at: String,
}

impl Default for Cadence {
    fn default() -> Self {
        Self { every_days: 1, at: "07:00".into() }
    }
}

impl Cadence {
    /// The time of day, when `at` reads as "HH:MM".
    pub fn time(&self) -> Option<NaiveTime> {
        NaiveTime::parse_from_str(self.at.trim(), "%H:%M").ok()
    }

    /// Farmer-readable problems: 400 material.
    pub fn check(&self) -> Result<(), String> {
        if !(1..=60).contains(&self.every_days) {
            return Err("Open a strip every 1 to 60 days.".into());
        }
        if self.time().is_none() {
            return Err("The open time must read like 07:00.".into());
        }
        Ok(())
    }
}

impl BackFence {
    pub fn check(&self) -> Result<(), String> {
        if !self.enabled {
            return Ok(());
        }
        if self.lag_strips > 10 {
            return Err("Keep at most 10 strips behind the herd.".into());
        }
        if !(1..=12).contains(&self.close_steps) {
            return Err("The back fence closes in 1 to 12 steps.".into());
        }
        if self.close_after_min > 24 * 60 * 7 || !(1..=24 * 60).contains(&self.close_every_min) {
            return Err("Back fence minutes are out of range.".into());
        }
        Ok(())
    }

    /// Minutes from an open to its last close step (0 without a back fence).
    pub fn span_min(&self) -> u32 {
        if self.enabled { self.close_after_min + self.close_every_min * self.close_steps.saturating_sub(1) } else { 0 }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ScheduleStatus {
    Active,
    Paused,
    Done,
}

impl DbEnum for ScheduleStatus {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Schedule {
    pub id: String,
    pub herd_id: String,
    pub paddock_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout_id: Option<String>,
    /// Copied when the schedule was made.
    pub strips: Vec<Polygon>,
    /// The next strip to open (0-based); `strips.len()` once every strip opened.
    pub next_index: u32,
    pub cadence: Cadence,
    /// The first open (occurrence 0).
    pub starts_at: DateTime<Utc>,
    pub back_fence: BackFence,
    pub status: ScheduleStatus,
    pub created_by: Actor,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    /// When the last strip was planned to be done with, as made (reports: planned vs actual).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub planned_end: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
}

/// Where a scheduled move stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MoveState {
    /// Not on the collars yet (waiting for room in their slots).
    Planned,
    /// Stored as a staged boundary.
    Staged,
    /// Took effect.
    Done,
    /// Never applied; `skipped` says why.
    Skipped,
}

impl DbEnum for MoveState {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduledMove {
    pub schedule_id: String,
    /// The strip (0-based).
    pub index: u32,
    /// 0 opens the strip, 1.. are back-fence close steps.
    pub step: u32,
    pub at: DateTime<Utc>,
    /// The boundary as planned, before it goes through `prepare`.
    pub geometry: Polygon,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_version: Option<u32>,
    /// `late` | `skipped` | `held`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skipped: Option<String>,
    pub state: MoveState,
    /// When the first collar applied it (its own clock).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_at: Option<DateTime<Utc>>,
}
