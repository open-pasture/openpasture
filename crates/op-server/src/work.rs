//! What the server is in the middle of, for whoever must not stop it
//! unawares: the desktop app asks before quitting, keeps the Mac awake and
//! puts off installing an update while any of it is going on.

use chrono::Duration;
use op_core::Ctx;
use serde::Serialize;

/// A collar heard from within this long counts as live.
pub const LIVE_WITHIN: Duration = Duration::minutes(15);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Work {
    /// Moves sweeping a herd's fence.
    pub moves: u32,
    /// Escapes bringing an animal back.
    pub escapes: u32,
    /// Collars off the shelf heard from in the last 15 minutes.
    pub live_collars: u32,
}

impl Work {
    /// A move or escape is running: stopping now freezes a fence mid-way.
    pub fn moving(&self) -> bool {
        self.moves > 0 || self.escapes > 0
    }

    /// Anything a sleeping or quitting server would let down.
    pub fn any(&self) -> bool {
        self.moving() || self.live_collars > 0
    }
}

pub async fn current(ctx: &Ctx) -> anyhow::Result<Work> {
    let since = op_core::time::to_db(&(op_core::time::now() - LIVE_WITHIN));
    let (moves, escapes, live_collars): (i64, i64, i64) = sqlx::query_as(
        "SELECT (SELECT COUNT(*) FROM moves WHERE status = 'sweeping'),
                (SELECT COUNT(*) FROM escapes WHERE status = 'returning'),
                (SELECT COUNT(*) FROM collars WHERE parked_at IS NULL AND last_seen >= ?)",
    )
    .bind(since)
    .fetch_one(ctx.db())
    .await?;
    Ok(Work { moves: moves as u32, escapes: escapes as u32, live_collars: live_collars as u32 })
}
