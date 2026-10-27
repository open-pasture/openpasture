//! What a collar enforces whatever its server sends (`messages.md` §4.7):
//! permanent no-go **zones**, and the **lease** a staged boundary needs
//! before it may take effect.
//!
//! - A `zones` command (type 12) is the farm's whole set of permanent no-go
//!   zones, with the **policy epoch** it belongs to. The collar holds it and
//!   enforces every zone as a hole of whatever boundary is active.
//! - A boundary names the epoch the server checked it against (key 21). It
//!   doesn't take effect while the collar holds an older epoch, and never
//!   when its fenced area overlaps a held zone (`in_exclusion`).
//! - A `lease` (type 13) says "as of now, epoch N with these zones is still
//!   current, until T". A staged boundary that names an epoch takes effect
//!   only under a current lease for the zones the collar holds, so a collar
//!   that missed a new zone (it was out of range) stops advancing its
//!   schedule within one lease window.

use op_geo::{CollarLimits, LonLat};

use super::cbor::{self, Value};
use super::command::{Body, Boundary, Command, Ring};
use super::crypto::sha256;
use super::registry::Reject;

/// Type 12: the permanent no-go zones (key 16) of policy epoch `epoch`
/// (key 17). The whole set: it replaces what the collar held.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Zones {
    pub zones: Vec<Ring>,
    pub epoch: u32,
}

/// Type 13: epoch `epoch` (key 16), whose zones have digest `digest` (key
/// 18), is current until `until` (key 17, GNSS time).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Lease {
    pub epoch: u32,
    pub until: u64,
    pub digest: [u8; 8],
}

/// Most zones a collar is asked to hold, and vertices across all of them,
/// unless its device record says otherwise.
pub const DEFAULT_ZONE_LIMITS: [u32; 2] = [16, 512];

/// A staged boundary is one that takes effect more than this long after it
/// was signed, seconds: it needs a lease.
pub const STAGED_AFTER_S: u64 = 600;

/// The digest a lease names: the first 8 bytes of SHA-256 of the zones'
/// key-16 value, as encoded.
pub fn zones_digest(zones: &[Ring]) -> [u8; 8] {
    let v = Value::Array(zones.iter().map(|r| ring_value(r)).collect());
    sha256(&[&cbor::encode(&v)])[..8].try_into().unwrap()
}

fn ring_value(r: &Ring) -> Value {
    // The boundary ring encoding: the first vertex, then differences.
    let mut out = Vec::with_capacity(r.len() * 2);
    let mut prev = [0i64; 2];
    for (i, p) in r.iter().enumerate() {
        let cur = [i64::from(p[0]), i64::from(p[1])];
        let d = if i == 0 { cur } else { [cur[0] - prev[0], cur[1] - prev[1]] };
        out.push(Value::int(d[0]));
        out.push(Value::int(d[1]));
        prev = cur;
    }
    Value::Array(out)
}

impl Zones {
    /// v1's ring rules for each zone (`too_few_vertices`,
    /// `self_intersecting`, `zero_area`, `out_of_range`), then the collar's
    /// limits: `too_many_holes` past `limits[0]` zones, `too_many_vertices`
    /// past `limits[1]` vertices in all. Zones may overlap each other.
    pub fn check(&self, limits: [u32; 2]) -> Result<(), Reject> {
        if self.zones.len() > limits[0] as usize {
            return Err(Reject::TooManyHoles);
        }
        if self.zones.iter().map(Vec::len).sum::<usize>() > limits[1] as usize {
            return Err(Reject::TooManyVertices);
        }
        let one = CollarLimits { outer: limits[1] as usize, holes: 0, hole_vertices: 0, total: limits[1] as usize, slots: 1, slot_bytes: 0 };
        for z in &self.zones {
            op_geo::shape::check_rings(&Boundary::lonlat(z), &[] as &[Vec<LonLat>], &one, 0.0, 0.0, 0.0).map_err(Reject::from)?;
        }
        Ok(())
    }

    pub fn digest(&self) -> [u8; 8] {
        zones_digest(&self.zones)
    }
}

/// Why a boundary isn't taking effect yet. It stays staged; the collar asks
/// again when zones, a lease or a clock arrive, or each minute.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Wait {
    /// It names a newer policy epoch than the zones held.
    Zones,
    /// Staged, and no current lease for the zones held.
    Lease,
    /// Staged, and no trustworthy time to judge a lease by.
    Clock,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activation {
    Activate,
    Wait(Wait),
    /// Refused for good (`in_exclusion`): the boundary in force stays.
    Reject(Reject),
}

/// The zones and lease a collar holds.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolicyState {
    /// The held zones; `None` before any arrived (epoch 0, no zones).
    pub zones: Option<Zones>,
    pub lease: Option<Lease>,
}

impl PolicyState {
    pub fn epoch(&self) -> u32 {
        self.zones.as_ref().map_or(0, |z| z.epoch)
    }

    /// Take a zones command (it has passed its checks and versions).
    pub fn take_zones(&mut self, z: Zones) {
        self.zones = Some(z);
    }

    /// Take a lease (newest by version, as any command).
    pub fn take_lease(&mut self, l: Lease) {
        self.lease = Some(l);
    }

    /// Whether boundary command `c` may take effect at GNSS time `now`, in
    /// order: a newer epoch than held waits; an overlap with a held zone is
    /// `in_exclusion`; a staged one that names an epoch needs a current
    /// lease for the held zones. Anything that isn't a boundary activates.
    pub fn may_activate(&self, c: &Command, now: Option<u64>) -> Activation {
        let Body::Boundary(b) = &c.body else { return Activation::Activate };
        if b.policy.is_some_and(|p| p > self.epoch()) {
            return Activation::Wait(Wait::Zones);
        }
        if let Some(z) = &self.zones
            && z.zones.iter().any(|zone| overlaps(&b.outer, &b.holes, zone))
        {
            return Activation::Reject(Reject::InExclusion);
        }
        let staged = c.effective_at.is_some_and(|e| e > c.issued_at.saturating_add(STAGED_AFTER_S));
        if staged && b.policy.is_some() {
            let Some(now) = now else { return Activation::Wait(Wait::Clock) };
            let held = self.zones.as_ref().map(|z| (z.epoch, z.digest()));
            let current = self.lease.is_some_and(|l| Some((l.epoch, l.digest)) == held && now < l.until);
            if !current {
                return Activation::Wait(Wait::Lease);
            }
        }
        Activation::Activate
    }
}

// ---------------------------------------------------------------- overlap

/// Coordinates scaled by 12, so midpoints, quarter points and triangle
/// centroids of e7 vertices stay integers.
type P = [i128; 2];
const SCALE: i128 = 12;

fn scaled(r: &Ring) -> Vec<P> {
    r.iter().map(|p| [i128::from(p[0]) * SCALE, i128::from(p[1]) * SCALE]).collect()
}

fn orient(a: P, b: P, c: P) -> i128 {
    ((b[0] - a[0]) * (c[1] - a[1]) - (b[1] - a[1]) * (c[0] - a[0])).signum()
}

fn on_segment(p: P, a: P, b: P) -> bool {
    orient(a, b, p) == 0 && p[0] >= a[0].min(b[0]) && p[0] <= a[0].max(b[0]) && p[1] >= a[1].min(b[1]) && p[1] <= a[1].max(b[1])
}

fn edges(r: &[P]) -> impl Iterator<Item = (P, P)> + '_ {
    (0..r.len()).map(move |i| (r[i], r[(i + 1) % r.len()]))
}

/// `Some(true)` strictly inside, `Some(false)` strictly outside, `None` on
/// the ring.
fn side(p: P, r: &[P]) -> Option<bool> {
    let mut inside = false;
    for (a, b) in edges(r) {
        if on_segment(p, a, b) {
            return None;
        }
        if (a[1] > p[1]) != (b[1] > p[1]) {
            // x of the edge at p's y, compared exactly: p.x < a.x + (p.y − a.y)(b.x − a.x)/(b.y − a.y)
            let lhs = (p[0] - a[0]) * (b[1] - a[1]);
            let rhs = (p[1] - a[1]) * (b[0] - a[0]);
            if (b[1] - a[1] > 0 && lhs < rhs) || (b[1] - a[1] < 0 && lhs > rhs) {
                inside = !inside;
            }
        }
    }
    Some(inside)
}

fn proper_cross(p1: P, p2: P, q1: P, q2: P) -> bool {
    orient(p1, p2, q1) * orient(p1, p2, q2) < 0 && orient(q1, q2, p1) * orient(q1, q2, p2) < 0
}

/// Points of a ring to test: vertices, quarter points of every edge, and the
/// centroid of one ear (a point strictly inside).
fn samples(r: &[P]) -> Vec<P> {
    let mut out: Vec<P> = r.to_vec();
    for (a, b) in edges(r) {
        for k in 1..4 {
            out.push([a[0] + (b[0] - a[0]) * k / 4, a[1] + (b[1] - a[1]) * k / 4]);
        }
    }
    let n = r.len();
    let area2: i128 = edges(r).map(|(a, b)| a[0] * b[1] - b[0] * a[1]).sum();
    for i in 0..n {
        let (a, b, c) = (r[(i + n - 1) % n], r[i], r[(i + 1) % n]);
        if orient(a, b, c) != area2.signum() || orient(a, b, c) == 0 {
            continue;
        }
        let tri = [a, b, c];
        let clear = r.iter().all(|p| tri.contains(p) || side(*p, &tri) == Some(false));
        if clear {
            out.push([(a[0] + b[0] + c[0]) / 3, (a[1] + b[1] + c[1]) / 3]);
            break;
        }
    }
    out
}

/// Whether a boundary's fenced area (inside `outer`, outside every hole)
/// and `zone` share a point strictly inside both. Touching edges and
/// vertices don't count, and a zone carved out exactly as a hole doesn't
/// overlap.
///
/// Exact on e7 integers for crossing edges and containment; boundaries that
/// meet only at shared vertices or along shared edges are judged from
/// sample points (vertices, quarter points, an inner point of each ring).
/// Whatever this says, the collar also enforces every zone as a hole of the
/// active boundary, so a strip it lets through still cues at the zone's edge.
pub fn overlaps(outer: &Ring, holes: &[Ring], zone: &Ring) -> bool {
    let (o, z) = (scaled(outer), scaled(zone));
    let hs: Vec<Vec<P>> = holes.iter().map(scaled).collect();
    if o.len() < 3 || z.len() < 3 {
        return false;
    }
    let rings = || std::iter::once(&o).chain(hs.iter());
    // 1. An edge of the zone properly crosses an edge of the area.
    if edges(&z).any(|(a, b)| rings().any(|r| edges(r).any(|(c, d)| proper_cross(a, b, c, d)))) {
        return true;
    }
    let in_area = |p: P| side(p, &o) == Some(true) && hs.iter().all(|h| side(p, h) == Some(false));
    // 2. A point of the zone is strictly inside the area.
    if samples(&z).into_iter().any(in_area) {
        return true;
    }
    // 3. A point on one of the area's rings (the area touches every one), or
    // an inner point that is in the area, is strictly inside the zone.
    rings().any(|r| samples(r).into_iter().filter(|p| side(*p, r).is_none() || in_area(*p)).any(|p| side(p, &z) == Some(true)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::v2::command::Body;

    fn sq(x0: i32, y0: i32, x1: i32, y1: i32) -> Ring {
        vec![[x0, y0], [x1, y0], [x1, y1], [x0, y1]]
    }

    #[test]
    fn overlap_cases() {
        let a = sq(0, 0, 1000, 1000);
        assert!(!overlaps(&a, &[], &sq(2000, 0, 3000, 1000)), "apart");
        assert!(!overlaps(&a, &[], &sq(1000, 0, 2000, 1000)), "sharing an edge");
        assert!(!overlaps(&a, &[], &sq(1000, 1000, 2000, 2000)), "touching at a corner");
        assert!(overlaps(&a, &[], &sq(500, 500, 1500, 1500)), "crossing");
        assert!(overlaps(&a, &[], &sq(100, 100, 200, 200)), "inside");
        assert!(overlaps(&a, &[], &sq(-100, -100, 2000, 2000)), "the zone holds the whole strip");
        assert!(overlaps(&a, &[], &a.clone()), "the same ring");
        assert!(overlaps(&a, &[], &sq(0, 500, 1000, 1500)), "edges along each other's, overlapping");
        let hole = sq(400, 400, 600, 600);
        assert!(!overlaps(&a, std::slice::from_ref(&hole), &hole), "carved out as a hole exactly");
        assert!(!overlaps(&a, std::slice::from_ref(&hole), &sq(450, 450, 550, 550)), "inside the hole");
        assert!(overlaps(&a, std::slice::from_ref(&hole), &sq(350, 450, 550, 550)), "sticking out of the hole");
        assert!(overlaps(&a, std::slice::from_ref(&hole), &a.clone()), "the whole strip, hole and all");
        // A non-convex zone (an L) wrapped around the strip's corner.
        let l = vec![[900, 900], [2000, 900], [2000, 2000], [-500, 2000], [-500, 1100], [900, 1100]];
        assert!(overlaps(&a, &[], &l));
    }

    fn strip(policy: Option<u32>, effective_at: Option<u64>) -> Command {
        Command {
            herd: b"H".to_vec(),
            device: None,
            version: 2,
            issued_at: 1_000,
            expires_at: None,
            reference: None,
            effective_at,
            body: Body::Boundary(Boundary { outer: sq(0, 0, 1000, 1000), policy, ..Default::default() }),
        }
    }

    /// Astra 02 F04: a strip staged for later activated after a no-go zone
    /// was added over it, with the collar offline the whole time.
    #[test]
    fn a_staged_strip_waits_for_zones_and_a_lease() {
        let creek = Zones { zones: vec![sq(500, 500, 600, 600)], epoch: 2 };
        let old = Zones { zones: vec![], epoch: 1 };
        let lease = |z: &Zones, until| Lease { epoch: z.epoch, until, digest: z.digest() };
        let later = Some(1_000 + 86_400);
        let now = Some(1_000 + 86_400);

        // Offline since epoch 1: its lease runs out, the strip waits.
        let offline = PolicyState { zones: Some(old.clone()), lease: Some(lease(&old, 1_000 + 7_200)) };
        assert_eq!(offline.may_activate(&strip(Some(1), later), now), Activation::Wait(Wait::Lease));
        // Online, it got the creek: the old strip is refused for good.
        let online = PolicyState { zones: Some(creek.clone()), lease: Some(lease(&creek, 1_000 + 90_000)) };
        assert_eq!(online.may_activate(&strip(Some(1), later), now), Activation::Reject(Reject::InExclusion));
        // A strip checked against epoch 2 that it hasn't got yet waits.
        assert_eq!(offline.may_activate(&strip(Some(2), later), now), Activation::Wait(Wait::Zones));
        // A clear strip under a current lease goes ahead.
        let clear = Command { body: Body::Boundary(Boundary { outer: sq(2000, 0, 3000, 1000), policy: Some(2), ..Default::default() }), ..strip(None, later) };
        assert_eq!(online.may_activate(&clear, now), Activation::Activate);
        assert_eq!(online.may_activate(&clear, None), Activation::Wait(Wait::Clock));
        // A lease for other zones under the same epoch doesn't count.
        let forged = PolicyState { zones: Some(creek.clone()), lease: Some(Lease { digest: [0; 8], ..lease(&creek, 1_000 + 90_000) }) };
        assert_eq!(forged.may_activate(&clear, now), Activation::Wait(Wait::Lease));
        // An immediate boundary (an escape pen) needs no lease, only no overlap.
        let pen = Command { body: Body::Boundary(Boundary { outer: sq(2000, 0, 3000, 1000), policy: Some(2), ..Default::default() }), ..strip(None, None) };
        assert_eq!(PolicyState { zones: Some(creek), lease: None }.may_activate(&pen, None), Activation::Activate);
    }

    #[test]
    fn zone_shapes_and_limits() {
        let z = Zones { zones: vec![sq(-936_200_000, 420_300_000, -936_190_000, 420_310_000)], epoch: 1 };
        z.check(DEFAULT_ZONE_LIMITS).unwrap();
        assert_eq!(z.check([0, 512]), Err(Reject::TooManyHoles));
        let bowtie = Zones { zones: vec![vec![[0, 0], [1000, 1000], [1000, 0], [0, 1000]]], epoch: 1 };
        assert_eq!(bowtie.check(DEFAULT_ZONE_LIMITS), Err(Reject::SelfIntersecting));
    }
}
