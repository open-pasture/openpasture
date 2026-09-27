//! The collars rules look at, and when a collar counts as silent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use chrono::{DateTime, Duration, Utc};
use op_core::{Collar, Ctx};
use sqlx::Row;

/// One collar on duty: not parked, and its animal (if any) not removed.
#[derive(Debug, Clone)]
pub struct Unit {
    pub collar: Collar,
    /// What people call it: the animal's tag, else the collar's name.
    pub label: String,
}

#[derive(Debug, Clone, Default)]
pub struct HerdInfo {
    pub name: String,
    /// The paddock the herd is in, by name.
    pub paddock: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Fleet {
    pub units: Vec<Unit>,
    pub herds: HashMap<String, HerdInfo>,
}

impl Fleet {
    pub fn herd(&self, id: &str) -> HerdInfo {
        self.herds.get(id).cloned().unwrap_or_default()
    }
}

/// Every collar on duty, by herd, with the herds' names and paddocks.
pub async fn fleet(ctx: &Ctx) -> anyhow::Result<Fleet> {
    let rows = sqlx::query(
        "SELECT c.*, a.tag AS a_tag FROM collars c LEFT JOIN animals a ON a.id = c.animal_id
         WHERE c.parked_at IS NULL AND (a.id IS NULL OR a.removed_at IS NULL) ORDER BY c.herd_id, c.id",
    )
    .fetch_all(ctx.db())
    .await?;
    let mut units = Vec::with_capacity(rows.len());
    for r in &rows {
        let collar = op_core::store::collar_from_row(r)?;
        let tag: Option<String> = r.try_get("a_tag")?;
        let label = tag.filter(|t| !t.trim().is_empty()).unwrap_or_else(|| collar.name.clone());
        units.push(Unit { collar, label });
    }
    let herds = sqlx::query("SELECT h.id, h.name, p.name AS p_name FROM herds h LEFT JOIN paddocks p ON p.id = h.paddock_id")
        .fetch_all(ctx.db())
        .await?
        .iter()
        .map(|r| Ok((r.try_get::<String, _>("id")?, HerdInfo { name: r.try_get("name")?, paddock: r.try_get("p_name")? })))
        .collect::<anyhow::Result<_>>()?;
    Ok(Fleet { units, herds })
}

/// Reports looked at for a collar's usual interval.
const INTERVAL_SAMPLE: i64 = 49;
/// How long a collar's usual interval is trusted before it is read again.
const INTERVAL_TTL: Duration = Duration::minutes(10);

type IntervalCache = HashMap<(PathBuf, String), (DateTime<Utc>, Option<f64>)>;
static INTERVALS: LazyLock<Mutex<IntervalCache>> = LazyLock::new(Default::default);

/// A collar's median seconds between reports over the last 24 h, from its
/// latest reports there (a bounded read on `health(collar_id, t)`), cached
/// for ten minutes. `None` with fewer than two reports.
async fn median_interval_s(ctx: &Ctx, collar_id: &str, now: DateTime<Utc>) -> anyhow::Result<Option<f64>> {
    let key = (ctx.data_dir().to_path_buf(), collar_id.to_owned());
    if let Some((at, v)) = INTERVALS.lock().unwrap_or_else(|e| e.into_inner()).get(&key).copied()
        && now >= at
        && now - at < INTERVAL_TTL
    {
        return Ok(v);
    }
    let since = (now - Duration::hours(24)).timestamp_millis();
    let ts: Vec<i64> = sqlx::query_scalar("SELECT t FROM health WHERE collar_id = ? AND t >= ? ORDER BY t DESC LIMIT ?")
        .bind(collar_id)
        .bind(since)
        .bind(INTERVAL_SAMPLE)
        .fetch_all(ctx.db())
        .await?;
    let mut gaps: Vec<f64> = ts.windows(2).map(|w| (w[0] - w[1]) as f64 / 1000.0).filter(|g| *g > 0.0).collect();
    let v = median(&mut gaps);
    INTERVALS.lock().unwrap_or_else(|e| e.into_inner()).insert(key, (now, v));
    Ok(v)
}

pub(crate) fn median(v: &mut [f64]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(f64::total_cmp);
    let n = v.len();
    Some(if n % 2 == 1 { v[n / 2] } else { (v[n / 2 - 1] + v[n / 2]) / 2.0 })
}

/// How long each collar may go without a report before it counts as silent:
/// `max(after_min, 3 × its median report interval over 24 h)`. Collars that
/// never reported are left out (nothing to go silent from).
pub async fn silence(ctx: &Ctx, units: &[Unit], after_min: u32, now: DateTime<Utc>) -> anyhow::Result<HashMap<String, Duration>> {
    let floor = Duration::minutes(after_min as i64);
    let mut out = HashMap::with_capacity(units.len());
    for u in units.iter().filter(|u| u.collar.last_seen.is_some()) {
        let usual = median_interval_s(ctx, &u.collar.id, now).await?;
        let limit = usual.map(|s| Duration::milliseconds((3.0 * s * 1000.0) as i64)).map_or(floor, |d| d.max(floor));
        out.insert(u.collar.id.clone(), limit);
    }
    Ok(out)
}

/// Whether a collar has been silent past its limit at `now`.
pub(crate) fn is_silent(u: &Unit, limits: &HashMap<String, Duration>, now: DateTime<Utc>) -> bool {
    match (u.collar.last_seen, limits.get(&u.collar.id)) {
        (Some(seen), Some(limit)) => now - seen >= *limit,
        _ => false,
    }
}
