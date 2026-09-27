//! Words for a spot on the farm, for texts and replies: "60 m N of east gate",
//! "in P3", "60 m N of P3".

use op_geo::Projection;

use crate::Ctx;
use crate::domain::{LonLat, Paddock};
use crate::features::{FeatureGeometry, FeatureKind, MapFeature, list_features};
use crate::units::Fmt;

/// How far a named gate, water or shade still names a spot.
pub const LANDMARK_M: f64 = 200.0;
/// Closer than this, a spot is "at" the landmark.
const AT_M: f64 = 10.0;

/// Words for a spot, for texts and replies. The nearest named gate, water or
/// shade in effect now, within [`LANDMARK_M`]: "60 m N of east gate", "at
/// east gate", "in north pond". Else by paddock: "in P3" inside one (the
/// smallest that holds it), "60 m N of P3" from the nearest paddock edge.
/// `None` when neither applies. Distances through `op_core::units`.
pub async fn describe(ctx: &Ctx, at: LonLat) -> anyhow::Result<Option<String>> {
    let fmt = Fmt::of(ctx).await?;
    let landmarks = list_features(ctx, None, None, Some(crate::time::now())).await?;
    if let Some(words) = describe_near(&landmarks, at, &fmt) {
        return Ok(Some(words));
    }
    let paddocks = ctx.store().list_paddocks().await?;
    Ok(describe_among(&paddocks, at, &fmt))
}

/// [`describe`]'s landmark part over a given feature list: only named gates,
/// water and shade count; the nearest within [`LANDMARK_M`] wins (the first
/// listed on a tie).
pub fn describe_near(features: &[MapFeature], at: LonLat, fmt: &Fmt) -> Option<String> {
    let proj = Projection::new(at);
    let mut best: Option<(&str, Near)> = None;
    for f in features {
        if !matches!(f.kind, FeatureKind::Gate | FeatureKind::Water | FeatureKind::Shade) {
            continue;
        }
        let Some(name) = f.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) else { continue };
        let Some(near) = nearest_on(&proj, &f.geometry, at) else { continue };
        if near.dist <= LANDMARK_M && best.as_ref().is_none_or(|b| near.dist < b.1.dist) {
            best = Some((name, near));
        }
    }
    let (name, near) = best?;
    Some(if near.inside {
        format!("in {name}")
    } else if near.dist < AT_M {
        format!("at {name}")
    } else {
        // From the landmark to the spot.
        format!("{} {} of {name}", fmt.len(near.dist), compass(-near.dx, -near.dy))
    })
}

/// Where a feature is from a spot (the projection origin): metres to its
/// closest point, that point's east/north offset, and whether the spot is
/// inside it (polygons only).
struct Near {
    dist: f64,
    dx: f64,
    dy: f64,
    inside: bool,
}

fn nearest_on(proj: &Projection, g: &FeatureGeometry, at: LonLat) -> Option<Near> {
    match g {
        FeatureGeometry::Point(p) => {
            let [dx, dy] = proj.forward(*p);
            Some(Near { dist: dx.hypot(dy), dx, dy, inside: false })
        }
        FeatureGeometry::Polygon(rings) => {
            let poly = crate::domain::Polygon { kind: Default::default(), coordinates: rings.clone() };
            if poly.contains(at) {
                return Some(Near { dist: 0.0, dx: 0.0, dy: 0.0, inside: true });
            }
            let ring_list: Vec<Vec<LonLat>> = std::iter::once(poly.outer_ring()).chain(poly.holes()).collect();
            let (dist, dx, dy) = ring_list.iter().filter_map(|r| nearest_edge(proj, r)).min_by(|a, b| a.0.total_cmp(&b.0))?;
            Some(Near { dist, dx, dy, inside: false })
        }
        FeatureGeometry::LineString(_) => None,
    }
}

/// [`describe`] over a given paddock list.
pub fn describe_among(paddocks: &[Paddock], at: LonLat, fmt: &Fmt) -> Option<String> {
    if let Some(p) = paddocks.iter().filter(|p| p.geometry.contains(at)).min_by(|a, b| a.area_ha.total_cmp(&b.area_ha)) {
        return Some(format!("in {}", p.name));
    }
    let proj = Projection::new(at);
    let (p, (dist, dx, dy)) =
        paddocks.iter().filter_map(|p| nearest_edge(&proj, &p.geometry.outer_ring()).map(|n| (p, n))).min_by(|a, b| a.1.0.total_cmp(&b.1.0))?;
    // From the edge to the spot.
    Some(format!("{} {} of {}", fmt.len(dist), compass(-dx, -dy), p.name))
}

/// Distance in metres from the projection origin (the spot) to the closest
/// point of the ring, and that point's east/north offset.
fn nearest_edge(proj: &Projection, ring: &[LonLat]) -> Option<(f64, f64, f64)> {
    if ring.len() < 2 {
        return None;
    }
    let pts = proj.forward_ring(ring);
    let mut best: Option<(f64, f64, f64)> = None;
    for i in 0..pts.len() {
        let [ax, ay] = pts[i];
        let [bx, by] = pts[(i + 1) % pts.len()];
        let (vx, vy) = (bx - ax, by - ay);
        let len2 = vx * vx + vy * vy;
        let t = if len2 > 0.0 { (-(ax * vx + ay * vy) / len2).clamp(0.0, 1.0) } else { 0.0 };
        let (cx, cy) = (ax + t * vx, ay + t * vy);
        let d = cx.hypot(cy);
        if best.is_none_or(|b| d < b.0) {
            best = Some((d, cx, cy));
        }
    }
    best
}

/// Eight-point compass direction of an east/north offset.
fn compass(dx: f64, dy: f64) -> &'static str {
    const NAMES: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
    let deg = dx.atan2(dy).to_degrees().rem_euclid(360.0);
    NAMES[((deg + 22.5) / 45.0) as usize % 8]
}

#[cfg(test)]
mod tests {
    use super::compass;

    #[test]
    fn compass_points() {
        assert_eq!(compass(0.0, 1.0), "N");
        assert_eq!(compass(1.0, 1.0), "NE");
        assert_eq!(compass(1.0, 0.0), "E");
        assert_eq!(compass(0.0, -1.0), "S");
        assert_eq!(compass(-1.0, 0.1), "W");
        assert_eq!(compass(-1.0, 1.0), "NW");
    }
}
