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
//! No-go zones are exclusions that are always in effect: they are cut out or
//! made holes the same way.
//!
//! **Findings** from the map and the collars: the exclusions it overlapped
//! (`overlaps_exclusion`), no-go zones it still takes in (`enters_no_go`:
//! one that covers all of it, or a hole the herd's collars that can't hold
//! holes won't keep), hazards it takes in, roads, neighbour lines and the
//! farm boundary it crosses, water inside it or none, collars offline,
//! animals that end up in a new hole or outside it, and, for a staged
//! boundary, collars with no room for it yet. Hazards, roads, neighbour
//! lines, water and the farm boundary never change the shape. Geometry on a
//! finding is GeoJSON to draw: the overlap, the part past the farm boundary,
//! the road inside, the water, the collars' last fixes.
//!
//! The critical findings are the ones [`crate::gate`] refuses: a road or the
//! farm boundary crossed, a no-go zone or a hard hazard (`props.hard`) taken
//! in, all of it in an exclusion, and no water inside when the farm requires
//! it ([`crate::gate::GateSettings`]).
//!
//! **Contact counts.** A road or a no-go zone within [`CONTACT_M`] of a
//! boundary is crossed or taken in: touching it at a corner, running along
//! it, or poking a few centimetres into it. A no-go zone is cut out with
//! [`NO_GO_STANDOFF_M`] of ground around it, so a boundary shaped here is
//! clear of that margin. The farm boundary is crossed when any corner of the
//! shape lies more than [`EDGE_TOLERANCE_M`] past it. When the farm requires
//! water and none is mapped, every boundary is `no_water`: nothing shows
//! water inside it. Areas below [`OVERLAP_M2`] are only rounding for the
//! findings that don't use contact (exclusions, soft hazards, water).

use std::collections::{BTreeSet, HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use geo::{Area, BooleanOps, Coord, Distance, Euclidean, Length, LineString, MultiLineString, MultiPolygon};
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
/// A road or no-go zone this close (m) to a boundary touches it: crossed or taken in.
pub const CONTACT_M: f64 = 0.5;
/// Ground (m) kept clear around a no-go zone cut out of a boundary (more than [`CONTACT_M`]).
pub const NO_GO_STANDOFF_M: f64 = 1.0;
/// A corner further than this (m) past the farm boundary crosses it (coordinates round to about 1 cm).
pub const EDGE_TOLERANCE_M: f64 = 0.05;
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
        for f in found.into_iter().filter(|f| matches!(f.kind, FeatureKind::Exclusion | FeatureKind::NoGo)) {
            let Some(mut p) = f.geometry.polygon() else { continue };
            // A no-go zone goes with ground around it: the boundary stays clear of touching it.
            if f.kind == FeatureKind::NoGo {
                p = grown(&p, NO_GO_STANDOFF_M);
            }
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
    out.extend(excluded.placed.iter().filter_map(exclusion_finding));
    let Some(loc) = Local::new(sent) else { return Ok(out) };
    let features = op_core::features::list_features(ctx, None, None, Some(at)).await?;
    let rules =
        MapRules { holeless: herd.iter().any(|c| c.parked.is_none() && !c.caps.holds_holes()), require_water: crate::gate::settings(ctx).await?.require_water };
    out.extend(map_findings(&loc, sent, &features, rules));

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

fn exclusion_finding(p: &Placed) -> Option<Finding> {
    // A no-go zone left in the shape is `enters_no_go` (map_findings); one kept out is just that.
    if p.feature.kind == FeatureKind::NoGo && p.placement == Placement::Drop {
        return None;
    }
    let name = name_or(&p.feature, if p.feature.kind == FeatureKind::NoGo { "no-go zone" } else { "exclusion" });
    let (severity, text) = match p.placement {
        // Covers all of it (nothing would be left to graze), so the collars don't keep it out.
        Placement::Drop => (Severity::Critical, format!("All of it lies in {}", if p.feature.name.is_some() { name } else { "an exclusion".into() })),
        _ => (Severity::Info, format!("{} kept out", capital(&name))),
    };
    Some(Finding { code: "overlaps_exclusion".into(), severity, text, geometry: p.overlap.clone(), targets: vec![("feature".into(), p.feature.id.clone())] })
}

/// What the map checks need beyond the features: whether some of the herd's
/// collars enforce the outer ring alone (they don't keep a no-go hole), and
/// whether the farm requires water inside every boundary.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct MapRules {
    pub holeless: bool,
    pub require_water: bool,
}

/// No-go zones, hazards, roads, neighbour lines, the farm boundary and water against the sent shape.
pub(crate) fn map_findings(loc: &Local, sent: &Polygon, features: &[MapFeature], rules: MapRules) -> Vec<Finding> {
    let mut out = Vec::new();
    let shape = loc.poly(sent);
    // What a collar that can't hold holes enforces: the outer ring alone.
    let ring = rules.holeless.then(|| loc.poly(&Polygon::from_ring(sent.outer_ring()))).filter(|_| sent.coordinates.len() > 1);
    let target = |f: &MapFeature| vec![("feature".to_owned(), f.id.clone())];
    let mut waters = 0;
    let mut water_inside = Vec::new();
    for f in features {
        match (f.kind, &f.geometry) {
            (FeatureKind::NoGo, FeatureGeometry::Polygon(rings)) => {
                let zone = loc.rings(rings);
                let name = name_or(f, "a no-go zone");
                let overlap = shape.intersection(&zone);
                let text = if overlap.unsigned_area() >= OVERLAP_M2 {
                    format!("Takes in {name}")
                } else if touches(&shape, &zone) {
                    format!("Touches {name}")
                } else if ring.as_ref().is_some_and(|r| touches(r, &zone)) {
                    format!("Collars that can't hold holes take in {name}")
                } else {
                    continue;
                };
                let geometry = loc.to_geojson(&overlap).or_else(|| loc.to_geojson(&MultiPolygon(vec![zone.clone()])));
                out.push(Finding { code: "enters_no_go".into(), severity: Severity::Critical, text, geometry, targets: target(f) });
            }
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
                let hard = f.props.get("hard").and_then(Value::as_bool).unwrap_or(false);
                // A hard hazard is a hard limit: touching it counts, as for a no-go zone.
                if overlap.unsigned_area() >= OVERLAP_M2 || (hard && touches(&shape, &area)) {
                    let text = match f.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
                        Some(n) => format!("Takes in {n}"),
                        None => "Takes in a hazard".into(),
                    };
                    let overlap = if overlap.unsigned_area() > 0.0 { overlap } else { MultiPolygon(vec![area.clone()]) };
                    out.push(Finding {
                        code: "overlaps_hazard".into(),
                        severity: if hard { Severity::Critical } else { Severity::Warning },
                        text,
                        geometry: loc.to_geojson(&overlap),
                        targets: target(f),
                    });
                }
            }
            (FeatureKind::Road | FeatureKind::NeighbourLine, FeatureGeometry::LineString(pts)) => {
                let line = loc.line(pts);
                let inside = shape.clip(&MultiLineString(vec![line.clone()]), false);
                let crossed = inside.0.iter().map(|l| l.length::<Euclidean>()).sum::<f64>() >= 0.5;
                // A road is a hard limit: within CONTACT_M of the boundary (a corner on it, a few
                // centimetres in, along the edge) counts as crossing it.
                let road = f.kind == FeatureKind::Road;
                if !crossed && !(road && line.0.len() >= 2 && Euclidean::distance(&line, &shape) < CONTACT_M) {
                    continue;
                }
                let (code, severity, text) = if road {
                    let verb = if crossed { "Crosses" } else { "Touches" };
                    ("crosses_road", Severity::Critical, format!("{verb} {}", name_or(f, "a road")))
                } else {
                    ("crosses_neighbour_line", Severity::Warning, format!("Crosses {}", name_or(f, "the neighbour line")))
                };
                let geometry = loc.lines_geojson(&inside).or_else(|| loc.lines_geojson(&MultiLineString(vec![line])));
                out.push(Finding { code: code.into(), severity, text, geometry, targets: target(f) });
            }
            (FeatureKind::FarmBoundary, FeatureGeometry::Polygon(rings)) => {
                let farm = loc.rings(rings);
                let past = shape.difference(&farm);
                if past.unsigned_area() >= OVERLAP_M2 || corner_past(&shape, &farm) {
                    out.push(Finding {
                        code: "crosses_farm_boundary".into(),
                        severity: Severity::Critical,
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
    // Only a farm that has mapped its water can say none is inside; when the
    // farm requires water, none mapped is no evidence of any, and that refuses.
    if waters > 0 && water_inside.is_empty() {
        let severity = if rules.require_water { Severity::Critical } else { Severity::Warning };
        out.push(Finding { code: "no_water".into(), severity, text: "No water inside".into(), geometry: None, targets: vec![] });
    } else if waters == 0 && rules.require_water {
        out.push(Finding {
            code: "no_water".into(),
            severity: Severity::Critical,
            text: "No water mapped, and the farm requires water inside every boundary".into(),
            geometry: None,
            targets: vec![],
        });
    }
    out.extend(water_inside);
    out
}

/// Within [`CONTACT_M`] of each other (overlapping, touching, or nearly).
fn touches(a: &geo::Polygon<f64>, b: &geo::Polygon<f64>) -> bool {
    !a.exterior().0.is_empty() && !b.exterior().0.is_empty() && Euclidean::distance(a, b) < CONTACT_M
}

/// Some corner of `shape` lies more than [`EDGE_TOLERANCE_M`] outside `farm`.
fn corner_past(shape: &geo::Polygon<f64>, farm: &geo::Polygon<f64>) -> bool {
    use geo::Contains;
    let edges: Vec<&LineString<f64>> = std::iter::once(farm.exterior()).chain(farm.interiors()).collect();
    shape.exterior().0.iter().any(|c| {
        let p = geo::Point(*c);
        !farm.contains(&p) && edges.iter().map(|e| Euclidean::distance(&p, *e)).fold(f64::INFINITY, f64::min) > EDGE_TOLERANCE_M
    })
}

/// `p` (its outer ring) grown by `d` metres all round: the ring with every
/// point within `d` of it, as edge strips and corner discs joined to it.
/// Corner discs are drawn outside their circle, so the growth is at least `d` everywhere.
pub(crate) fn grown(p: &Polygon, d: f64) -> Polygon {
    let Some(loc) = Local::new(p) else { return p.clone() };
    let base = loc.rings(&[p.outer_ring()]);
    let pts: Vec<Coord<f64>> = base.exterior().0.clone();
    if pts.len() < 4 || d <= 0.0 {
        return p.clone();
    }
    const SIDES: usize = 16;
    let r = d / (std::f64::consts::PI / SIDES as f64).cos();
    let mut parts: Vec<geo::Polygon<f64>> = Vec::new();
    for w in pts.windows(2) {
        let (a, b) = (w[0], w[1]);
        let (dx, dy) = (b.x - a.x, b.y - a.y);
        let len = dx.hypot(dy);
        if len <= 0.0 {
            continue;
        }
        let (nx, ny) = (-dy / len * d, dx / len * d);
        let quad = vec![
            Coord { x: a.x + nx, y: a.y + ny },
            Coord { x: b.x + nx, y: b.y + ny },
            Coord { x: b.x - nx, y: b.y - ny },
            Coord { x: a.x - nx, y: a.y - ny },
            Coord { x: a.x + nx, y: a.y + ny },
        ];
        parts.push(geo::Polygon::new(LineString(quad), vec![]));
    }
    for c in &pts[..pts.len() - 1] {
        let mut disc: Vec<Coord<f64>> = (0..SIDES)
            .map(|i| {
                let t = std::f64::consts::TAU * i as f64 / SIDES as f64;
                Coord { x: c.x + r * t.cos(), y: c.y + r * t.sin() }
            })
            .collect();
        disc.push(disc[0]);
        parts.push(geo::Polygon::new(LineString(disc), vec![]));
    }
    // One at a time: a union of overlapping pieces in one multipolygon isn't their union.
    let merged = parts.into_iter().fold(MultiPolygon(vec![base.clone()]), |acc, p| acc.union(&MultiPolygon(vec![p])));
    let Some(outer) = merged.0.iter().max_by(|a, b| a.unsigned_area().total_cmp(&b.unsigned_area())) else { return p.clone() };
    // Rounding to 7 decimals moves a corner by under a centimetre: well within NO_GO_STANDOFF_M - CONTACT_M.
    let ring: Vec<LonLat> = outer.exterior().coords().map(|c| loc.back(c)).collect();
    Polygon::from_ring(ring)
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

/// How much of `of` (0 to 1) lies inside `within`, holes counted.
pub fn share_inside(of: &Polygon, within: &Polygon) -> f64 {
    let Some(loc) = Local::new(of) else { return 0.0 };
    let a = loc.poly(of);
    let area = a.unsigned_area();
    if area <= 0.0 {
        return 0.0;
    }
    a.intersection(&loc.poly(within)).unsigned_area() / area
}

/// Local metres about a shape's first corner, for areas, overlaps and clipping.
pub(crate) struct Local {
    proj: Projection,
}

impl Local {
    pub(crate) fn new(p: &Polygon) -> Option<Self> {
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

    const OPEN: MapRules = MapRules { holeless: false, require_water: false };

    #[test]
    fn roads_the_farm_boundary_and_hard_hazards_are_critical() {
        let sent = Polygon::from_ring(rect(0.0, 0.0, 200.0, 100.0));
        let loc = Local::new(&sent).unwrap();
        let road = feature(FeatureKind::Road, FeatureGeometry::LineString(vec![at(-50.0, 50.0), at(250.0, 50.0)]), Some("county road"), json!({}));
        let well = feature(FeatureKind::Hazard, FeatureGeometry::Point(at(20.0, 20.0)), Some("old well"), json!({ "radius_m": 10.0 }));
        let farm = feature(FeatureKind::FarmBoundary, FeatureGeometry::Polygon(vec![rect(-10.0, -10.0, 150.0, 110.0)]), None, json!({}));
        let trough = feature(FeatureKind::Water, FeatureGeometry::Point(at(500.0, 500.0)), Some("north trough"), json!({}));
        let f = map_findings(&loc, &sent, &[road.clone(), well, farm, trough.clone()], OPEN);
        assert_eq!(codes(&f), ["crosses_road", "overlaps_hazard", "crosses_farm_boundary", "no_water"]);
        let sev: Vec<Severity> = f.iter().map(|f| f.severity).collect();
        assert_eq!(sev, [Severity::Critical, Severity::Warning, Severity::Critical, Severity::Warning]);
        assert_eq!(f[0].text, "Crosses county road");
        assert_eq!(f[0].geometry.as_ref().unwrap()["type"], "LineString");
        assert_eq!(f[1].text, "Takes in old well");
        // The part past the farm boundary: 50 x 100 m.
        let past = f[2].geometry.as_ref().unwrap();
        assert_eq!(past["type"], "Polygon");
        // A hazard marked hard is critical; water is when the farm requires it.
        let pit = feature(FeatureKind::Hazard, FeatureGeometry::Point(at(20.0, 20.0)), Some("pit"), json!({ "radius_m": 10.0, "hard": true }));
        let f = map_findings(&loc, &sent, &[pit, trough.clone()], MapRules { require_water: true, ..OPEN });
        assert_eq!(codes(&f), ["overlaps_hazard", "no_water"]);
        assert!(f.iter().all(|f| f.severity == Severity::Critical));
        // Water inside: ringed, and no "no water".
        let pond = feature(FeatureKind::Water, FeatureGeometry::Point(at(100.0, 80.0)), Some("pond"), json!({}));
        let f = map_findings(&loc, &sent, &[trough, pond], OPEN);
        assert_eq!(codes(&f), ["water_inside"]);
        assert_eq!(f[0].text, "Pond inside");
        // No water mapped at all: nothing said about water, unless the farm requires it (then it refuses).
        assert!(map_findings(&loc, &sent, std::slice::from_ref(&road), OPEN).iter().all(|f| !f.code.contains("water")));
        let f = map_findings(&loc, &sent, &[road], MapRules { require_water: true, ..OPEN });
        assert!(f.iter().any(|f| f.code == "no_water" && f.severity == Severity::Critical), "{f:?}");
    }

    #[test]
    fn a_no_go_zone_taken_in_is_critical_and_a_hole_counts_only_for_collars_that_hold_holes() {
        let zone = rect(80.0, 30.0, 120.0, 70.0);
        let creek = feature(FeatureKind::NoGo, FeatureGeometry::Polygon(vec![zone.clone()]), Some("creek"), json!({}));
        // Drawn over it: taken in.
        let sent = Polygon::from_ring(rect(0.0, 0.0, 200.0, 100.0));
        let loc = Local::new(&sent).unwrap();
        let f = map_findings(&loc, &sent, std::slice::from_ref(&creek), OPEN);
        assert_eq!(codes(&f), ["enters_no_go"]);
        assert_eq!((f[0].severity, f[0].text.as_str()), (Severity::Critical, "Takes in creek"));
        // Made a hole with the standoff around it: kept out, unless some collars enforce the outer ring alone.
        let holed = Polygon::from_rings(rect(0.0, 0.0, 200.0, 100.0), [rect(79.0, 29.0, 121.0, 71.0)]);
        let loc = Local::new(&holed).unwrap();
        assert!(map_findings(&loc, &holed, std::slice::from_ref(&creek), OPEN).is_empty());
        // A hole exactly on the zone's edge touches it.
        let flush = Polygon::from_rings(rect(0.0, 0.0, 200.0, 100.0), [zone]);
        let f = map_findings(&Local::new(&flush).unwrap(), &flush, std::slice::from_ref(&creek), OPEN);
        assert_eq!(codes(&f), ["enters_no_go"]);
        let f = map_findings(&loc, &holed, std::slice::from_ref(&creek), MapRules { holeless: true, ..OPEN });
        assert_eq!(codes(&f), ["enters_no_go"]);
        assert!(f[0].text.starts_with("Collars that can't hold holes"));
        // Clear of it: nothing.
        let away = Polygon::from_ring(rect(300.0, 0.0, 400.0, 100.0));
        assert!(map_findings(&Local::new(&away).unwrap(), &away, &[creek], OPEN).is_empty());
    }

    /// Astra 02 F10: contact with a road or no-go zone is a crossing; tolerances are rounding, not permission.
    #[test]
    fn touching_a_road_or_no_go_zone_counts() {
        let sent = Polygon::from_ring(rect(0.0, 0.0, 100.0, 100.0));
        let loc = Local::new(&sent).unwrap();
        let road = |pts: Vec<LonLat>| feature(FeatureKind::Road, FeatureGeometry::LineString(pts), Some("lane"), json!({}));
        // A road ending exactly at a corner, one reaching 0.2 m inside, one 0.3 m off the edge.
        for r in [road(vec![at(-50.0, -50.0), at(0.0, 0.0)]), road(vec![at(-50.0, 50.0), at(0.2, 50.0)]), road(vec![at(-50.0, -0.3), at(150.0, -0.3)])] {
            let f = map_findings(&loc, &sent, &[r], OPEN);
            assert_eq!(codes(&f), ["crosses_road"], "{f:?}");
            assert_eq!(f[0].severity, Severity::Critical);
        }
        // 2 m off: clear.
        assert!(map_findings(&loc, &sent, &[road(vec![at(-50.0, -2.0), at(150.0, -2.0)])], OPEN).is_empty());
        // A no-go sliver 3 mm deep along 100 m of edge.
        let sliver = feature(FeatureKind::NoGo, FeatureGeometry::Polygon(vec![rect(-10.0, 99.997, 110.0, 120.0)]), Some("bank"), json!({}));
        let f = map_findings(&loc, &sent, &[sliver], OPEN);
        assert_eq!(codes(&f), ["enters_no_go"]);
        // A corner 0.2 m past the farm boundary (0.2 m² past it).
        let spike = Polygon::from_ring(vec![
            at(0.0, 0.0),
            at(100.0, 0.0),
            at(100.0, 100.0),
            at(51.0, 100.0),
            at(50.0, 100.2),
            at(49.0, 100.0),
            at(0.0, 100.0),
            at(0.0, 0.0),
        ]);
        let farm = feature(FeatureKind::FarmBoundary, FeatureGeometry::Polygon(vec![rect(0.0, 0.0, 100.0, 100.0)]), None, json!({}));
        assert_eq!(codes(&map_findings(&Local::new(&spike).unwrap(), &spike, std::slice::from_ref(&farm), OPEN)), ["crosses_farm_boundary"]);
        assert!(map_findings(&loc, &sent, &[farm], OPEN).is_empty(), "on the farm boundary is inside it");
    }

    #[test]
    fn a_grown_zone_keeps_the_standoff() {
        let zone = Polygon::from_ring(vec![at(0.0, 0.0), at(100.0, 0.0), at(50.0, 5.0), at(0.0, 0.0)]);
        let g = grown(&zone, NO_GO_STANDOFF_M);
        let loc = Local::new(&zone).unwrap();
        let d = Euclidean::distance(&loc.poly(&zone).exterior().clone(), &loc.poly(&g).exterior().clone());
        assert!(d > CONTACT_M && d < NO_GO_STANDOFF_M * 1.1, "{d}");
        assert!(loc.poly(&g).unsigned_area() > loc.poly(&zone).unsigned_area());
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
