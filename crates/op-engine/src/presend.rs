//! The pre-send check (field-ready §2.13): `POST /api/herds/{id}/check` and
//! the MCP tool `check_boundary`. What a boundary would do before it goes to
//! the collars, in SI:
//!
//! - `sent`: the shape as the collars get it, exactly what sending it now
//!   stores (it goes through the same [`op_ingest::prepare`]: exclusion
//!   holes and cuts, fitting); `legacy`: the ring a firmware 0.1 collar
//!   enforces, when the herd has one.
//! - facts: area, head, area per head, forage and grazing days from the
//!   containing paddock's forage (a measured height, else imagery;
//!   [`calc::grazing_days`] of forage × area, as strips), rest days, corners,
//!   holes, and the sweep's minutes.
//! - findings: [`op_ingest::prepare`]'s (map features, collars, slots) plus
//!   `forage_short`, `area_per_head_low`, `rested_short` and `weak_coverage`
//!   (from G's coverage day tables, never raw fixes).
//! - `sweep`: when asked, the sweep that would walk the herd in, found by
//!   running the move driver's own [`moves::advance`] on the herd's current
//!   positions: back lines about every 10 m and the minutes it takes.
//!
//! The check never blocks sending; only a shape that can't be sent is 400.

use axum::extract::{Path, State};
use axum::routing::post;
use axum::{Json, Router};
use chrono::{DateTime, Duration, Utc};
use op_analytics::coverage::{self, Metric};
use op_analytics::range::TimeRange;
use op_core::check::Finding;
use op_core::tools::{ToolCall, ToolSpec};
use op_core::units::Fmt;
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Herd, LonLat, Paddock, Polygon, Role, Severity, Species, time};
use op_geo::{Projection, point_in_ring};
use op_ingest::SendOpts;
use op_ingest::moves::{self, MoveState, Next, Situation, Sweep};
use op_ingest::planner::Frame;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

use crate::{calc, db, signals, strips};

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/herds/{id}/check", post(route))
}

/// Rested less than this since last grazed: `rested_short` (three weeks of regrowth).
pub const MIN_REST_DAYS: f64 = 21.0;
/// Less grass than this many days for the herd: `forage_short`.
pub const SHORT_FORAGE_DAYS: f64 = 0.5;
/// A 10 m coverage cell is weak from this median accuracy (m)…
pub const WEAK_ACCURACY_M: f64 = 5.0;
/// …or below this share of the fixes its collars should have sent.
pub const WEAK_FIX_SHARE: f64 = 0.9;
/// Fewer weak cells than this inside the shape is noise.
const WEAK_MIN_CELLS: usize = 3;
/// Coverage looks back this far.
const COVERAGE_DAYS: i64 = 7;
/// Back lines in the preview are about this far apart (m).
pub const BACK_LINE_EVERY_M: f64 = 10.0;
/// Pieces of a back line closer than this (m) are drawn as one.
const BACK_LINE_JOIN_M: f64 = 60.0;
/// A cued animal in the preview walks at most this far ahead along the sweep (m).
const WALK_AHEAD_M: f64 = 12.0;
/// A cued animal in the preview walks this far clear of the warning band (m).
pub const CLEAR_OF_BAND_M: f64 = 1.0;
/// The preview gives up past this many steps, or this many steps waiting.
const MAX_STEPS: u32 = 600;
const MAX_WAITS: u32 = 40;

/// Area per head below which a shape is tight (m²).
pub fn tight_m2_per_head(species: Species) -> f64 {
    match species {
        Species::Cattle => 25.0,
        Species::Sheep | Species::Goats => 4.0,
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckRequest {
    pub geometry: Polygon,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub warn_m: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_at: Option<DateTime<Utc>>,
    /// Preview the sweep that walks the herd in.
    #[serde(default)]
    pub sweep: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckFacts {
    pub area_ha: f64,
    pub head: u32,
    pub m2_per_head: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grazing_days: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forage_kg_dm: Option<f64>,
    /// `ndvi` or `measured`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forage_source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rest_days: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep_minutes: Option<f64>,
    pub vertices: usize,
    pub holes: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepPreview {
    /// Where the back of the sweep will be, first to last, about every 10 m.
    pub back_lines: Vec<Vec<LonLat>>,
    pub minutes: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckResult {
    pub sent: Polygon,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy: Option<Polygon>,
    pub facts: CheckFacts,
    pub findings: Vec<Finding>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sweep: Option<SweepPreview>,
}

async fn route(State(ctx): State<Ctx>, Path(herd_id): Path<String>, ApiJson(req): ApiJson<CheckRequest>) -> ApiResult<Json<CheckResult>> {
    Ok(Json(check(&ctx, &herd_id, &req).await?))
}

/// Check a boundary for a herd (see the module docs).
pub async fn check(ctx: &Ctx, herd_id: &str, req: &CheckRequest) -> ApiResult<CheckResult> {
    let store = ctx.store();
    let herd = store.get_herd(herd_id).await?.ok_or_else(|| ApiError::not_found("No such herd."))?;
    let opts = SendOpts { warn_m: req.warn_m, hysteresis_m: None, effective_at: req.effective_at };
    let prepared = op_ingest::prepare(ctx, herd_id, &req.geometry, &opts).await?;
    let sent = prepared.geometry;
    let now = time::now();
    let fmt = Fmt::of(ctx).await?;
    let legacy = match op_ingest::prepare::has_legacy_collars(ctx, herd_id).await? {
        true => Some(op_ingest::prepare::legacy_fence(&sent, prepared.warn_m)?),
        false => None,
    };

    let paddocks = store.list_paddocks().await?;
    let paddock = sent.interior_point().and_then(|c| signals::paddock_at(&paddocks, c)).cloned();
    let area_ha = sent.area_ha();
    let (head, au) = strips::feeding(Some(&herd), None);
    let m2_per_head = if head > 0 { calc::round(area_ha * 10_000.0 / head as f64, 1) } else { 0.0 };
    let forage = match &paddock {
        Some(p) => strips::paddock_forage(ctx, p).await?,
        None => None,
    };
    let forage_kg_dm = forage.as_ref().map(|f| calc::round(f.kg_dm_per_ha * area_ha, 0));
    // The rule strip days use, so a strip and its check say the same days.
    let grazing_days = forage_kg_dm.and_then(|kg| calc::grazing_days(kg, au));
    let forage_source = forage.and_then(|f| f.source).map(|s| if s == "imagery" { "ndvi".to_owned() } else { s });
    let rest_days = match &paddock {
        Some(p) => rest_days(ctx, p, now).await?,
        None => None,
    };

    let mut findings = prepared.findings;
    if let Some(d) = grazing_days.filter(|d| *d < SHORT_FORAGE_DAYS) {
        findings.push(finding("forage_short", Severity::Warning, format!("Grass for {d:.1} d"), None, vec![]));
    }
    if head > 0 && m2_per_head < tight_m2_per_head(herd.species) {
        findings.push(finding("area_per_head_low", Severity::Warning, format!("Only {}", fmt.per_head(m2_per_head)), None, vec![]));
    }
    // Rest counts when the herd would move onto ground it isn't on now.
    if let (Some(p), Some(r)) = (&paddock, rest_days)
        && herd.paddock_id.as_deref() != Some(p.id.as_str())
        && r < MIN_REST_DAYS
    {
        let text = if r < 1.0 { format!("{} grazed in the last day", p.name) } else { format!("{} rested {r:.0} d", p.name) };
        findings.push(finding("rested_short", Severity::Warning, text, None, vec![("paddock".into(), p.id.clone())]));
    }
    if let Some(f) = weak_coverage(ctx, &sent, now, &fmt).await? {
        findings.push(f);
    }

    let sweep = match req.sweep {
        true => sweep_preview(ctx, &herd, &sent, prepared.warn_m, now).await?,
        false => None,
    };
    if sweep.is_some() {
        // The sweep walks them in: shown by its back lines, not a warning.
        for f in findings.iter_mut().filter(|f| f.code == "animals_outside") {
            f.severity = Severity::Info;
        }
    }
    op_ingest::prepare::sort(&mut findings);

    let facts = CheckFacts {
        area_ha: calc::round(area_ha, 3),
        head,
        m2_per_head,
        grazing_days,
        forage_kg_dm,
        forage_source,
        rest_days,
        sweep_minutes: sweep.as_ref().map(|s| s.minutes),
        vertices: sent.total_vertices(),
        holes: sent.coordinates.len().saturating_sub(1),
    };
    Ok(CheckResult { sent, legacy, facts, findings, sweep })
}

fn finding(code: &str, severity: Severity, text: String, geometry: Option<Value>, targets: Vec<(String, String)>) -> Finding {
    Finding { code: code.into(), severity, text, geometry, targets }
}

/// Days since the paddock was last grazed, from the record: 0 while a herd with
/// head is in it, else the latest of its `grazed_until`, an applied move out of it,
/// and the last day collars grazed it ([`signals::collar_grazed`]: a real
/// share of a herd's tracked day there, from hot fixes, rolled-up days and
/// imported history, so fixes across a fence from the herd next door don't
/// count). Any herd counts. A few small reads, never a walk of the fixes, so
/// it answers while the farmer draws.
pub async fn rest_days(ctx: &Ctx, p: &Paddock, now: DateTime<Utc>) -> anyhow::Result<Option<f64>> {
    let herds = ctx.store().list_herds().await?;
    // A herd with head in it grazes it now. An empty one (the Training herd
    // once its animals went back) isn't grazing anything.
    if herds.iter().any(|h| h.count > 0 && h.paddock_id.as_deref() == Some(p.id.as_str())) {
        return Ok(Some(0.0));
    }
    let history = db::list(ctx, None, 100).await?;
    let mut last = signals::last_grazed(ctx, None, std::slice::from_ref(p), None, &history, now).await?.remove(&p.id).flatten();
    let since = now - Duration::days(signals::REST_LOOKBACK_DAYS);
    // A collar clock running ahead counts as now.
    for at in signals::collar_grazed(ctx, signals::Grazer::Paddock(&p.id), since, None).await?.into_values() {
        last = last.max(Some(at.min(now)));
    }
    Ok(last.map(|at| calc::round((now - at).num_milliseconds().max(0) as f64 / 86_400_000.0, 1)))
}

/// Weak GPS inside the shape over the last week: 10 m cells whose median
/// accuracy is 5 m or worse, or that got under 90 % of their fixes.
async fn weak_coverage(ctx: &Ctx, sent: &Polygon, now: DateTime<Utc>, fmt: &Fmt) -> anyhow::Result<Option<Finding>> {
    let Some(bbox) = sent.bbox() else { return Ok(None) };
    let range = TimeRange::new(now - Duration::days(COVERAGE_DAYS), now);
    let mut weak: Vec<[f64; 4]> = Vec::new();
    for (metric, is_weak) in [(Metric::Accuracy, (|v: f64| v >= WEAK_ACCURACY_M) as fn(f64) -> bool), (Metric::Fixes, |v: f64| v < WEAK_FIX_SHARE)] {
        for c in coverage::grid(ctx, bbox, range, metric).await? {
            if is_weak(c.value) && sent.contains([c.lon, c.lat]) && !weak.contains(&c.bbox) {
                weak.push(c.bbox);
            }
        }
    }
    if weak.len() < WEAK_MIN_CELLS {
        return Ok(None);
    }
    let Some((geometry, ha)) = op_ingest::prepare::clip_boxes(sent, &weak) else { return Ok(None) };
    Ok(Some(finding("weak_coverage", Severity::Warning, format!("Weak GPS on {}", fmt.area(ha)), Some(geometry), vec![])))
}

/// The sweep that would walk the herd into `target` (already prepared), from
/// where its collars are now: the move driver's own planner and step rule
/// ([`moves::advance`]) run forward, each step [`moves::STEP_EVERY`] or more
/// apart, with every animal a step cues (inside the warning band of its edge)
/// walking on along the sweep until 1 m clear of the band, and again at each
/// wait while it is still in it. Its minutes are
/// the length of the sweep over the pace ([`pace_m_per_min`]), plus half a
/// base report interval for the first step to reach the collars. `None` when
/// the herd is already in it, has no fresh positions, or the sweep can't
/// finish.
pub async fn sweep_preview(ctx: &Ctx, herd: &Herd, target: &Polygon, warn_m: f64, now: DateTime<Utc>) -> anyhow::Result<Option<SweepPreview>> {
    let sit = op_ingest::prepare::situation(ctx, &herd.id, now).await?;
    let head = sit.positions.len();
    let target = target.clone();
    // The planner runs once a step: off the async threads.
    let Some(steps) = tokio::task::spawn_blocking(move || simulate(&target, warn_m, sit, now)).await? else { return Ok(None) };
    let pace = pace_m_per_min(ctx, &herd.id, head).await?;
    let c = op_ingest::config::load(ctx).await?;
    // The first step reaches each collar at its next report on the base cadence.
    let first = c.report_s as f64 / 2.0 / 60.0;
    let minutes = calc::round(first + steps.start_m / pace, 1);
    Ok(Some(SweepPreview { back_lines: steps.back_lines, minutes }))
}

/// Steps of a simulated sweep: how many boundaries it sends, its back lines,
/// and how far its back line has to go (the first step's `remaining_m`).
#[derive(Debug, Clone, PartialEq)]
pub struct Simulated {
    pub count: u32,
    pub back_lines: Vec<Vec<LonLat>>,
    pub start_m: f64,
}

/// Run [`moves::advance`] forward from `sit` (see [`sweep_preview`]). Pure.
pub fn simulate(target: &Polygon, warn_m: f64, mut sit: Situation, start: DateTime<Utc>) -> Option<Simulated> {
    if sit.positions.is_empty() {
        return None;
    }
    let proj = Projection::new(target.centroid()?);
    let mut sweep = Sweep::default();
    let mut stragglers: Vec<String> = Vec::new();
    let (mut step, mut waits) = (0u32, 0u32);
    let mut back_lines = Vec::new();
    let mut last_level = f64::NEG_INFINITY;
    let mut t = start;
    while step < MAX_STEPS {
        let out = moves::advance(&MoveState { target, warn_m, step, sweep: &sweep, stragglers: &stragglers }, &sit, t);
        sweep = out.sweep;
        stragglers = out.stragglers;
        match out.next {
            Next::Send { last: true, .. } if step == 0 => return None,
            Next::Send { last: true, .. } => return Some(Simulated { count: step + 1, back_lines, start_m: sweep.start_m.unwrap_or(0.0) }),
            Next::Send { polygon, .. } => {
                step += 1;
                waits = 0;
                if let (Some(frame), Some(level)) = (sweep.frame, sweep.level)
                    && (back_lines.is_empty() || level >= last_level + BACK_LINE_EVERY_M)
                    && let Some(line) = back_line(&proj, &polygon, frame, level)
                {
                    back_lines.push(line);
                    last_level = level;
                }
                cue(&proj, &mut sit.positions, &polygon, warn_m, sweep.frame, &stragglers);
                sit.active = Some(polygon);
            }
            Next::Wait => {
                waits += 1;
                if waits > MAX_WAITS {
                    return None;
                }
                // Cued animals keep being cued while they stay in the band (@L).
                if let Some(active) = sit.active.clone() {
                    cue(&proj, &mut sit.positions, &active, warn_m, sweep.frame, &stragglers);
                }
            }
        }
        t += moves::STEP_EVERY;
    }
    None
}

type P = [f64; 2];

/// Animals inside `step` within the warning band of its edge walk until 1 m
/// clear of the band: on along the sweep, else away from the nearest edge.
fn cue(proj: &Projection, positions: &mut [(String, LonLat)], step: &Polygon, warn_m: f64, frame: Option<Frame>, stragglers: &[String]) {
    let ring: Vec<P> = proj.forward_ring(&step.outer_ring());
    if ring.len() < 3 {
        return;
    }
    for (c, p) in positions.iter_mut() {
        if stragglers.contains(c) {
            continue;
        }
        let q = proj.forward(*p);
        if !point_in_ring(q, &ring) {
            continue;
        }
        let (d, away) = nearest_edge(q, &ring);
        let need = warn_m + CLEAR_OF_BAND_M - d;
        if need <= 0.0 {
            continue;
        }
        // Along the sweep: the shortest walk ahead that clears the band.
        if let Some(Frame::Axis { axis }) = frame {
            let ahead = (1..=4 * WALK_AHEAD_M as usize)
                .map(|k| k as f64 * 0.25)
                .map(|t| [q[0] + axis[0] * t, q[1] + axis[1] * t])
                .find(|r| point_in_ring(*r, &ring) && nearest_edge(*r, &ring).0 >= warn_m + CLEAR_OF_BAND_M);
            if let Some(r) = ahead {
                *p = proj.inverse(r);
                continue;
            }
        }
        let moved = [q[0] + away[0] * need, q[1] + away[1] * need];
        if point_in_ring(moved, &ring) {
            *p = proj.inverse(moved);
        }
    }
}

/// Distance to the nearest edge of a ring and the unit direction from that edge to `q`.
fn nearest_edge(q: P, ring: &[P]) -> (f64, P) {
    let n = ring.len();
    let mut best = (f64::INFINITY, [0.0, 0.0]);
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        let ab = [b[0] - a[0], b[1] - a[1]];
        let len2 = ab[0] * ab[0] + ab[1] * ab[1];
        let s = if len2 > 0.0 { (((q[0] - a[0]) * ab[0] + (q[1] - a[1]) * ab[1]) / len2).clamp(0.0, 1.0) } else { 0.0 };
        let c = [a[0] + ab[0] * s, a[1] + ab[1] * s];
        let v = [q[0] - c[0], q[1] - c[1]];
        let d = v[0].hypot(v[1]);
        if d < best.0 {
            let dir = if d > 1e-9 {
                [v[0] / d, v[1] / d]
            } else {
                // On the edge: its inward normal, whichever way the ring winds.
                let l = len2.sqrt().max(1e-12);
                let left = [-ab[1] / l, ab[0] / l];
                if point_in_ring([q[0] + left[0] * 0.01, q[1] + left[1] * 0.01], ring) { left } else { [-left[0], -left[1]] }
            };
            best = (d, dir);
        }
    }
    best
}

/// The back line of a step: across the step at `level` along the sweep's
/// axis (the longest piece inside it, joining pieces less than
/// [`BACK_LINE_JOIN_M`] apart: a step's back edge follows the animals, so the
/// line leaves it at the notch behind each), or the step's edge when
/// gathering.
fn back_line(proj: &Projection, step: &Polygon, frame: Frame, level: f64) -> Option<Vec<LonLat>> {
    let ring: Vec<P> = proj.forward_ring(&step.outer_ring());
    if ring.len() < 3 {
        return None;
    }
    let axis = match frame {
        Frame::Axis { axis } => axis,
        Frame::Gather => {
            let mut edge: Vec<LonLat> = step.outer_ring();
            edge.push(edge[0]);
            return Some(edge);
        }
    };
    let across = [-axis[1], axis[0]];
    let dot = |p: P, d: P| p[0] * d[0] + p[1] * d[1];
    let n = ring.len();
    let mut hits: Vec<f64> = Vec::new();
    for i in 0..n {
        let (a, b) = (ring[i], ring[(i + 1) % n]);
        let (fa, fb) = (dot(a, axis) - level, dot(b, axis) - level);
        if (fa < 0.0) != (fb < 0.0) {
            let k = fa / (fa - fb);
            hits.push(dot([a[0] + (b[0] - a[0]) * k, a[1] + (b[1] - a[1]) * k], across));
        }
    }
    hits.sort_by(f64::total_cmp);
    let mut pieces: Vec<(f64, f64)> = Vec::new();
    for w in hits.chunks_exact(2) {
        match pieces.last_mut() {
            Some(last) if w[0] - last.1 < BACK_LINE_JOIN_M => last.1 = w[1],
            _ => pieces.push((w[0], w[1])),
        }
    }
    let (s0, s1) = pieces.into_iter().max_by(|x, y| (x.1 - x.0).total_cmp(&(y.1 - y.0)))?;
    let at = |s: f64| {
        let p = proj.inverse([axis[0] * level + across[0] * s, axis[1] * level + across[1] * s]);
        [op_geo::projection::round7(p[0]), op_geo::projection::round7(p[1])]
    };
    Some(vec![at(s0), at(s1)])
}

/// A sweep's pace, metres of back line a minute: this herd's own over its
/// last finished sweeps (five at most, of five steps or more, that recorded
/// their length), else [`default_pace`] for `head` animals at the fast
/// intervals of `collars.config`.
pub async fn pace_m_per_min(ctx: &Ctx, herd_id: &str, head: usize) -> anyhow::Result<f64> {
    let rows =
        sqlx::query("SELECT started_at, updated_at, sweep FROM moves WHERE herd_id = ? AND status = 'done' AND step >= 5 ORDER BY started_at DESC LIMIT 5")
            .bind(herd_id)
            .fetch_all(ctx.db())
            .await?;
    let mut paces: Vec<f64> = Vec::new();
    for r in &rows {
        let (from, to) = (time::from_db(&r.try_get::<String, _>("started_at")?)?, time::from_db(&r.try_get::<String, _>("updated_at")?)?);
        let sweep: Sweep = serde_json::from_str(&r.try_get::<String, _>("sweep")?).unwrap_or_default();
        let minutes = (to - from).num_milliseconds() as f64 / 60_000.0;
        if let Some(m) = sweep.start_m.filter(|m| *m > 0.0 && minutes > 0.0) {
            paces.push(m / minutes);
        }
    }
    if !paces.is_empty() {
        paces.sort_by(f64::total_cmp);
        return Ok(paces[paces.len() / 2]);
    }
    let c = op_ingest::config::load(ctx).await?;
    Ok(default_pace(head, c.fast_poll_s, c.fast_report_s))
}

/// Pace of a sweep with no history, metres a minute: measured with
/// collar-sim at the default fast intervals (10 s), `13.1 × head^-0.23`
/// (about 7.4 for 12 head, 4.9 for 72, 3.7 for 250); slower fast intervals
/// slow each step's round (a step, then the poll that fetches it and the
/// report that shows the walk) in proportion.
pub fn default_pace(head: usize, fast_poll_s: u32, fast_report_s: u32) -> f64 {
    let round = moves::STEP_EVERY.num_seconds() as f64 + (fast_poll_s + fast_report_s) as f64 / 2.0;
    let default_round = moves::STEP_EVERY.num_seconds() as f64 + 10.0;
    PACE_AT_ONE * (head.max(1) as f64).powf(-PACE_HEAD_EXPONENT) * default_round / round
}

/// [`default_pace`]'s fit: metres a minute for one animal, and how it falls with the head count.
pub const PACE_AT_ONE: f64 = 13.1;
pub const PACE_HEAD_EXPONENT: f64 = 0.23;

/// MCP `check_boundary` (read).
pub fn tool() -> ToolSpec {
    ToolSpec {
        name: "check_boundary",
        description: "Check a boundary for a herd before sending it, without sending anything: the shape as the collars would get it (exclusion holes cut in, fitted to the collars), the ring old collars without holes enforce, facts (area in hectares, head, square metres per head, forage kg DM and grazing days, rest days, corners, holes, sweep minutes) and findings: water inside or none, short forage, tight area per head, exclusions kept out, hazards taken in, roads, neighbour lines or the farm boundary crossed, weak GPS, collars offline or unable to hold holes, short rest, animals left in new holes or outside, simplified, no room for a staged boundary. With sweep, the back lines and minutes of the sweep that walks the herd in.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "herd_id": { "type": "string", "description": "Herd id. Optional when the farm has one herd." },
                "geometry": { "type": "object", "description": "GeoJSON Polygon, [longitude, latitude]; inner rings are holes." },
                "warn_m": { "type": "number", "minimum": 0, "maximum": 1000, "description": "Warning distance inside the line, metres (5 when absent)." },
                "effective_at": { "type": "string", "description": "RFC 3339 time it would take effect (staged); now when absent." },
                "sweep": { "type": "boolean", "description": "Preview the sweep that walks the herd in." }
            },
            "required": ["geometry"],
            "additionalProperties": false,
        }),
        read: true,
        brain: false,
        min_role: Role::Viewer,
        run: ToolSpec::run_fn(|c: ToolCall| async move {
            let herd = herd_of(&c.ctx, c.args.get("herd_id").and_then(Value::as_str)).await?;
            let mut args = c.args.clone();
            if let Some(o) = args.as_object_mut() {
                o.remove("herd_id");
            }
            let req: CheckRequest = serde_json::from_value(args).map_err(|e| ApiError::bad_request(format!("geometry must be a GeoJSON Polygon: {e}")))?;
            Ok(serde_json::to_value(check(&c.ctx, &herd.id, &req).await?).map_err(anyhow::Error::from)?)
        }),
    }
}

async fn herd_of(ctx: &Ctx, id: Option<&str>) -> ApiResult<Herd> {
    if let Some(id) = id.map(str::trim).filter(|s| !s.is_empty()) {
        return ctx.store().get_herd(id).await?.ok_or_else(|| ApiError::not_found(format!("No herd {id}.")));
    }
    let mut herds = ctx.store().list_herds().await?;
    match herds.len() {
        1 => Ok(herds.remove(0)),
        0 => Err(ApiError::not_found("The farm has no herds yet.")),
        n => Err(ApiError::bad_request(format!("The farm has {n} herds; pass herd_id."))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const O: LonLat = [-93.6225, 42.0318];

    fn at(x: f64, y: f64) -> LonLat {
        Projection::new(O).inverse([x, y])
    }
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Polygon {
        Polygon::from_ring(vec![at(x0, y0), at(x1, y0), at(x1, y1), at(x0, y1)])
    }

    #[test]
    fn a_herd_already_inside_has_no_sweep() {
        let target = rect(0.0, 0.0, 200.0, 200.0);
        let sit = Situation { active: Some(rect(-50.0, -50.0, 250.0, 250.0)), positions: vec![("c1".into(), at(100.0, 100.0))], ..Default::default() };
        assert_eq!(simulate(&target, 5.0, sit, time::now()), None);
        assert_eq!(simulate(&target, 5.0, Situation::default(), time::now()), None, "no positions");
    }

    #[test]
    fn a_sweep_steps_toward_the_target_about_three_metres_a_step() {
        // 12 animals in the west of a 400 m paddock, target its east end.
        let paddock = rect(0.0, 0.0, 400.0, 150.0);
        let target = rect(300.0, 0.0, 400.0, 150.0);
        let positions = (0..12).map(|i| (format!("c{i}"), at(20.0 + 5.0 * (i % 4) as f64, 30.0 + 25.0 * (i / 4) as f64))).collect();
        let sit = Situation { active: Some(paddock), positions, ..Default::default() };
        let s = simulate(&target, 5.0, sit, time::now()).expect("a sweep");
        // From the rearmost animal (x 20, back line 3 m behind it) to the target's rear edge (x 300).
        let per_step = (300.0 - 17.0) / (s.count - 1) as f64;
        assert!((2.5..=4.0).contains(&per_step), "{per_step} m a step over {} steps", s.count);
        assert!(s.back_lines.len() >= 20, "about every 10 m: {}", s.back_lines.len());
        // Back lines run across the sweep (north-south) and advance east.
        let x = |l: &Vec<LonLat>| Projection::new(O).forward(l[0])[0];
        assert!(s.back_lines.windows(2).all(|w| x(&w[1]) > x(&w[0])));
        let l = &s.back_lines[0];
        let (a, b) = (Projection::new(O).forward(l[0]), Projection::new(O).forward(*l.last().unwrap()));
        // Across the herd, whose buffered hull is the step.
        assert!((a[1] - b[1]).abs() > 3.0 * (a[0] - b[0]).abs() && (a[1] - b[1]).abs() > 20.0, "{a:?} {b:?}");
    }

    /// A herd of 250 bunched in a 50 m blob: the preview still finishes,
    /// though the tenth of the herd at its back hardly moves each step.
    #[test]
    fn a_herd_of_250_gets_a_preview() {
        let paddock = rect(0.0, 0.0, 413.0, 400.0);
        let target = rect(270.0, 0.0, 413.0, 400.0);
        let mut seed: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            (seed >> 11) as f64 / (1u64 << 53) as f64
        };
        let positions = (0..250).map(|i| (format!("c{i}"), at(185.0 + 50.0 * next(), 172.0 + 57.0 * next()))).collect();
        let sit = Situation { active: Some(paddock), positions, ..Default::default() };
        let s = simulate(&target, 5.0, sit, time::now()).expect("a sweep");
        assert!((60.0..=90.0).contains(&s.start_m), "the back line starts {} m from the target", s.start_m);
        assert!(s.back_lines.len() >= 6, "{}", s.back_lines.len());
    }

    #[test]
    fn nearest_edge_points_inside() {
        let ring: Vec<P> = vec![[0.0, 0.0], [10.0, 0.0], [10.0, 10.0], [0.0, 10.0]];
        let (d, dir) = nearest_edge([2.0, 5.0], &ring);
        assert!((d - 2.0).abs() < 1e-9 && (dir[0] - 1.0).abs() < 1e-9);
        let (d, dir) = nearest_edge([5.0, 0.0], &ring);
        assert!(d < 1e-9 && (dir[1] - 1.0).abs() < 1e-9, "{dir:?}");
    }
}
