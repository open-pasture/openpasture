//! Per-day paddock dwell of imported fixes, written to `imported_paddock_days`
//! so pasture history (rest days, last grazed) includes the history. The rule
//! is op-analytics' rollup rule, so imported and collar days add up alike:
//! the time until the next fix (at most 30 minutes, never past the end of the
//! UTC day) belongs to the paddock of the earlier fix.

use std::collections::HashMap;

use op_core::Paddock;
use op_geo::{LonLat, Polygon};

/// Longest gap between fixes still counted as time spent where the first
/// fix was (op-analytics' `MAX_DWELL_GAP_MS`).
pub const MAX_DWELL_GAP_MS: i64 = 30 * 60 * 1000;
const DAY_MS: i64 = 86_400_000;

/// Which paddock holds a point, by bounding box then polygon.
pub struct PaddockIndex {
    items: Vec<(String, [f64; 4], Polygon)>,
}

impl PaddockIndex {
    pub fn new(paddocks: &[Paddock]) -> Self {
        Self { items: paddocks.iter().filter_map(|p| Some((p.id.clone(), p.geometry.bbox()?, p.geometry.clone()))).collect() }
    }

    pub fn locate(&self, p: LonLat) -> Option<&str> {
        self.items.iter().find(|(_, b, poly)| p[0] >= b[0] && p[1] >= b[1] && p[0] <= b[2] && p[1] <= b[3] && poly.contains(p)).map(|(id, _, _)| id.as_str())
    }
}

/// (UTC day number, herd, animal, paddock); paddock is "" outside every paddock.
pub type Key = (i64, String, String, String);

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Agg {
    pub fixes: i64,
    pub dwell_ms: i64,
    pub last_t: i64,
}

#[derive(Default)]
pub struct Dwell {
    prev: HashMap<String, (i64, String, String)>,
    pub by_key: HashMap<Key, Agg>,
}

impl Dwell {
    /// One fix of `animal`, in time order per animal.
    pub fn push(&mut self, animal: &str, herd: &str, paddock: Option<&str>, t: i64) {
        let paddock = paddock.unwrap_or("");
        if let Some((pt, ph, pp)) = self.prev.get(animal) {
            if t < *pt {
                return;
            }
            let dt = (t - pt).min(MAX_DWELL_GAP_MS).min(day_end(*pt) - pt);
            self.by_key.entry((pt.div_euclid(DAY_MS), ph.clone(), animal.to_owned(), pp.clone())).or_default().dwell_ms += dt;
        }
        let e = self.by_key.entry((t.div_euclid(DAY_MS), herd.to_owned(), animal.to_owned(), paddock.to_owned())).or_default();
        e.fixes += 1;
        e.last_t = e.last_t.max(t);
        self.prev.insert(animal.to_owned(), (t, herd.to_owned(), paddock.to_owned()));
    }

    /// Credit each animal's last fix up to the gap limit (and the day's end):
    /// history has no "now" to run to.
    pub fn finish(mut self) -> HashMap<Key, Agg> {
        for (animal, (pt, ph, pp)) in std::mem::take(&mut self.prev) {
            let dt = MAX_DWELL_GAP_MS.min(day_end(pt) - pt);
            self.by_key.entry((pt.div_euclid(DAY_MS), ph, animal, pp)).or_default().dwell_ms += dt;
        }
        self.by_key
    }
}

fn day_end(t: i64) -> i64 {
    (t.div_euclid(DAY_MS) + 1) * DAY_MS
}

/// `YYYY-MM-DD` of a UTC day number.
pub fn date_of_day(day: i64) -> String {
    op_core::time::from_unix_ms(day * DAY_MS).format("%Y-%m-%d").to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const H: i64 = 3_600_000;

    #[test]
    fn gaps_cap_at_thirty_minutes_and_split_at_midnight() {
        let mut d = Dwell::default();
        let day = 20_000 * DAY_MS;
        d.push("a", "h", Some("p1"), day + 10 * H);
        d.push("a", "h", Some("p1"), day + 10 * H + 10 * 60_000); // 10 min later
        d.push("a", "h", Some("p2"), day + 13 * H); // 3 h gap: 30 min to p1
        d.push("a", "h", Some("p2"), day + 24 * H - 5 * 60_000); // last 5 min of the day
        d.push("a", "h", Some("p2"), day + 24 * H + 10 * 60_000); // next day
        let got = d.finish();
        let p1 = got[&(20_000, "h".into(), "a".into(), "p1".into())];
        assert_eq!((p1.fixes, p1.dwell_ms), (2, 40 * 60_000));
        let p2 = got[&(20_000, "h".into(), "a".into(), "p2".into())];
        assert_eq!((p2.fixes, p2.dwell_ms), (2, 35 * 60_000), "30 min after 13:00, then 5 min to midnight");
        let next = got[&(20_001, "h".into(), "a".into(), "p2".into())];
        assert_eq!((next.fixes, next.dwell_ms), (1, 30 * 60_000));
        assert_eq!(date_of_day(20_000), "2024-10-04");
    }
}
