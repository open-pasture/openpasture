//! What a herd boundary does on the ground (field-ready §2.12, §2.13): the
//! part of [`crate::shape::prepare`] that applies exclusions and finds what
//! the farmer should know before it goes to the collars.
//!
//! **Exclusions.** Those in effect when the boundary takes effect (its
//! `effective_at` when that is ahead, else now) and that overlap it go to
//! [`op_geo::exclude::shape_target`]: cut out of the outer ring, made holes,
//! joined to the edge or to each other, or dropped. An exclusion the shape
//! already keeps out (a prepared boundary coming back through: a sweep step,
//! a reissue) is left alone, so preparing twice changes nothing.
//!
//! **Findings** from the map and the collars: the exclusions it overlapped
//! (`overlaps_exclusion`), hazards it takes in, roads, neighbour lines and the
//! farm boundary it crosses, water inside it or none, collars offline,
//! animals that end up in a new hole or outside it, and, for a staged
//! boundary, collars with no room for it yet. Hazards, roads, neighbour
//! lines, water and the farm boundary never change the shape. Geometry on a
//! finding is GeoJSON to draw: the overlap, the part past the farm boundary,
//! the road inside, the water, the collars' last fixes.

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use geo::{Area, BooleanOps, Coord, Euclidean, Length, LineString, MultiLineString, MultiPolygon};
use op_core::check::Finding;
use op_core::features::{FeatureGeometry, FeatureKind, MapFeature};
use op_core::time::now;
use op_core::{ApiResult, Ctx, LonLat, Severity};
use op_geo::exclude::{self, Placement};
use op_geo::projection::round7;
use op_geo::shape::{self, SERVER_SLACK_M};
use op_geo::{CollarLimits, Polygon, Projection, point_in_ring};
use op_protocol::BoundaryCommand;
use serde_json::{Value, json};
use sqlx::Row;

use crate::SendOpts;
use crate::shape::{CollarCaps, HerdCollar};
use crate::{db, escapes, moves};

/// Overlaps smaller than this (m²) are rounding, not ground.
pub const OVERLAP_M2: f64 = 1.0;
/// A collar that hasn't reported for this long won't take a boundary soon (A-engine's `silent` default).
pub const OFFLINE_AFTER: Duration = Duration::minutes(20);
/// Sides of the circle drawn for a hazard point's radius.
const DISC_SIDES: usize = 32;

/// When a send takes effect: its `effective_at` when that is still ahead, else now.
pub fn activation(opts: &SendOpts) -> DateTime<Utc> {
    let t = now();
    opts.effective_at.filter(|at| *at > t).unwrap_or(t)
}

/// The target after exclusions and fitting, and what each overlapping exclusion became.
pub(crate) struct Excluded {
    /// The target with the exclusions cut out or made holes, before fitting.
    pub shaped: Polygon,
    pub fitted: Polygon,
    /// Fitting to the collars changed the shape (beyond the exclusions).
    pub simplified: bool,
    pub placed: Vec<Placed>,
}

pub(crate) struct Placed {
    pub feature: MapFeature,
    pub placement: Placement,
    /// Where it overlapped the drawn shape (GeoJSON).
    pub overlap: Option<Value>,
}

/// Apply the exclusions in effect at `at` to a validated target and fit it to `limits`.
pub(crate) async fn exclude(ctx: &Ctx, geometry: &Polygon, limits: &CollarLimits, warn_m: f64, gap: f64, at: DateTime<Utc>) -> anyhow::Result<Excluded> {
    let found = op_core::features::exclusions_for(ctx, geometry, at).await?;
    let mut pending = Vec::new();
    if let Some(loc) = Local::new(geometry) {
        let target = loc.poly(geometry);
        for f in found.into_iter().filter(|f| f.kind == FeatureKind::Exclusion) {
            let Some(p) = f.geometry.polygon() else { continue };
            let overlap = target.intersection(&loc.poly(&p));
            if overlap.unsigned_area() >= OVERLAP_M2 {
                pending.push((f, p, loc.to_geojson(&overlap)));
            }
        }
    }
    if pending.is_empty() {
        let fitted = shape::fit_gap(geometry, limits, gap);
        let simplified = fitted.coordinates != geometry.coordinates;
        return Ok(Excluded { shaped: geometry.clone(), fitted, simplified, placed: vec![] });
    }
    // Shaped with room for any number of corners (only `holes` matters until the
    // final fit), then fitted: the same result as shaping with `limits`, and the
    // difference between the two is what fitting simplified.
    let roomy = CollarLimits { outer: 1 << 20, hole_vertices: 1 << 20, total: 1 << 20, ..*limits };
    let rings: Vec<Polygon> = pending.iter().map(|(_, p, _)| p.clone()).collect();
    let shaped = exclude::shape_target(geometry, &rings, &roomy, warn_m);
    let fitted = shape::fit_gap(&shaped.geometry, limits, gap);
    let simplified = fitted.coordinates != shaped.geometry.coordinates;
    let placed = pending.into_iter().zip(shaped.placements).map(|((feature, _, overlap), placement)| Placed { feature, placement, overlap }).collect();
    Ok(Excluded { shaped: shaped.geometry, fitted, simplified, placed })
}

/// What the farmer should know about `sent` (the shape as it goes to the
/// collars), taking effect at `at`. See the module docs.
pub(crate) async fn findings(
    ctx: &Ctx,
    herd_id: &str,
    sent: &Polygon,
    excluded: &Excluded,
    herd: &[HerdCollar],
    at: DateTime<Utc>,
    warn_m: f64,
) -> ApiResult<Vec<Finding>> {
    let mut out = Vec::new();
    for p in &excluded.placed {
        out.push(exclusion_finding(p));
    }
    let Some(loc) = Local::new(sent) else { return Ok(out) };
    let features = op_core::features::list_features(ctx, None, None, Some(at)).await?;
    out.extend(map_findings(&loc, sent, &features));

    let t = now();
    let collars = db::list_collars(ctx.db(), Some(herd_id)).await?;
    let escaped: HashSet<String> = escapes::escaped_collars(ctx.db(), herd_id).await?.into_iter().collect();
    let offline: Vec<&op_core::Collar> = collars.iter().filter(|c| c.parked_at.is_none() && c.last_seen.is_none_or(|s| t - s > OFFLINE_AFTER)).collect();
    if !offline.is_empty() {
        let n = offline.len();
        out.push(Finding {
            code: "collars_offline".into(),
            severity: Severity::Warning,
            text: if n == 1 { "1 collar offline".into() } else { format!("{n} collars offline") },
            geometry: multipoint(offline.iter().filter_map(|c| c.last_fix.as_ref().map(|f| f.point))),
            targets: offline.iter().map(|c| ("collar".to_owned(), c.id.clone())).collect(),
        });
    }
    let active = db::herd_boundaries(ctx.db(), herd_id, t).await?.active.map(|b| b.geometry);
    let fresh: Vec<(&str, LonLat)> = collars
        .iter()
        .filter(|c| c.parked_at.is_none() && !escaped.contains(&c.id))
        .filter_map(|c| c.last_fix.as_ref().filter(|f| t - f.at <= moves::FRESH_FIX).map(|f| (c.id.as_str(), f.point)))
        .collect();
    let (in_holes, outside) = placed_animals(sent, active.as_ref(), &fresh);
    if !in_holes.is_empty() {
        let n = in_holes.len();
        out.push(Finding {
            code: "animals_in_new_holes".into(),
            severity: Severity::Warning,
            text: if n == 1 { "1 inside a new hole".into() } else { format!("{n} inside new holes") },
            geometry: multipoint(in_holes.iter().map(|(_, p)| *p)),
            targets: in_holes.iter().map(|(c, _)| ("collar".to_owned(), c.to_string())).collect(),
        });
    }
    if !outside.is_empty() {
        let n = outside.len();
        out.push(Finding {
            code: "animals_outside".into(),
            severity: Severity::Warning,
            text: format!("{n} outside it"),
            geometry: multipoint(outside.iter().map(|(_, p)| *p)),
            targets: outside.iter().map(|(c, _)| ("collar".to_owned(), c.to_string())).collect(),
        });
    }
    if at > t {
        let full = no_room(ctx, herd_id, sent, at, herd, &escaped, warn_m).await?;
        if !full.is_empty() {
            let n = full.len();
            out.push(Finding {
                code: "slots_full".into(),
                severity: Severity::Warning,
                text: if n == 1 { "1 collar has no room for it yet".into() } else { format!("{n} collars have no room for it yet") },
                geometry: None,
                targets: full.into_iter().map(|c| ("collar".to_owned(), c)).collect(),
            });
        }
    }
    Ok(out)
}

/// Most severe first; findings of one severity keep their order.
pub fn sort(findings: &mut [Finding]) {
    findings.sort_by(|a, b| b.severity.cmp(&a.severity));
}

fn name_or(f: &MapFeature, fallback: &str) -> String {
    f.name.as_deref().map(str::trim).filter(|n| !n.is_empty()).map(str::to_owned).unwrap_or_else(|| fallback.to_owned())
}

fn capital(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

fn exclusion_finding(p: &Placed) -> Finding {
    let name = name_or(&p.feature, "exclusion");
    let (severity, text) = match p.placement {
        // Covers all of it (nothing would be left to graze), so the collars don't keep it out.
        Placement::Drop => (Severity::Warning, format!("All of it lies in {}", if p.feature.name.is_some() { name } else { "an exclusion".into() })),
        _ => (Severity::Info, format!("{} kept out", capital(&name))),
    };
    Finding { code: "overlaps_exclusion".into(), severity, text, geometry: p.overlap.clone(), targets: vec![("feature".into(), p.feature.id.clone())] }
}

/// Hazards, roads, neighbour lines, the farm boundary and water against the sent shape.
fn map_findings(loc: &Local, sent: &Polygon, features: &[MapFeature]) -> Vec<Finding> {
    let mut out = Vec::new();
    let shape = loc.poly(sent);
    let target = |f: &MapFeature| vec![("feature".to_owned(), f.id.clone())];
    let mut waters = 0;
    let mut water_inside = Vec::new();
    for f in features {
        match (f.kind, &f.geometry) {
            (FeatureKind::Hazard, g) => {
                let area = match g {
                    FeatureGeometry::Point(c) => match f.props.get("radius_m").and_then(Value::as_f64).filter(|r| r.is_finite() && *r > 0.0) {
                        Some(r) => loc.disc(*c, r),
                        None => continue,
                    },
                    FeatureGeometry::Polygon(rings) => loc.rings(rings),
                    FeatureGeometry::LineString(_) => continue,
                };
                let overlap = shape.intersection(&area);
                if overlap.unsigned_area() >= OVERLAP_M2 {
                    let text = match f.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                        Some(n) => format!("Takes in {n}"),
                        None => "Takes in a hazard".into(),
                    };
                    out.push(Finding {
                        code: "overlaps_hazard".into(),
                        severity: Severity::Warning,
                        text,
                        geometry: loc.to_geojson(&overlap),
                        targets: target(f),
                    });
                }
            }
            (FeatureKind::Road | FeatureKind::NeighbourLine, FeatureGeometry::LineString(pts)) => {
                let inside = shape.clip(&MultiLineString(vec![loc.line(pts)]), false);
                if inside.0.iter().map(|l| l.length::<Euclidean>()).sum::<f64>() < 0.5 {
                    continue;
                }
                let (code, severity, text) = if f.kind == FeatureKind::Road {
                    ("crosses_road", Severity::Critical, format!("Crosses {}", name_or(f, "a road")))
                } else {
                    ("crosses_neighbour_line", Severity::Warning, format!("Crosses {}", name_or(f, "the neighbour line")))
                };
                out.push(Finding { code: code.into(), severity, text, geometry: loc.lines_geojson(&inside), targets: target(f) });
            }
            (FeatureKind::FarmBoundary, FeatureGeometry::Polygon(rings)) => {
                let past = shape.difference(&loc.rings(rings));
                if past.unsigned_area() >= OVERLAP_M2 {
                    out.push(Finding {
                        code: "crosses_farm_boundary".into(),
                        severity: Severity::Warning,
                        text: "Goes past the farm boundary".into(),
                        geometry: loc.to_geojson(&past),
                        targets: target(f),
                    });
                }
            }
            (FeatureKind::Water, g) => {
                waters += 1;
                let inside = match g {
                    FeatureGeometry::Point(p) => sent.contains(*p),
                    FeatureGeometry::Polygon(rings) => shape.intersection(&loc.rings(rings)).unsigned_area() >= OVERLAP_M2,
                    FeatureGeometry::LineString(_) => false,
                };
                if inside {
                    water_inside.push(Finding {
                        code: "water_inside".into(),
                        severity: Severity::Info,
                        text: format!("{} inside", capital(&name_or(f, "water"))),
                        geometry: serde_json::to_value(g).ok(),
                        targets: target(f),
                    });
                }
            }
            _ => {}
        }
    }
    // Only a farm that has mapped its water can say none is inside.
    if waters > 0 && water_inside.is_empty() {
        out.push(Finding { code: "no_water".into(), severity: Severity::Warning, text: "No water inside".into(), geometry: None, targets: vec![] });
    }
    out.extend(water_inside);
    out
}

type At<'a> = (&'a str, LonLat);

/// Animals the sent shape leaves in one of its holes that isn't a hole of
/// the active boundary, and animals outside it otherwise.
fn placed_animals<'a>(sent: &Polygon, active: Option<&Polygon>, fresh: &[At<'a>]) -> (Vec<At<'a>>, Vec<At<'a>>) {
    let outer = sent.outer_ring();
    let holes: Vec<Vec<LonLat>> = sent.holes().collect();
    let old: Vec<Vec<LonLat>> = active.map(|a| a.holes().collect()).unwrap_or_default();
    let (mut in_holes, mut outside) = (Vec::new(), Vec::new());
    for (c, p) in fresh {
        if !point_in_ring(*p, &outer) {
            outside.push((*c, *p));
        } else if holes.iter().any(|h| point_in_ring(*p, h)) {
            if old.iter().any(|h| point_in_ring(*p, h)) {
                outside.push((*c, *p));
            } else {
                in_holes.push((*c, *p));
            }
        }
    }
    (in_holes, outside)
}

/// Collars that can't store a boundary staged for `at` now: after the
/// versions that die when it activates are pruned, the one in effect, the
/// staged ones before it (held, or on the server waiting for them) and the
/// new one need more slots, or more slot bytes, than the collar has (the
/// collar's own rule, §3.7). Parked collars and collars out on an escape
/// are left out; they get the herd's boundaries when they come back.
async fn no_room(
    ctx: &Ctx,
    herd_id: &str,
    sent: &Polygon,
    at: DateTime<Utc>,
    herd: &[HerdCollar],
    escaped: &HashSet<String>,
    warn_m: f64,
) -> anyhow::Result<Vec<String>> {
    let split = db::herd_boundaries(ctx.db(), herd_id, now()).await?;
    let waiting: Vec<u32> = split.staged.iter().filter(|b| db::activation(b) < at).map(|b| b.version).collect();
    let rows = sqlx::query(
        "SELECT s.collar_id, s.version, s.status, s.effective_at FROM collar_slots s JOIN collars c ON c.id = s.collar_id
         WHERE c.herd_id = ? AND s.status != 'rejected'",
    )
    .bind(herd_id)
    .fetch_all(ctx.db())
    .await?;
    // Per collar: the version it applied, and the staged versions it holds that activate before `at`.
    let mut held: HashMap<String, (Option<u32>, Vec<u32>)> = HashMap::new();
    for r in &rows {
        let v = r.try_get::<i64, _>("version")? as u32;
        let e = held.entry(r.try_get("collar_id")?).or_default();
        if r.try_get::<String, _>("status")? == "applied" {
            e.0 = e.0.max(Some(v));
        } else if op_core::time::opt_from_db(r.try_get("effective_at")?)?.is_some_and(|t| t < at) {
            e.1.push(v);
        }
    }
    // Stored shapes of every version a collar keeps, for their record bytes.
    let active = split.active.as_ref().map(|b| b.version);
    let versions: HashSet<u32> = held.values().flat_map(|(a, s)| a.iter().chain(s)).chain(&waiting).chain(&active).copied().collect();
    let mut shapes: HashMap<u32, Polygon> = HashMap::new();
    for v in versions {
        if let Some(r) = sqlx::query("SELECT geometry FROM boundaries WHERE version = ?").bind(v as i64).fetch_optional(ctx.db()).await? {
            shapes.insert(v, serde_json::from_str(&r.try_get::<String, _>(0)?)?);
        }
    }
    let gap = shape::min_gap_m(warn_m) + SERVER_SLACK_M;
    let mut new_bytes: HashMap<CollarLimits, usize> = HashMap::new();
    let mut full = Vec::new();
    for c in herd.iter().filter(|c| c.parked.is_none() && !escaped.contains(&c.id)) {
        let (applied, staged) = held.get(&c.id).cloned().unwrap_or_default();
        // The one in effect (what it applied, else the herd's it will download first).
        let in_effect = applied.or(active);
        let staged: BTreeSet<u32> = staged.into_iter().chain(waiting.iter().copied()).filter(|v| Some(*v) != in_effect).collect();
        let count = usize::from(in_effect.is_some()) + staged.len() + 1;
        let limits = c.caps.fit_limits();
        let new = *new_bytes.entry(limits).or_insert_with(|| CollarLimits::record_bytes(shape::fit_gap(sent, &limits, gap).total_vertices()));
        let used: usize =
            in_effect.iter().chain(&staged).filter_map(|v| shapes.get(v)).map(|p| CollarLimits::record_bytes(record_vertices(p, &c.caps))).sum::<usize>() + new;
        if count > c.caps.limits.slots || (c.caps.limits.slot_bytes > 0 && used > c.caps.limits.slot_bytes) {
            full.push(c.id.clone());
        }
    }
    Ok(full)
}

/// Vertices a collar stores for a stored boundary: its holes only if it holds them, at most its total.
fn record_vertices(p: &Polygon, caps: &CollarCaps) -> usize {
    let n = if caps.holds_holes() { p.total_vertices() } else { p.outer_ring().len() };
    n.min(caps.limits.total.max(caps.limits.outer))
}

/// The ring a legacy collar (firmware 0.1: no holes, 64 vertices) enforces
/// for `sent`, exactly as [`crate::command_for`] builds its command.
pub fn legacy_fence(sent: &Polygon, warn_m: f64) -> ApiResult<Polygon> {
    let caps = CollarCaps { fw: None, caps: vec![], limits: CollarLimits::LEGACY };
    let g = shape::fit_gap(sent, &caps.fit_limits(), shape::min_gap_m(warn_m) + SERVER_SLACK_M);
    Ok(BoundaryCommand::from_shape("preview", 1, &g, None)?.polygon())
}

/// Whether the herd has an unparked collar without caps (firmware 0.1).
pub async fn has_legacy_collars(ctx: &Ctx, herd_id: &str) -> anyhow::Result<bool> {
    Ok(crate::shape::herd_collars(ctx.db(), herd_id).await?.iter().any(|c| c.parked.is_none() && c.caps.caps.is_empty()))
}

/// What the move driver sees for the herd at `at` when a new move starts
/// (its active boundary, paddock, fresh positions and limits), for previewing a sweep.
pub async fn situation(ctx: &Ctx, herd_id: &str, at: DateTime<Utc>) -> anyhow::Result<moves::Situation> {
    moves::situation(ctx, herd_id, at, None, None).await
}

/// Squares (`[west, south, east, north]`) clipped to `shape`, as one GeoJSON
/// geometry, with the area they cover inside it (ha). For the pre-send
/// check's weak-coverage cells.
pub fn clip_boxes(shape: &Polygon, boxes: &[[f64; 4]]) -> Option<(Value, f64)> {
    let loc = Local::new(shape)?;
    let squares = MultiPolygon(
        boxes.iter().map(|b| loc.rings(&[vec![[b[0], b[1]], [b[2], b[1]], [b[2], b[3]], [b[0], b[3]], [b[0], b[1]]]])).collect::<Vec<geo::Polygon<f64>>>(),
    );
    let merged = squares.union(&MultiPolygon::<f64>(vec![]));
    let inside = loc.poly(shape).intersection(&merged);
    let ha = inside.unsigned_area() / 10_000.0;
    Some((loc.to_geojson(&inside)?, ha))
}

fn multipoint(points: impl Iterator<Item = LonLat>) -> Option<Value> {
    let pts: Vec<LonLat> = points.collect();
    (!pts.is_empty()).then(|| json!({ "type": "MultiPoint", "coordinates": pts }))
}

/// Local metres about a shape's first corner, for areas, overlaps and clipping.
/// How much of `of` (0 to 1) lies inside `within`, holes counted.
pub(crate) fn share_inside(of: &Polygon, within: &Polygon) -> f64 {
    let Some(loc) = Local::new(of) else { return 0.0 };
    let a = loc.poly(of);
    let area = a.unsigned_area();
    if area <= 0.0 {
        return 0.0;
    }
    a.intersection(&loc.poly(within)).unsigned_area() / area
}

struct Local {
    proj: Projection,
}

impl Local {
    fn new(p: &Polygon) -> Option<Self> {
        p.outer_ring().first().map(|o| Self { proj: Projection::new(*o) })
    }

    fn ring(&self, r: &[LonLat]) -> LineString<f64> {
        let mut pts: Vec<Coord<f64>> = op_geo::clean_ring(r).iter().map(|p| self.proj.forward(*p)).map(|[x, y]| Coord { x, y }).collect();
        if let Some(first) = pts.first().copied() {
            pts.push(first);
        }
        LineString(pts)
    }

    fn rings(&self, rings: &[Vec<LonLat>]) -> geo::Polygon<f64> {
        let mut it = rings.iter();
        let outer = it.next().map(|r| self.ring(r)).unwrap_or_else(|| LineString(vec![]));
        geo::Polygon::new(outer, it.map(|r| self.ring(r)).collect())
    }

    fn poly(&self, p: &Polygon) -> geo::Polygon<f64> {
        self.rings(&p.coordinates)
    }

    fn line(&self, pts: &[LonLat]) -> LineString<f64> {
        LineString(pts.iter().map(|p| self.proj.forward(*p)).map(|[x, y]| Coord { x, y }).collect())
    }

    fn disc(&self, c: LonLat, r: f64) -> geo::Polygon<f64> {
        let [cx, cy] = self.proj.forward(c);
        let mut pts: Vec<Coord<f64>> = (0..DISC_SIDES)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / DISC_SIDES as f64;
                Coord { x: cx + r * a.cos(), y: cy + r * a.sin() }
            })
            .collect();
        pts.push(pts[0]);
        geo::Polygon::new(LineString(pts), vec![])
    }

    fn back(&self, c: &Coord<f64>) -> LonLat {
        let p = self.proj.inverse([c.x, c.y]);
        [round7(p[0]), round7(p[1])]
    }

    fn back_ring(&self, ls: &LineString<f64>) -> Vec<LonLat> {
        ls.coords().map(|c| self.back(c)).collect()
    }

    /// A Polygon, or a MultiPolygon for several pieces; `None` when empty.
    fn to_geojson(&self, mp: &MultiPolygon<f64>) -> Option<Value> {
        let polys: Vec<Vec<Vec<LonLat>>> =
            mp.0.iter()
                .filter(|p| p.unsigned_area() > 0.0)
                .map(|p| std::iter::once(self.back_ring(p.exterior())).chain(p.interiors().iter().map(|r| self.back_ring(r))).collect())
                .collect();
        match polys.len() {
            0 => None,
            1 => Some(json!({ "type": "Polygon", "coordinates": polys[0] })),
            _ => Some(json!({ "type": "MultiPolygon", "coordinates": polys })),
        }
    }

    fn lines_geojson(&self, ml: &MultiLineString<f64>) -> Option<Value> {
        let lines: Vec<Vec<LonLat>> = ml.0.iter().filter(|l| l.0.len() >= 2).map(|l| self.back_ring(l)).collect();
        match lines.len() {
            0 => None,
            1 => Some(json!({ "type": "LineString", "coordinates": lines[0] })),
            _ => Some(json!({ "type": "MultiLineString", "coordinates": lines })),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const O: LonLat = [-93.6225, 42.0318];

    fn at(x: f64, y: f64) -> LonLat {
        Projection::new(O).inverse([x, y])
    }
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<LonLat> {
        vec![at(x0, y0), at(x1, y0), at(x1, y1), at(x0, y1), at(x0, y0)]
    }
    fn feature(kind: FeatureKind, geometry: FeatureGeometry, name: Option<&str>, props: Value) -> MapFeature {
        MapFeature {
            id: format!("fea_{}", name.unwrap_or("x")),
            kind,
            name: name.map(str::to_owned),
            geometry,
            paddock_id: None,
            notes: None,
            props,
            active_from: None,
            active_until: None,
            created_at: now(),
            updated_at: now(),
        }
    }
    fn codes(f: &[Finding]) -> Vec<&str> {
        f.iter().map(|f| f.code.as_str()).collect()
    }

    #[test]
    fn roads_hazards_water_and_the_farm_boundary_are_found_not_enforced() {
        let sent = Polygon::from_ring(rect(0.0, 0.0, 200.0, 100.0));
        let loc = Local::new(&sent).unwrap();
        let road = feature(FeatureKind::Road, FeatureGeometry::LineString(vec![at(-50.0, 50.0), at(250.0, 50.0)]), Some("county road"), json!({}));
        let well = feature(FeatureKind::Hazard, FeatureGeometry::Point(at(20.0, 20.0)), Some("old well"), json!({ "radius_m": 10.0 }));
        let farm = feature(FeatureKind::FarmBoundary, FeatureGeometry::Polygon(vec![rect(-10.0, -10.0, 150.0, 110.0)]), None, json!({}));
        let trough = feature(FeatureKind::Water, FeatureGeometry::Point(at(500.0, 500.0)), Some("north trough"), json!({}));
        let f = map_findings(&loc, &sent, &[road.clone(), well, farm, trough.clone()]);
        assert_eq!(codes(&f), ["crosses_road", "overlaps_hazard", "crosses_farm_boundary", "no_water"]);
        assert_eq!(f[0].severity, Severity::Critical);
        assert_eq!(f[0].text, "Crosses county road");
        assert_eq!(f[0].geometry.as_ref().unwrap()["type"], "LineString");
        assert_eq!(f[1].text, "Takes in old well");
        // The part past the farm boundary: 50 x 100 m.
        let past = f[2].geometry.as_ref().unwrap();
        assert_eq!(past["type"], "Polygon");
        // Water inside: ringed, and no "no water".
        let pond = feature(FeatureKind::Water, FeatureGeometry::Point(at(100.0, 80.0)), Some("pond"), json!({}));
        let f = map_findings(&loc, &sent, &[trough, pond]);
        assert_eq!(codes(&f), ["water_inside"]);
        assert_eq!(f[0].text, "Pond inside");
        // No water mapped at all: nothing said about water.
        assert!(map_findings(&loc, &sent, &[road]).iter().all(|f| !f.code.contains("water")));
    }

    #[test]
    fn a_road_alongside_is_not_a_crossing() {
        let sent = Polygon::from_ring(rect(0.0, 0.0, 200.0, 100.0));
        let loc = Local::new(&sent).unwrap();
        let road = feature(FeatureKind::Road, FeatureGeometry::LineString(vec![at(-50.0, 110.0), at(250.0, 110.0)]), None, json!({}));
        assert!(map_findings(&loc, &sent, &[road]).is_empty());
    }

    #[test]
    fn animals_in_new_holes_and_outside() {
        let hole = rect(80.0, 30.0, 120.0, 70.0);
        let sent = Polygon::from_rings(rect(0.0, 0.0, 200.0, 100.0), [hole.clone()]);
        let fresh = [("a", at(100.0, 50.0)), ("b", at(10.0, 10.0)), ("c", at(300.0, 10.0))];
        let (holes, out) = placed_animals(&sent, None, &fresh);
        assert_eq!((holes.len(), out.len()), (1, 1));
        assert_eq!((holes[0].0, out[0].0), ("a", "c"));
        // A hole the active boundary already had isn't new: that animal was already outside.
        let active = Polygon::from_rings(rect(0.0, 0.0, 200.0, 100.0), [hole]);
        let (holes, out) = placed_animals(&sent, Some(&active), &fresh);
        assert_eq!((holes.len(), out.len()), (0, 2));
    }

    #[test]
    fn the_legacy_fence_is_one_ring_of_at_most_64() {
        let circle: Vec<LonLat> = (0..=120)
            .map(|i| {
                let a = std::f64::consts::TAU * (i % 120) as f64 / 120.0;
                at(200.0 * a.cos(), 200.0 * a.sin())
            })
            .collect();
        let sent = Polygon::from_rings(circle, [rect(-20.0, -20.0, 20.0, 20.0)]);
        let fence = legacy_fence(&sent, 5.0).unwrap();
        assert_eq!(fence.coordinates.len(), 1);
        assert!(fence.outer_ring().len() <= 64);
        assert!(fence.contains(at(0.0, 0.0)), "no hole for a legacy collar");
    }

    #[test]
    fn boxes_clip_to_the_shape() {
        let shape = Polygon::from_ring(rect(0.0, 0.0, 100.0, 100.0));
        let b = |x: f64, y: f64| {
            let (w, s) = (at(x, y), at(x + 10.0, y + 10.0));
            [w[0], w[1], s[0], s[1]]
        };
        // One square inside, one half out, one far away.
        let (g, ha) = clip_boxes(&shape, &[b(10.0, 10.0), b(95.0, 50.0), b(500.0, 500.0)]).unwrap();
        assert!((ha - 0.015).abs() < 0.0005, "{ha}");
        assert_eq!(g["type"], "MultiPolygon");
        assert!(clip_boxes(&shape, &[b(500.0, 500.0)]).is_none());
    }
}
