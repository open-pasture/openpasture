//! Collar data wins over imported history: an imported point is left out
//! when the animal's collar already recorded that time, so the dwell of one
//! animal-hour is never counted twice in pasture history (collar days plus
//! imported days).
//!
//! "Recorded that time" is read at the dwell rule's own resolution (a fix
//! counts for at most 30 minutes after it):
//! - hot fixes (still in `fixes`): each collar's first fix in each 30-minute
//!   bucket of the import's span, one index seek per bucket, credited to the
//!   animal that fix carries;
//! - rolled days (`analytics_paddock_days`, fixes already in Parquet): a collar
//!   day covers the stretch its dwell sums to, ending with its last fix, and
//!   is credited to the animal the collar is on now (the day rows keep no
//!   animal; a collar that moved between animals within those days credits
//!   its current one).

use std::collections::HashMap;

use op_core::Ctx;
use sqlx::Row;

use super::dwell::MAX_DWELL_GAP_MS;

const DAY_MS: i64 = 86_400_000;
/// Hot coverage buckets: the longest time one fix counts for.
const BUCKET_MS: i64 = MAX_DWELL_GAP_MS;

/// Per animal, the merged `[from, to)` stretches (unix ms) its collar recorded.
#[derive(Debug, Default)]
pub struct Covered {
    spans: HashMap<String, Vec<(i64, i64)>>,
}

impl Covered {
    fn add(&mut self, animal: &str, from: i64, to: i64) {
        if to > from {
            self.spans.entry(animal.to_owned()).or_default().push((from, to));
        }
    }

    fn merge(mut self) -> Self {
        for spans in self.spans.values_mut() {
            spans.sort_unstable();
            let mut out: Vec<(i64, i64)> = Vec::with_capacity(spans.len());
            for &(a, b) in spans.iter() {
                match out.last_mut() {
                    Some(last) if a <= last.1 => last.1 = last.1.max(b),
                    _ => out.push((a, b)),
                }
            }
            *spans = out;
        }
        self
    }

    /// The animal's collar recorded `t`.
    pub fn contains(&self, animal: &str, t: i64) -> bool {
        let Some(spans) = self.spans.get(animal) else { return false };
        let i = spans.partition_point(|s| s.1 <= t);
        spans.get(i).is_some_and(|s| s.0 <= t)
    }

    pub fn is_empty(&self) -> bool {
        self.spans.is_empty()
    }
}

/// Next collar with hot fixes after `?1` (`fixes_collar_t`).
pub const NEXT_COLLAR_SQL: &str = "SELECT collar_id FROM fixes WHERE collar_id > ? ORDER BY collar_id LIMIT 1";
/// Whether the collar has a hot fix in `[?2, ?3]`.
pub const IN_SPAN_SQL: &str = "SELECT 1 FROM fixes WHERE collar_id = ? AND t >= ? AND t <= ? LIMIT 1";
/// A collar's first fix in each of `?3` buckets of `?4` ms from `?2`: the
/// bucket number and the animal the fix carries, one seek each.
pub const BUCKETS_SQL: &str = "WITH RECURSIVE b(k) AS (SELECT 0 UNION ALL SELECT k + 1 FROM b WHERE k + 1 < ?3)
     SELECT b.k, f.animal_id FROM b JOIN fixes f ON f.id = (
         SELECT id FROM fixes WHERE collar_id = ?1 AND t >= ?2 + b.k * ?4 AND t < ?2 + (b.k + 1) * ?4 ORDER BY t, id LIMIT 1)";
/// Rolled collar days in `[?1, ?2]` (dates): each collar's dwell and last fix.
pub const ROLLED_SQL: &str = "SELECT date, collar_id, SUM(dwell_s), MAX(last_t) FROM analytics_paddock_days
     WHERE date >= ? AND date <= ? AND fixes > 0 GROUP BY date, collar_id";

/// What collars recorded between `from` and `to` (unix ms, inclusive).
pub async fn collar_coverage(ctx: &Ctx, from: i64, to: i64) -> anyhow::Result<Covered> {
    let mut covered = Covered::default();
    if to < from {
        return Ok(covered);
    }

    // Hot fixes: only the part of the span they hold.
    let (lo, hi): (Option<i64>, Option<i64>) = sqlx::query_as("SELECT MIN(t), MAX(t) FROM fixes").fetch_one(ctx.db()).await?;
    if let (Some(lo), Some(hi)) = (lo, hi) {
        let start = from.max(lo).div_euclid(BUCKET_MS) * BUCKET_MS;
        let end = to.min(hi);
        if end >= start {
            let buckets = (end - start).div_euclid(BUCKET_MS) + 1;
            let mut after = String::new();
            while let Some(c) = sqlx::query_scalar::<_, String>(NEXT_COLLAR_SQL).bind(&after).fetch_optional(ctx.db()).await? {
                let here: Option<i64> = sqlx::query_scalar(IN_SPAN_SQL).bind(&c).bind(start).bind(end).fetch_optional(ctx.db()).await?;
                if here.is_none() {
                    after = c;
                    continue;
                }
                let rows = sqlx::query(BUCKETS_SQL).bind(&c).bind(start).bind(buckets).bind(BUCKET_MS).fetch_all(ctx.db()).await?;
                for r in rows {
                    let (k, animal): (i64, Option<String>) = (r.try_get(0)?, r.try_get(1)?);
                    if let Some(a) = animal {
                        let b = start + k * BUCKET_MS;
                        covered.add(&a, b, b + BUCKET_MS);
                    }
                }
                after = c;
            }
        }
    }

    // Rolled days: the stretch each collar day's dwell covers, ending at its last fix.
    let date = |ms: i64| op_core::time::from_unix_ms(ms).format("%Y-%m-%d").to_string();
    let rolled = sqlx::query(ROLLED_SQL).bind(date(from)).bind(date(to)).fetch_all(ctx.db()).await?;
    if !rolled.is_empty() {
        let wearer: HashMap<String, String> = sqlx::query_as::<_, (String, String)>("SELECT id, animal_id FROM collars WHERE animal_id IS NOT NULL")
            .fetch_all(ctx.db())
            .await?
            .into_iter()
            .collect();
        for r in rolled {
            let (day, collar, dwell_s, last_t): (String, String, f64, i64) = (r.try_get(0)?, r.try_get(1)?, r.try_get(2)?, r.try_get(3)?);
            let (Some(animal), Ok(d)) = (wearer.get(&collar), chrono::NaiveDate::parse_from_str(&day, "%Y-%m-%d")) else { continue };
            let day_start = d.and_hms_opt(0, 0, 0).map(|t| t.and_utc().timestamp_millis()).unwrap_or(last_t);
            let end = (last_t + MAX_DWELL_GAP_MS).min(day_start + DAY_MS);
            let begin = (end - (dwell_s * 1000.0) as i64).max(day_start);
            covered.add(animal, begin, end);
        }
    }
    Ok(covered.merge())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stretches_merge_and_answer_by_binary_search() {
        let mut c = Covered::default();
        c.add("a", 10, 20);
        c.add("a", 40, 50);
        c.add("a", 18, 30);
        c.add("b", 0, 5);
        c.add("a", 60, 60);
        let c = c.merge();
        assert_eq!(c.spans["a"], vec![(10, 30), (40, 50)]);
        for (t, want) in [(9, false), (10, true), (29, true), (30, false), (45, true), (50, false), (60, false)] {
            assert_eq!(c.contains("a", t), want, "{t}");
        }
        assert!(c.contains("b", 4) && !c.contains("b", 5) && !c.contains("z", 4));
    }
}
