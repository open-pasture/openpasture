//! Pure calculations over fixes: distance, dwell per paddock, grid binning,
//! percentiles, fix cadence and trends. Fed row by row so a scan never has to
//! sit in memory.

use std::collections::{BTreeMap, HashMap};

use op_core::Paddock;
use op_geo::{LonLat, Polygon, Projection};

/// Mean Earth radius (IUGG), metres.
const EARTH_R_M: f64 = 6_371_008.8;

/// Great-circle distance in metres.
pub fn haversine_m(a: LonLat, b: LonLat) -> f64 {
    let (la1, la2) = (a[1].to_radians(), b[1].to_radians());
    let dlat = la2 - la1;
    let dlon = (b[0] - a[0]).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + la1.cos() * la2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_R_M * h.sqrt().min(1.0).asin()
}

/// Faster than any cow runs; anything above is a bad fix.
const MAX_SPEED_MS: f64 = 12.0;
/// Accuracy assumed when a fix doesn't report one.
const DEFAULT_ACCURACY_M: f64 = 10.0;
/// Never count steps shorter than this, whatever the reported accuracy.
const MIN_STEP_M: f64 = 1.0;

/// Distance walked by one collar. A step only counts once the animal has
/// moved further from the last counted point than either fix's accuracy, so
/// GNSS jitter while standing adds nothing. Fixes implying an impossible
/// speed are skipped.
#[derive(Debug, Default, Clone)]
pub struct Walk {
    anchor: Option<(i64, LonLat, f64)>,
    last_t: i64,
    pub metres: f64,
}

impl Walk {
    pub fn push(&mut self, t: i64, p: LonLat, accuracy_m: Option<f64>) {
        if self.anchor.is_some() && t <= self.last_t {
            return;
        }
        self.last_t = t;
        let acc = accuracy_m.filter(|a| *a > 0.0).unwrap_or(DEFAULT_ACCURACY_M);
        let Some((at, ap, aacc)) = self.anchor else {
            self.anchor = Some((t, p, acc));
            return;
        };
        let d = haversine_m(ap, p);
        if d <= acc.max(aacc).max(MIN_STEP_M) {
            return;
        }
        let dt_s = (t - at) as f64 / 1000.0;
        if dt_s > 0.0 && d / dt_s > MAX_SPEED_MS {
            return;
        }
        self.metres += d;
        self.anchor = Some((t, p, acc));
    }
}

/// Paddocks with bounding boxes for quick point-in-polygon.
pub struct PaddockIndex {
    items: Vec<(String, [f64; 4], Polygon)>,
}

impl PaddockIndex {
    pub fn new(paddocks: &[Paddock]) -> Self {
        let items = paddocks.iter().filter_map(|p| Some((p.id.clone(), p.geometry.bbox()?, p.geometry.clone()))).collect();
        Self { items }
    }

    pub fn locate(&self, p: LonLat) -> Option<&str> {
        self.items.iter().find(|(_, b, poly)| p[0] >= b[0] && p[1] >= b[1] && p[0] <= b[2] && p[1] <= b[3] && poly.contains(p)).map(|(id, _, _)| id.as_str())
    }

    /// The paddock ingest recorded for the fix if it still exists, else the
    /// one containing it.
    pub fn resolve<'a>(&'a self, stored: Option<&str>, p: LonLat) -> Option<&'a str> {
        if let Some(s) = stored {
            if let Some((id, _, _)) = self.items.iter().find(|(id, _, _)| id == s) {
                return Some(id.as_str());
            }
        }
        self.locate(p)
    }
}

/// Longest gap between fixes still counted as time spent where the first
/// fix was. Longer gaps are unknown time.
pub const MAX_DWELL_GAP_MS: i64 = 30 * 60 * 1000;
const DAY_MS: i64 = 86_400_000;

/// Dwell key: (UTC day number, herd, collar, paddock). Paddock and herd are
/// empty strings when unknown.
pub type DwellKey = (i64, String, String, String);

#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct DwellAgg {
    pub fixes: i64,
    pub dwell_ms: i64,
    pub last_t: i64,
}

/// Time each collar spends in each paddock per UTC day. The time until the
/// next fix (capped at [`MAX_DWELL_GAP_MS`] and the end of the day) belongs
/// to the paddock of the earlier fix.
#[derive(Default)]
pub struct Dwell {
    prev: HashMap<String, (i64, String, String)>,
    pub by_key: HashMap<DwellKey, DwellAgg>,
}

impl Dwell {
    pub fn push(&mut self, collar: &str, herd: Option<&str>, paddock: Option<&str>, t: i64) {
        let herd = herd.unwrap_or("");
        let paddock = paddock.unwrap_or("");
        if let Some((pt, ph, pp)) = self.prev.get(collar) {
            if t < *pt {
                return;
            }
            let dt = (t - pt).min(MAX_DWELL_GAP_MS).min(day_end(*pt) - pt);
            let key = (pt.div_euclid(DAY_MS), ph.clone(), collar.to_owned(), pp.clone());
            self.by_key.entry(key).or_default().dwell_ms += dt;
        }
        let e = self.by_key.entry((t.div_euclid(DAY_MS), herd.to_owned(), collar.to_owned(), paddock.to_owned())).or_default();
        e.fixes += 1;
        e.last_t = e.last_t.max(t);
        self.prev.insert(collar.to_owned(), (t, herd.to_owned(), paddock.to_owned()));
    }

    /// Credit each collar's last fix up to `end_ms` (the end of the range or
    /// now), within the same limits.
    pub fn finish(mut self, end_ms: i64) -> HashMap<DwellKey, DwellAgg> {
        for (collar, (pt, ph, pp)) in std::mem::take(&mut self.prev) {
            let dt = (end_ms - pt).clamp(0, MAX_DWELL_GAP_MS).min(day_end(pt) - pt);
            let key = (pt.div_euclid(DAY_MS), ph, collar, pp);
            self.by_key.entry(key).or_default().dwell_ms += dt;
        }
        self.by_key
    }
}

fn day_end(t: i64) -> i64 {
    (t.div_euclid(DAY_MS) + 1) * DAY_MS
}

/// Grid binning in metres around an origin. Cells are keyed by their integer
/// east/north index.
pub struct Grid {
    proj: Projection,
    cell_m: f64,
    pub cells: HashMap<(i64, i64), f64>,
}

impl Grid {
    pub fn new(origin: LonLat, cell_m: f64) -> Self {
        Self { proj: Projection::new(origin), cell_m, cells: HashMap::new() }
    }

    pub fn add(&mut self, p: LonLat, w: f64) {
        let [x, y] = self.proj.forward(p);
        let key = ((x / self.cell_m).floor() as i64, (y / self.cell_m).floor() as i64);
        *self.cells.entry(key).or_default() += w;
    }

    /// `[lon, lat, weight]` at each cell's centre, sorted by cell. With
    /// `normalize` the heaviest cell is 1.
    pub fn points(&self, normalize: bool) -> Vec<[f64; 3]> {
        let max = self.cells.values().cloned().fold(0.0, f64::max);
        let mut keys: Vec<_> = self.cells.keys().copied().collect();
        keys.sort();
        keys.into_iter()
            .map(|k| {
                let c = self.proj.inverse([(k.0 as f64 + 0.5) * self.cell_m, (k.1 as f64 + 0.5) * self.cell_m]);
                let w = self.cells[&k];
                let w = if normalize && max > 0.0 { round(w / max, 4) } else { w };
                [round(c[0], 7), round(c[1], 7), w]
            })
            .collect()
    }
}

/// Linear-interpolated percentile, `q` in 0..=1. Sorts `v`.
pub fn percentile(v: &mut [f64], q: f64) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    let pos = q.clamp(0.0, 1.0) * (v.len() - 1) as f64;
    let (lo, hi) = (pos.floor() as usize, pos.ceil() as usize);
    Some(v[lo] + (v[hi] - v[lo]) * (pos - lo as f64))
}

/// Typical seconds between fixes: the median gap, from a histogram of gaps
/// rounded to whole seconds. Batched reports don't matter since fixes carry
/// their own time.
#[derive(Default, Clone)]
pub struct Cadence {
    last_t: Option<i64>,
    gaps: HashMap<i64, u64>,
}

impl Cadence {
    pub fn push(&mut self, t: i64) {
        if let Some(prev) = self.last_t {
            if t > prev {
                *self.gaps.entry(((t - prev) as f64 / 1000.0).round().max(1.0) as i64).or_default() += 1;
            }
        }
        self.last_t = Some(self.last_t.map_or(t, |p| p.max(t)));
    }

    /// `n` gaps of `secs` seconds (whole, at least 1) counted elsewhere (@L:
    /// the hot fixes' gaps, from SQLite).
    pub fn add_gaps(&mut self, secs: i64, n: u64) {
        *self.gaps.entry(secs.max(1)).or_default() += n;
    }

    pub fn median_s(&self) -> Option<f64> {
        let n: u64 = self.gaps.values().sum();
        if n == 0 {
            return None;
        }
        let mut keys: Vec<_> = self.gaps.keys().copied().collect();
        keys.sort();
        let mut seen = 0;
        for k in keys {
            seen += self.gaps[&k];
            if seen * 2 >= n {
                return Some(k as f64);
            }
        }
        None
    }
}

// @L
/// Accuracy values and how often each came, for percentiles without keeping
/// every fix: keyed by centimetres (fixes carry decimetres or coarser).
#[derive(Debug, Default, Clone)]
pub struct Hist {
    counts: BTreeMap<i64, u64>,
}

impl Hist {
    pub fn add(&mut self, v: f64, n: u64) {
        if v.is_finite() && n > 0 {
            *self.counts.entry((v * 100.0).round() as i64).or_default() += n;
        }
    }

    pub fn merge(&mut self, other: &Hist) {
        for (k, n) in &other.counts {
            *self.counts.entry(*k).or_default() += n;
        }
    }

    /// As [`percentile`] of every value once per count.
    pub fn percentile(&self, q: f64) -> Option<f64> {
        let n: u64 = self.counts.values().sum();
        if n == 0 {
            return None;
        }
        let pos = q.clamp(0.0, 1.0) * (n - 1) as f64;
        let (lo, hi) = (pos.floor() as u64, pos.ceil() as u64);
        let at = |i: u64| {
            let mut seen = 0;
            for (k, c) in &self.counts {
                seen += c;
                if i < seen {
                    return *k as f64 / 100.0;
                }
            }
            *self.counts.keys().last().expect("not empty") as f64 / 100.0
        };
        let (a, b) = (at(lo), at(hi));
        Some(a + (b - a) * (pos - lo as f64))
    }
}

/// Least-squares slope of `y` against its index.
pub fn slope(y: &[f64]) -> Option<f64> {
    let n = y.len();
    if n < 2 {
        return None;
    }
    let mx = (n - 1) as f64 / 2.0;
    let my = y.iter().sum::<f64>() / n as f64;
    let (mut num, mut den) = (0.0, 0.0);
    for (i, v) in y.iter().enumerate() {
        let dx = i as f64 - mx;
        num += dx * (v - my);
        den += dx * dx;
    }
    Some(num / den)
}

/// Animal units per head, from the agent kit's `calculations.py`.
pub fn au_per_head(species: &str) -> f64 {
    match species.trim().to_ascii_lowercase().as_str() {
        "cattle" | "cow" | "beef" | "bison" => 1.0,
        "dairy" => 1.4,
        "horse" | "horses" => 1.25,
        "sheep" => 0.2,
        "goat" | "goats" => 0.15,
        _ => 1.0,
    }
}

pub fn round(v: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    (v * m).round() / m
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGIN: LonLat = [-79.2, 38.1];

    fn at(east: f64, north: f64) -> LonLat {
        Projection::new(ORIGIN).offset(east, north)
    }

    #[test]
    fn haversine_known_distance() {
        // One degree of latitude is about 111.2 km.
        let d = haversine_m([0.0, 0.0], [0.0, 1.0]);
        assert!((d - 111_195.0).abs() < 10.0, "{d}");
        let d = haversine_m(at(0.0, 0.0), at(300.0, 400.0));
        assert!((d - 500.0).abs() < 1.0, "{d}");
    }

    #[test]
    fn walk_ignores_jitter_and_counts_steps() {
        let mut w = Walk::default();
        // Standing still for 10 minutes with 4 m jitter and 5 m accuracy.
        for i in 0..600 {
            let j = if i % 2 == 0 { 2.0 } else { -2.0 };
            w.push(i * 1000, at(j, 0.0), Some(5.0));
        }
        assert_eq!(w.metres, 0.0);
        // Then 200 m east in 10 m steps, 10 s apart.
        let mut w = Walk::default();
        for i in 0..=20 {
            w.push(i * 10_000, at(i as f64 * 10.0, 0.0), Some(5.0));
        }
        assert!((w.metres - 200.0).abs() < 1.0, "{}", w.metres);
        // Slow walking below the accuracy still adds up.
        let mut w = Walk::default();
        for i in 0..=100 {
            w.push(i * 1000, at(i as f64 * 0.5, 0.0), Some(5.0));
        }
        assert!((w.metres - 50.0).abs() < 6.0, "{}", w.metres);
        // A 2 km jump in a second is a bad fix.
        let mut w = Walk::default();
        w.push(0, at(0.0, 0.0), Some(5.0));
        w.push(1000, at(2000.0, 0.0), Some(5.0));
        w.push(2000, at(0.0, 0.0), Some(5.0));
        assert_eq!(w.metres, 0.0);
    }

    #[test]
    fn dwell_splits_by_paddock_and_caps_gaps() {
        let mut d = Dwell::default();
        let base = 1_790_000_000_000 - 1_790_000_000_000 % DAY_MS; // midnight
        // One hour in A at one fix a minute, then one hour in B.
        for i in 0..60 {
            d.push("c1", Some("h1"), Some("A"), base + i * 60_000);
        }
        for i in 60..120 {
            d.push("c1", Some("h1"), Some("B"), base + i * 60_000);
        }
        let m = d.finish(base + 120 * 60_000);
        let get = |p: &str| m[&(base / DAY_MS, "h1".into(), "c1".into(), p.into())];
        assert_eq!(get("A").dwell_ms, 3_600_000);
        assert_eq!(get("B").dwell_ms, 3_600_000);
        assert_eq!(get("A").fixes, 60);
        // A three hour gap only counts the cap.
        let mut d = Dwell::default();
        d.push("c1", None, Some("A"), base);
        d.push("c1", None, Some("A"), base + 3 * 3_600_000);
        let m = d.finish(base + 3 * 3_600_000);
        assert_eq!(m[&(base / DAY_MS, "".into(), "c1".into(), "A".into())].dwell_ms, MAX_DWELL_GAP_MS);
    }

    #[test]
    fn grid_bins_by_metres() {
        let mut g = Grid::new(ORIGIN, 10.0);
        g.add(at(1.0, 1.0), 1.0);
        g.add(at(9.0, 9.0), 1.0);
        g.add(at(5.0, 5.0), 1.0);
        g.add(at(15.0, 1.0), 1.0);
        g.add(at(-1.0, 1.0), 1.0);
        assert_eq!(g.cells.len(), 3);
        assert_eq!(g.cells[&(0, 0)], 3.0);
        assert_eq!(g.cells[&(1, 0)], 1.0);
        assert_eq!(g.cells[&(-1, 0)], 1.0);
        let pts = g.points(true);
        let heavy = pts.iter().find(|p| p[2] == 1.0).unwrap();
        let centre = at(5.0, 5.0);
        assert!(haversine_m([heavy[0], heavy[1]], centre) < 0.05);
        assert!(pts.iter().any(|p| (p[2] - 0.3333).abs() < 1e-9));
    }

    #[test]
    fn percentiles_cadence_slope() {
        let mut v = vec![5.0, 1.0, 3.0, 2.0, 4.0];
        assert_eq!(percentile(&mut v, 0.5), Some(3.0));
        assert_eq!(percentile(&mut v, 0.95), Some(4.8));
        let mut c = Cadence::default();
        for i in 0..100 {
            c.push(i * 30_000);
        }
        c.push(100 * 30_000 + 3_600_000);
        assert_eq!(c.median_s(), Some(30.0));
        assert_eq!(slope(&[10.0, 8.0, 6.0, 4.0]), Some(-2.0));
        assert_eq!(au_per_head("Sheep"), 0.2);
    }
}
