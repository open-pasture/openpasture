//! Episodes for collars that don't report them (firmware 0.1, whose cues
//! carry no kind), rebuilt from what the server stored: each fix with the
//! server fence's state and margin (the same shape that collar enforces), and
//! the cues. The rules are the collar's own (`op_geo::cue::EpisodeTracker`):
//! an episode starts at a warning cue and ends turned back at an inside fix,
//! crossed at an outside one, rest at a warning fix where the 20 s cap
//! silenced the collar, or boundary changed when the fix names a newer
//! boundary. Rebuilt episodes are stored in `episodes` with `derived = 1`.
//!
//! A background task reads the firmware 0.1 cues that arrived since its last
//! run (by row id, `welfare_marks`), and per collar rebuilds from the first of
//! them onwards, replacing the derived episodes there. An episode whose end
//! hasn't arrived yet waits for the next run; one with nothing stored after it
//! for 10 minutes is left out.

use std::collections::{BTreeMap, HashMap};

use op_core::{Ctx, FenceState};
use sqlx::Row;

/// Tone length firmware 0.1 plays per cue (`CueConfig::beep_ms`); it doesn't
/// report one.
pub const LEGACY_BEEP_MS: u32 = 300;
/// Waiting longer than this for an episode's end means it will never come
/// from what is stored.
const STALE_MS: i64 = 10 * 60_000;
/// New cues read per run; the rest wait for the next.
const BATCH: i64 = 20_000;
/// Oldest fix a rebuild reads back to (older rows have left for Parquet).
const LOOKBACK_MS: i64 = 5 * 86_400_000;

/// One fix of a collar as the collar's cue policy saw it, and the cue it
/// played there, if any.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Step {
    pub t: i64,
    pub state: FenceState,
    pub margin_m: Option<f64>,
    pub boundary_version: Option<u32>,
    pub cue: Option<StepCue>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StepCue {
    pub id: i64,
    pub warn: bool,
    pub level: u8,
    pub margin_m: f64,
    pub ring: Option<u32>,
}

/// A firmware 0.1 cue as stored.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LegacyCue {
    pub id: i64,
    pub t: i64,
    pub level: u8,
    pub margin_m: f64,
    pub boundary_version: Option<u32>,
}

impl LegacyCue {
    /// Firmware 0.1 sends no kind: outside when past the line, else warn.
    pub fn warn(&self) -> bool {
        self.margin_m >= 0.0
    }
}

/// A rebuilt episode.
#[derive(Debug, Clone, PartialEq)]
pub struct Derived {
    pub start: i64,
    pub end: i64,
    pub ring: u32,
    pub cues: u32,
    pub max_level: u8,
    pub min_margin_m: f64,
    pub outcome: &'static str,
    pub boundary_version: Option<u32>,
    /// The cue that started it.
    pub first_cue: i64,
}

/// Episodes that ended, and the one still running at the last step.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Run {
    pub closed: Vec<Derived>,
    pub open: Option<Derived>,
}

/// Fixes `(t, state, margin, boundary version)` and cues in one time line. A
/// cue belongs to the fix it was played at (the same time); a cue with no
/// stored fix stands for one: warning when it is a warning cue, else outside.
pub fn steps(fixes: &[(i64, FenceState, Option<f64>, Option<u32>)], cues: &[LegacyCue]) -> Vec<Step> {
    let mut by_t: BTreeMap<i64, Step> = BTreeMap::new();
    for (t, state, margin_m, bv) in fixes {
        by_t.entry(*t).or_insert(Step { t: *t, state: *state, margin_m: *margin_m, boundary_version: *bv, cue: None });
    }
    for c in cues {
        let cue = StepCue { id: c.id, warn: c.warn(), level: c.level, margin_m: c.margin_m, ring: None };
        match by_t.get_mut(&c.t) {
            Some(s) if s.cue.is_none() => s.cue = Some(cue),
            Some(_) => {}
            None => {
                let state = if cue.warn { FenceState::Warning } else { FenceState::Outside };
                by_t.insert(c.t, Step { t: c.t, state, margin_m: Some(c.margin_m), boundary_version: c.boundary_version, cue: Some(cue) });
            }
        }
    }
    by_t.into_values().collect()
}

/// The collar's episode tracker over stored steps.
pub fn episodes(steps: &[Step]) -> Run {
    let mut run = Run::default();
    for s in steps {
        // A newer boundary rearmed the collar: a running episode ends there.
        if let (Some(ep), Some(now)) = (run.open.as_ref(), s.boundary_version) {
            if ep.boundary_version.is_some_and(|v| v != now) {
                let mut ep = run.open.take().expect("open");
                ep.end = s.t;
                ep.outcome = "boundary_changed";
                run.closed.push(ep);
            }
        }
        let warn = s.cue.filter(|c| c.warn);
        if let Some(ep) = run.open.as_mut() {
            if let Some(m) = s.margin_m.or(s.cue.map(|c| c.margin_m)) {
                ep.min_margin_m = ep.min_margin_m.min(m);
            }
            if let Some(c) = warn {
                ep.cues += 1;
                ep.max_level = ep.max_level.max(c.level);
            }
            let outcome = match s.state {
                FenceState::Outside => Some("crossed"),
                FenceState::Inside => Some("turned_back"),
                // Armed in the warning zone and silent: the 20 s cap began a rest.
                FenceState::Warning if warn.is_none() => Some("rest"),
                _ => None,
            };
            if let Some(o) = outcome {
                let mut ep = run.open.take().expect("open");
                ep.end = s.t;
                ep.outcome = o;
                run.closed.push(ep);
            }
            continue;
        }
        if let Some(c) = warn {
            run.open = Some(Derived {
                start: s.t,
                end: s.t,
                ring: c.ring.unwrap_or(0),
                cues: 1,
                max_level: c.level,
                min_margin_m: s.margin_m.unwrap_or(c.margin_m),
                outcome: "turned_back",
                boundary_version: s.boundary_version,
                first_cue: c.id,
            });
        }
    }
    run
}

async fn mark(ctx: &Ctx) -> anyhow::Result<i64> {
    let m: Option<i64> = sqlx::query_scalar("SELECT max_id FROM welfare_marks WHERE source = 'cues'").fetch_optional(ctx.db()).await?;
    Ok(m.unwrap_or(0))
}

async fn set_mark(ctx: &Ctx, id: i64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO welfare_marks (source, max_id, updated_at) VALUES ('cues', ?, ?) \
         ON CONFLICT(source) DO UPDATE SET max_id = excluded.max_id, updated_at = excluded.updated_at",
    )
    .bind(id)
    .bind(op_core::time::to_db(&op_core::time::now()))
    .execute(ctx.db())
    .await?;
    Ok(())
}

/// What one pass did.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Pass {
    /// Collars whose episodes were rebuilt.
    pub collars: usize,
    /// Derived episodes stored.
    pub episodes: usize,
    /// Episodes still waiting for their end.
    pub waiting: usize,
}

/// One pass over the firmware 0.1 cues that arrived since the last.
pub async fn derive(ctx: &Ctx, now_ms: i64) -> anyhow::Result<Pass> {
    let since = mark(ctx).await?;
    // The newest id and the new cues from one snapshot, so the mark never
    // passes a cue this pass didn't see.
    let mut read = ctx.db().begin().await?;
    let top: Option<i64> = sqlx::query_scalar("SELECT MAX(id) FROM cues").fetch_one(&mut *read).await?;
    let top = top.unwrap_or(0);
    let rows = sqlx::query("SELECT id, collar_id, t FROM cues WHERE id > ? AND id <= ? AND kind IS NULL ORDER BY id LIMIT ?")
        .bind(since)
        .bind(top)
        .bind(BATCH)
        .fetch_all(&mut *read)
        .await?;
    read.rollback().await?;
    let full = rows.len() as i64 == BATCH;
    let mut first: HashMap<String, i64> = HashMap::new();
    let mut last_id = since;
    for r in &rows {
        let (id, collar, t): (i64, String, i64) = (r.try_get(0)?, r.try_get(1)?, r.try_get(2)?);
        last_id = last_id.max(id);
        first.entry(collar).and_modify(|x| *x = (*x).min(t)).or_insert(t);
    }
    let mut pass = Pass::default();
    let mut hold: Option<i64> = None;
    for (collar, t0) in &first {
        let (stored, waiting) = rebuild(ctx, collar, *t0, now_ms).await?;
        pass.collars += 1;
        pass.episodes += stored;
        if let Some(id) = waiting {
            pass.waiting += 1;
            hold = Some(hold.map_or(id, |h: i64| h.min(id)));
        }
    }
    let next = match hold {
        Some(h) => (h - 1).max(since),
        None if full => last_id,
        None => top.max(since),
    };
    if next != since {
        set_mark(ctx, next).await?;
    }
    Ok(pass)
}

/// Rebuild one collar's derived episodes from `t0` on. Returns how many were
/// stored and, when one is still running, the id of the cue that started it.
async fn rebuild(ctx: &Ctx, collar: &str, t0: i64, now_ms: i64) -> anyhow::Result<(usize, Option<i64>)> {
    // Start at the beginning of a derived episode that reaches t0, so an
    // episode is always rebuilt whole.
    let back: Option<i64> = sqlx::query_scalar("SELECT MIN(start_t) FROM episodes WHERE collar_id = ? AND derived = 1 AND end_t >= ?")
        .bind(collar)
        .bind(t0)
        .fetch_one(ctx.db())
        .await?;
    let from = back.map_or(t0, |b| b.min(t0)).max(now_ms - LOOKBACK_MS);
    let fixes: Vec<(i64, FenceState, Option<f64>, Option<u32>)> =
        sqlx::query("SELECT t, state, margin_m, boundary_version FROM fixes WHERE collar_id = ? AND t >= ? ORDER BY t")
            .bind(collar)
            .bind(from)
            .fetch_all(ctx.db())
            .await?
            .iter()
            .map(|r| {
                let state: Option<String> = r.try_get(1).ok().flatten();
                let bv: Option<i64> = r.try_get(3).ok().flatten();
                (
                    r.try_get::<i64, _>(0).unwrap_or_default(),
                    state.map_or(FenceState::Unknown, |s| FenceState::parse(&s)),
                    r.try_get(2).ok().flatten(),
                    bv.map(|v| v as u32),
                )
            })
            .collect();
    let rows = sqlx::query(
        "SELECT id, t, level, margin_m, boundary_version, herd_id, animal_id FROM cues WHERE collar_id = ? AND t >= ? AND kind IS NULL ORDER BY t, id",
    )
    .bind(collar)
    .bind(from)
    .fetch_all(ctx.db())
    .await?;
    let mut cues = Vec::with_capacity(rows.len());
    let mut whose: HashMap<i64, (Option<String>, Option<String>)> = HashMap::new();
    for r in &rows {
        let id: i64 = r.try_get(0)?;
        let bv: Option<i64> = r.try_get(4)?;
        cues.push(LegacyCue {
            id,
            t: r.try_get(1)?,
            level: r.try_get::<i64, _>(2)?.clamp(0, 255) as u8,
            margin_m: r.try_get(3)?,
            boundary_version: bv.map(|v| v as u32),
        });
        whose.insert(id, (r.try_get(5)?, r.try_get(6)?));
    }
    let steps = steps(&fixes, &cues);
    let run = episodes(&steps);
    let last_t = steps.last().map(|s| s.t);
    let waiting = run.open.as_ref().filter(|_| last_t.is_some_and(|t| now_ms - t < STALE_MS)).map(|e| e.first_cue);

    // Nothing new since the last pass (another collar's episode held the
    // mark back): keep the stored rows and their ids.
    let stored: Vec<(i64, i64, String, i64, i64)> =
        sqlx::query_as("SELECT start_t, end_t, outcome, cues, max_level FROM episodes WHERE collar_id = ? AND derived = 1 AND start_t >= ? ORDER BY start_t")
            .bind(collar)
            .bind(from)
            .fetch_all(ctx.db())
            .await?;
    let same = stored.len() == run.closed.len()
        && stored.iter().zip(&run.closed).all(|(s, e)| (s.0, s.1, s.2.as_str(), s.3, s.4) == (e.start, e.end, e.outcome, e.cues as i64, e.max_level as i64));
    if same {
        return Ok((0, waiting));
    }

    let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
    sqlx::query("DELETE FROM episodes WHERE collar_id = ? AND derived = 1 AND start_t >= ?").bind(collar).bind(from).execute(&mut *tx).await?;
    for e in &run.closed {
        let (herd, animal) = whose.get(&e.first_cue).cloned().unwrap_or_default();
        sqlx::query(
            "INSERT INTO episodes (id, collar_id, herd_id, animal_id, start_t, end_t, start_at, end_at, boundary_version, ring, cues, max_level, min_margin_m, outcome, derived)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 1) ON CONFLICT(collar_id, start_t) DO NOTHING",
        )
        .bind(op_core::id::new_id(super::EPISODE))
        .bind(collar)
        .bind(herd)
        .bind(animal)
        .bind(e.start)
        .bind(e.end)
        .bind(op_core::time::to_db(&op_core::time::from_unix_ms(e.start)))
        .bind(op_core::time::to_db(&op_core::time::from_unix_ms(e.end)))
        .bind(e.boundary_version.map(i64::from))
        .bind(e.ring as i64)
        .bind(e.cues as i64)
        .bind(e.max_level as i64)
        .bind(e.min_margin_m)
        .bind(e.outcome)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok((run.closed.len(), waiting))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fix(t: i64, state: FenceState, m: f64) -> (i64, FenceState, Option<f64>, Option<u32>) {
        (t, state, Some(m), Some(7))
    }

    fn cue(id: i64, t: i64, level: u8, m: f64) -> LegacyCue {
        LegacyCue { id, t, level, margin_m: m, boundary_version: Some(7) }
    }

    #[test]
    fn a_warning_run_that_ends_inside_turned_back() {
        let fixes = [
            fix(0, FenceState::Inside, 9.0),
            fix(1000, FenceState::Warning, 4.0),
            fix(2000, FenceState::Warning, 2.0),
            fix(3000, FenceState::Warning, 3.0),
            fix(4000, FenceState::Inside, 6.5),
        ];
        let cues = [cue(1, 1000, 1, 4.0), cue(2, 2000, 3, 2.0), cue(3, 3000, 2, 3.0)];
        let run = episodes(&steps(&fixes, &cues));
        assert_eq!(run.open, None);
        assert_eq!(run.closed.len(), 1);
        let e = &run.closed[0];
        assert_eq!((e.start, e.end, e.cues, e.max_level, e.outcome, e.first_cue), (1000, 4000, 3, 3, "turned_back", 1));
        assert_eq!(e.min_margin_m, 2.0);
    }

    #[test]
    fn crossing_rest_and_a_new_boundary_end_episodes_too() {
        // Crossed: warning, then outside with the outside tone.
        let crossed = [fix(0, FenceState::Warning, 1.0), fix(1000, FenceState::Outside, -1.5)];
        let run = episodes(&steps(&crossed, &[cue(1, 0, 4, 1.0), cue(2, 1000, 4, -1.5)]));
        assert_eq!(run.closed[0].outcome, "crossed");
        assert_eq!(run.closed[0].cues, 1, "the outside tone isn't a warning cue");
        assert_eq!(run.closed[0].min_margin_m, -1.5);
        // Rest: still in the warning zone, and the collar went quiet.
        let rest = [fix(0, FenceState::Warning, 2.0), fix(20_000, FenceState::Warning, 2.0)];
        assert_eq!(episodes(&steps(&rest, &[cue(1, 0, 2, 2.0)])).closed[0].outcome, "rest");
        // A fix under a newer boundary.
        let mut changed = vec![fix(0, FenceState::Warning, 2.0)];
        changed.push((1000, FenceState::Warning, Some(2.0), Some(8)));
        let run = episodes(&steps(&changed, &[cue(1, 0, 2, 2.0)]));
        assert_eq!((run.closed[0].outcome, run.closed[0].end), ("boundary_changed", 1000));
        // Still running at the end.
        let open = episodes(&steps(&[fix(0, FenceState::Warning, 2.0)], &[cue(9, 0, 2, 2.0)]));
        assert!(open.closed.is_empty());
        assert_eq!(open.open.map(|e| e.first_cue), Some(9));
    }

    #[test]
    fn a_cue_without_its_fix_stands_in_for_it() {
        let run = episodes(&steps(&[], &[cue(1, 0, 2, 3.0), cue(2, 1000, 4, -0.5)]));
        assert_eq!(run.closed.len(), 1);
        assert_eq!(run.closed[0].outcome, "crossed");
    }
}
