//! Words for a spot on the farm, for texts and replies: "in P3", "60 m N of P3".

use op_geo::Projection;

use crate::Ctx;
use crate::domain::{LonLat, Paddock};
use crate::units::Fmt;

/// Words for a spot, for texts and replies: "in P3" inside a paddock (the
/// smallest that holds it), else "60 m N of P3" from the nearest paddock
/// edge. `None` when the farm has no paddocks. Distances through
/// `op_core::units`.
pub async fn describe(ctx: &Ctx, at: LonLat) -> anyhow::Result<Option<String>> {
    let paddocks = ctx.store().list_paddocks().await?;
    let fmt = Fmt::of(ctx).await?;
    Ok(describe_among(&paddocks, at, &fmt))
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
