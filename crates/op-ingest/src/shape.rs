//! Every herd boundary on its way to the collars (field-ready §2.12).
//!
//! [`prepare`] runs before a herd boundary is stored, on every path: the
//! farmer's draw, an applied decision, each sweep step, a reissue for a moved
//! collar (and later S's staged schedule steps). It validates the shape and
//! fits it to the strictest limits among the herd's collars that hold holes.
//! F adds exclusions to its body.
//!
//! [`command_for`] is what one collar downloads, computed on request from the
//! stored boundary and the collar's caps: fitted to its limits, with holes,
//! `collar_id` and `cue_mode` only when its caps say it takes them. A legacy
//! collar (firmware 0.1, no caps) gets the outer ring fitted to 64 vertices
//! and no holes. [`fence_geometry`] is the shape that collar enforces, for
//! the server-side fence, so the server's idea of inside matches the collar's.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

use op_core::check::Finding;
use op_core::{ApiError, ApiResult, Boundary, Collar, Ctx, Severity};
use op_geo::shape::{self, SERVER_SLACK_M};
use op_geo::{CollarLimits, Polygon};
use op_protocol::{BoundaryCommand, caps, sign_command};
use sqlx::Row;

use crate::{SendOpts, margins};

/// A boundary ready to store: the shape as it goes to the collars that hold
/// holes, the margins it goes with, and what the farmer should know.
#[derive(Debug, Clone, PartialEq)]
pub struct Prepared {
    pub geometry: Polygon,
    pub warn_m: f64,
    pub hysteresis_m: f64,
    pub findings: Vec<Finding>,
}

/// Validate and fit a herd boundary before it is stored. Missing margins come
/// from [`margins::default_margins`]. An invalid shape, or one that can't be
/// fitted to the herd's collars, is 400. Findings: `simplified` when fitting
/// changed the shape, `collars_no_holes` when it has holes and some of the
/// herd's collars can't hold them (they get the outer ring alone).
pub async fn prepare(ctx: &Ctx, herd_id: &str, target: &Polygon, opts: &SendOpts) -> ApiResult<Prepared> {
    let (default_warn, default_hyst) = margins::default_margins(ctx, herd_id).await?;
    let warn_m = opts.warn_m.unwrap_or(default_warn);
    let hysteresis_m = opts.hysteresis_m.unwrap_or(default_hyst);
    for (v, name) in [(warn_m, "warn_m"), (hysteresis_m, "hysteresis_m")] {
        if !v.is_finite() || !(0.0..=shape::MAX_MARGIN_M).contains(&v) {
            return Err(ApiError::bad_request(format!("{name} must be between 0 and 1000 metres.")));
        }
    }
    let geometry = target.validated()?;
    let collars = herd_collars(ctx.db(), herd_id).await?;
    let limits = strictest(collars.iter().map(|c| &c.caps));
    let gap = shape::min_gap_m(warn_m) + SERVER_SLACK_M;
    let fitted = shape::fit_gap(&geometry, &limits, gap);
    if let Err(code) = shape::check(&fitted, &limits, warn_m, hysteresis_m, SERVER_SLACK_M) {
        let msg = match code {
            shape::ShapeCode::HoleTooClose => {
                let fmt = op_core::units::Fmt::of(ctx).await?;
                format!("Holes need {} between them and from the edge.", fmt.len(gap))
            }
            c => c.message().to_owned(),
        };
        return Err(ApiError::bad_request(msg));
    }
    let mut findings = Vec::new();
    if fitted.coordinates != geometry.coordinates {
        findings.push(Finding {
            code: "simplified".into(),
            severity: Severity::Info,
            text: "Simplified to fit the collars".into(),
            geometry: None,
            targets: vec![],
        });
    }
    if fitted.coordinates.len() > 1 {
        let no_holes: Vec<&HerdCollar> = collars.iter().filter(|c| c.parked.is_none() && !c.caps.holds_holes()).collect();
        if !no_holes.is_empty() {
            let n = no_holes.len();
            findings.push(Finding {
                code: "collars_no_holes".into(),
                severity: Severity::Warning,
                text: if n == 1 { "1 collar can't hold holes".into() } else { format!("{n} collars can't hold holes") },
                geometry: None,
                targets: no_holes.iter().map(|c| ("collar".to_owned(), c.id.clone())).collect(),
            });
        }
    }
    Ok(Prepared { geometry: fitted, warn_m, hysteresis_m, findings })
}

/// What a collar can take, from its row: no caps is a legacy collar
/// ([`CollarLimits::LEGACY`]); caps without limits are [`CollarLimits::V0`].
#[derive(Debug, Clone, PartialEq)]
pub struct CollarCaps {
    pub fw: Option<String>,
    pub caps: Vec<String>,
    pub limits: CollarLimits,
}

impl CollarCaps {
    pub fn has(&self, cap: &str) -> bool {
        self.caps.iter().any(|c| c == cap)
    }

    pub fn of(collar: &Collar, limits_json: Option<&str>) -> Self {
        Self::from_parts(collar.fw.clone(), collar.caps.clone(), limits_json)
    }

    pub(crate) fn from_parts(fw: Option<String>, caps: Vec<String>, limits_json: Option<&str>) -> Self {
        let limits = if caps.is_empty() { CollarLimits::LEGACY } else { limits_json.and_then(|s| serde_json::from_str(s).ok()).unwrap_or(CollarLimits::V0) };
        Self { fw, caps, limits }
    }

    /// Reads the `caps` and `limits` columns of a `collars` row.
    pub(crate) fn from_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Self> {
        let caps: Option<String> = r.try_get("caps")?;
        let caps: Vec<String> = caps.map(|s| serde_json::from_str(&s)).transpose()?.unwrap_or_default();
        let limits: Option<String> = r.try_get("limits")?;
        Ok(Self::from_parts(r.try_get("fw")?, caps, limits.as_deref()))
    }

    /// Takes holes: the `holes` cap and room for at least one.
    pub fn holds_holes(&self) -> bool {
        self.has(caps::HOLES) && self.limits.holes > 0
    }

    /// The limits a boundary is fitted to for this collar: no holes without the cap.
    pub fn fit_limits(&self) -> CollarLimits {
        let mut l = self.limits;
        if !self.holds_holes() {
            l.holes = 0;
            l.hole_vertices = 0;
        }
        l
    }
}

/// A herd's collar and what it can take.
pub(crate) struct HerdCollar {
    pub id: String,
    pub caps: CollarCaps,
    pub parked: Option<String>,
}

pub(crate) async fn herd_collars(db: &sqlx::SqlitePool, herd_id: &str) -> anyhow::Result<Vec<HerdCollar>> {
    let rows = sqlx::query("SELECT id, fw, caps, limits, parked_at FROM collars WHERE herd_id = ? ORDER BY created_at, id").bind(herd_id).fetch_all(db).await?;
    rows.iter().map(|r| Ok(HerdCollar { id: r.try_get("id")?, caps: CollarCaps::from_row(r)?, parked: r.try_get("parked_at")? })).collect()
}

/// Field-wise minimum over the collars that hold holes; V0 when none report caps.
pub(crate) fn strictest<'a>(caps: impl IntoIterator<Item = &'a CollarCaps>) -> CollarLimits {
    caps.into_iter().filter(|c| c.holds_holes()).map(|c| c.limits).reduce(|a, b| a.min(&b)).unwrap_or(CollarLimits::V0)
}

/// The limits a herd's boundaries are fitted to (see [`prepare`]).
pub async fn herd_limits(ctx: &Ctx, herd_id: &str) -> anyhow::Result<CollarLimits> {
    Ok(strictest(herd_collars(ctx.db(), herd_id).await?.iter().map(|c| &c.caps)))
}

/// The signed command one collar downloads for a stored boundary. The command
/// id is the boundary id.
pub fn command_for(ctx: &Ctx, b: &Boundary, caps: &CollarCaps) -> ApiResult<BoundaryCommand> {
    let mut cmd = unsigned_command(b, caps)?;
    sign_command(&mut cmd, ctx.signing_key());
    Ok(cmd)
}

fn unsigned_command(b: &Boundary, caps: &CollarCaps) -> ApiResult<BoundaryCommand> {
    let g = fitted(b, &caps.fit_limits());
    let mut cmd = BoundaryCommand::from_shape(b.id.clone(), b.version, &g, b.effective_at)?;
    cmd.herd_id = Some(b.herd_id.clone());
    if caps.has(caps::COLLAR_ID) {
        cmd.collar_id = b.collar_id.clone();
    }
    cmd.warn_m = Some(b.warn_m);
    cmd.hysteresis_m = Some(b.hysteresis_m);
    Ok(cmd)
}

/// The shape a collar with `caps` enforces for `b`: the command's rings.
pub fn fence_geometry(b: &Boundary, caps: &CollarCaps) -> Polygon {
    match unsigned_command(b, caps) {
        Ok(cmd) => cmd.polygon(),
        Err(_) => (*fitted(b, &caps.fit_limits())).clone(),
    }
}

/// Flash bytes the command for `b` takes in a collar with `caps`.
pub(crate) fn record_bytes(b: &Boundary, caps: &CollarCaps) -> ApiResult<usize> {
    Ok(unsigned_command(b, caps)?.record_bytes())
}

/// Stored boundaries never change, so each is fitted once per set of limits.
type FitCache = HashMap<(String, CollarLimits), Arc<Polygon>>;
static FITTED: LazyLock<Mutex<FitCache>> = LazyLock::new(Default::default);
const FIT_CACHE_MAX: usize = 4096;

fn fitted(b: &Boundary, limits: &CollarLimits) -> Arc<Polygon> {
    let key = (b.id.clone(), *limits);
    if let Some(p) = FITTED.lock().unwrap_or_else(|e| e.into_inner()).get(&key) {
        return p.clone();
    }
    let p = Arc::new(shape::fit_gap(&b.geometry, limits, shape::min_gap_m(b.warn_m) + SERVER_SLACK_M));
    let mut cache = FITTED.lock().unwrap_or_else(|e| e.into_inner());
    if cache.len() >= FIT_CACHE_MAX {
        cache.clear();
    }
    cache.insert(key, p.clone());
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use op_geo::Projection;

    const O: [f64; 2] = [-93.6225, 42.0318];

    fn at(x: f64, y: f64) -> [f64; 2] {
        Projection::new(O).inverse([x, y])
    }
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Vec<[f64; 2]> {
        vec![at(x0, y0), at(x1, y0), at(x1, y1), at(x0, y1), at(x0, y0)]
    }
    fn circle(cx: f64, cy: f64, r: f64, n: usize) -> Vec<[f64; 2]> {
        let mut v: Vec<[f64; 2]> = (0..n)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / n as f64;
                at(cx + r * a.cos(), cy + r * a.sin())
            })
            .collect();
        v.push(v[0]);
        v
    }
    fn boundary(g: Polygon) -> Boundary {
        Boundary {
            id: op_core::id::new_id(op_core::id::BOUNDARY),
            herd_id: "herd_1".into(),
            version: 7,
            geometry: g,
            warn_m: 5.0,
            hysteresis_m: 1.0,
            effective_at: None,
            decision_id: "dec_1".into(),
            created_at: op_core::time::now(),
            collar_id: Some("col_1".into()),
        }
    }
    fn v0() -> CollarCaps {
        CollarCaps { fw: Some("0.2.0".into()), caps: caps::ALL.iter().map(|s| s.to_string()).collect(), limits: CollarLimits::V0 }
    }
    fn legacy() -> CollarCaps {
        CollarCaps { fw: None, caps: vec![], limits: CollarLimits::LEGACY }
    }

    #[test]
    fn caps_from_the_row() {
        assert_eq!(CollarCaps::from_parts(None, vec![], None).limits, CollarLimits::LEGACY);
        assert_eq!(CollarCaps::from_parts(None, vec!["holes".into()], None).limits, CollarLimits::V0);
        let v1 = serde_json::to_string(&CollarLimits::V1).unwrap();
        assert_eq!(CollarCaps::from_parts(None, vec!["holes".into()], Some(&v1)).limits, CollarLimits::V1);
        assert!(!legacy().holds_holes());
        let no_holes_cap = CollarCaps { caps: vec!["slots".into()], ..v0() };
        assert_eq!(no_holes_cap.fit_limits().holes, 0);
    }

    #[test]
    fn strictest_ignores_collars_without_holes() {
        let small = CollarCaps { limits: CollarLimits { outer: 100, ..CollarLimits::V1 }, ..v0() };
        assert_eq!(strictest([&legacy()]), CollarLimits::V0);
        assert_eq!(strictest([&legacy(), &small]).outer, 100);
        assert_eq!(strictest([&small, &v0()]).slots, 16);
    }

    #[test]
    fn legacy_collars_get_one_ring_of_at_most_64_and_the_same_fence() {
        let g = Polygon::from_rings(circle(0.0, 0.0, 200.0, 120), [circle(0.0, 0.0, 40.0, 12)]);
        let b = boundary(g);
        let v = unsigned_command(&b, &v0()).unwrap();
        assert_eq!((v.boundary.len(), v.holes.len()), (120, 1));
        assert_eq!(v.collar_id.as_deref(), Some("col_1"));
        let l = unsigned_command(&b, &legacy()).unwrap();
        assert!(l.holes.is_empty() && l.boundary.len() <= 64, "{} corners", l.boundary.len());
        assert!(l.collar_id.is_none(), "not in its caps");
        l.check(Some("herd_1"), None, &CollarLimits::LEGACY, &Default::default()).unwrap();
        let fence = fence_geometry(&b, &legacy());
        assert_eq!(fence, l.polygon());
        // Fitting only shrinks the outer ring.
        let centre = at(0.0, 0.0);
        assert!(fence.contains(centre), "no hole for a legacy collar");
        assert!(!fence.contains(at(0.0, 201.0)));
    }

    #[test]
    fn a_v0_shaped_boundary_serializes_as_before() {
        let b = Boundary { collar_id: None, ..boundary(Polygon::from_ring(rect(0.0, 0.0, 100.0, 80.0))) };
        let old = {
            let mut c = BoundaryCommand::from_polygon(b.id.clone(), b.version, &b.geometry, None).unwrap();
            c.herd_id = Some(b.herd_id.clone());
            c.warn_m = Some(5.0);
            c.hysteresis_m = Some(1.0);
            c
        };
        for caps in [v0(), legacy()] {
            assert_eq!(serde_json::to_string(&unsigned_command(&b, &caps).unwrap()).unwrap(), serde_json::to_string(&old).unwrap());
        }
    }
}
