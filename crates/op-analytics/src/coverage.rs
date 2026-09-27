//! Coverage maps (field-ready G): how good GNSS is, and how many fixes
//! arrive, across the farm. Read only from `coverage_days` (see
//! [`crate::days`]), never from `fixes`.
//!
//! `GET /api/coverage?metric=accuracy|fixes&from=&to=&cell_m=10&herd_id=`
//! → `{metric, cell_m, unit, size, cells: [[lon, lat, value, n]]}`: each cell
//! at its centre, `size` the degrees of longitude and latitude a cell spans.
//! - `accuracy`: the median accuracy of the fixes in the cell, metres, from
//!   the histogram; `n` is the fixes with an accuracy.
//! - `fixes`: fixes that arrived ÷ fixes the collars' cadence called for
//!   (a missed fix counts where the animal was before the gap); `n` is the
//!   fixes called for.
//! - `fix_rate` (H): fixes the receivers got ÷ fixes they tried, from the
//!   collars' health reports; `n` is the attempts.
//! - `cell` (H): the median cell signal (LTE RSRP) the collars measured, dBm;
//!   `n` is the reports that measured it. Only boards with a modem report one.
//!
//! Cells with `n` under 5 are left out. `cell_m` is a multiple of 10: coarser
//! cells are blocks of 10 m ones.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Query, State};
use axum::routing::get;
use chrono::{Duration, NaiveDate};
use op_core::tools::{ToolCall, ToolSpec};
use op_core::{ApiError, ApiResult, Ctx, Role};
use serde::ser::SerializeTuple;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::days::{ACC_BINS, ACC_EDGES_M, CellAgg, Grid};
use crate::range::{TimeRange, date_of};

/// Fewest samples a cell needs to be shown.
pub const MIN_N: u64 = 5;
/// Largest cell a map may ask for, metres.
pub const MAX_CELL_M: u32 = 1000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Metric {
    Accuracy,
    Fixes,
    // @H
    #[serde(rename = "fix_rate")]
    FixRate,
    Cell,
}

impl Metric {
    pub fn parse(s: &str) -> Result<Self, ApiError> {
        match s.trim() {
            "" | "accuracy" => Ok(Self::Accuracy),
            "fixes" => Ok(Self::Fixes),
            "fix_rate" => Ok(Self::FixRate),
            "cell" => Ok(Self::Cell),
            _ => Err(ApiError::bad_request("`metric` must be accuracy, fixes, fix_rate or cell.")),
        }
    }

    /// SI unit of the values: metres, or a share from 0 to 1.
    pub fn unit(self) -> &'static str {
        match self {
            Self::Accuracy => "m",
            Self::Fixes | Self::FixRate => "ratio",
            Self::Cell => "dBm",
        }
    }

    /// The value a cell shows and the samples behind it, or `None` when it
    /// has too few to show.
    pub fn value(self, a: &CellAgg) -> Option<(f64, u64)> {
        match self {
            Self::Accuracy => (a.n >= MIN_N).then(|| hist_median(&a.hist)).flatten().map(|v| (round(v, 2), a.n)),
            Self::Fixes => (a.expected >= MIN_N).then(|| (round((a.got as f64 / a.expected as f64).min(1.0), 3), a.expected)),
            Self::FixRate => (a.fix_attempts >= MIN_N).then(|| (round((a.fix_ok as f64 / a.fix_attempts as f64).min(1.0), 3), a.fix_attempts)),
            Self::Cell => {
                let n: u64 = a.rsrp.values().sum();
                (n >= MIN_N).then(|| rsrp_median(&a.rsrp)).flatten().map(|v| (round(v, 1), n))
            }
        }
    }
}

/// Median of whole-dBm readings (the mean of the two middle ones for an even count).
pub fn rsrp_median(hist: &std::collections::BTreeMap<i32, u64>) -> Option<f64> {
    let n: u64 = hist.values().sum();
    if n == 0 {
        return None;
    }
    let at = |k: u64| {
        let mut seen = 0;
        for (d, c) in hist {
            seen += c;
            if seen > k {
                return *d as f64;
            }
        }
        f64::NAN
    };
    Some(if n % 2 == 1 { at(n / 2) } else { (at(n / 2 - 1) + at(n / 2)) / 2.0 })
}

/// Median of an accuracy histogram, interpolated inside its bucket; 20 m
/// when it falls in the open top bucket.
pub fn hist_median(hist: &[u64; ACC_BINS]) -> Option<f64> {
    let n: u64 = hist.iter().sum();
    if n == 0 {
        return None;
    }
    let half = n as f64 / 2.0;
    let mut below = 0u64;
    for (i, h) in hist.iter().enumerate() {
        if *h > 0 && (below + h) as f64 >= half {
            let lo = if i == 0 { 0.0 } else { ACC_EDGES_M[i - 1] };
            let Some(hi) = ACC_EDGES_M.get(i) else { return Some(lo) };
            return Some(lo + (hi - lo) * (half - below as f64) / *h as f64);
        }
        below += h;
    }
    None
}

fn round(v: f64, places: i32) -> f64 {
    let m = 10f64.powi(places);
    (v * m).round() / m
}

/// One cell of a coverage map: its centre, value, samples and box. Sent as
/// `[lon, lat, value, n]`.
#[derive(Debug, Clone, PartialEq)]
pub struct Cell {
    pub lon: f64,
    pub lat: f64,
    pub value: f64,
    pub n: u64,
    /// `[west, south, east, north]`.
    pub bbox: [f64; 4],
}

impl Serialize for Cell {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let mut t = s.serialize_tuple(4)?;
        t.serialize_element(&self.lon)?;
        t.serialize_element(&self.lat)?;
        t.serialize_element(&self.value)?;
        t.serialize_element(&self.n)?;
        t.end()
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Coverage {
    pub metric: Metric,
    pub cell_m: u32,
    pub unit: &'static str,
    /// Degrees of longitude and latitude one cell spans (absent before any
    /// fix has been aggregated).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<[f64; 2]>,
    pub cells: Vec<Cell>,
}

/// What a map asks for.
#[derive(Debug, Clone)]
pub struct Ask {
    pub metric: Metric,
    /// UTC dates, both included.
    pub from: NaiveDate,
    pub to: NaiveDate,
    pub cell_m: u32,
    pub herd_id: Option<String>,
    pub bbox: Option<[f64; 4]>,
}

impl Ask {
    /// The dates a time range touches.
    pub fn dates(range: &TimeRange) -> (NaiveDate, NaiveDate) {
        (date_of(range.from_ms()), date_of(range.to_ms() - 1))
    }
}

/// A coverage map from the day tables.
pub async fn coverage(ctx: &Ctx, ask: &Ask) -> anyhow::Result<Coverage> {
    let k = (ask.cell_m / 10).max(1) as i64;
    let Some(grid) = crate::days::grid(ctx).await? else {
        return Ok(Coverage { metric: ask.metric, cell_m: ask.cell_m, unit: ask.metric.unit(), size: None, cells: Vec::new() });
    };
    let blocks = blocks(ctx, &grid, ask, k).await?;
    let mut cells: Vec<Cell> = blocks
        .into_iter()
        .filter_map(|((bx, by), a)| {
            let (value, n) = ask.metric.value(&a)?;
            let c = grid.block_center(bx, by, k);
            Some(Cell { lon: c[0], lat: c[1], value, n, bbox: grid.block_bbox(bx, by, k) })
        })
        .collect();
    cells.sort_by(|a, b| (a.lat, a.lon).partial_cmp(&(b.lat, b.lon)).unwrap_or(std::cmp::Ordering::Equal));
    Ok(Coverage { metric: ask.metric, cell_m: ask.cell_m, unit: ask.metric.unit(), size: Some(grid.block_size_deg(k)), cells })
}

async fn blocks(ctx: &Ctx, grid: &Grid, ask: &Ask, k: i64) -> anyhow::Result<HashMap<(i64, i64), CellAgg>> {
    let mut sql = String::from("SELECT cx, cy, n, acc_hist, expected, got, fix_attempts, fix_ok, rsrp_hist FROM coverage_days WHERE date >= ? AND date <= ?");
    if ask.herd_id.is_some() {
        sql.push_str(" AND herd_id = ?");
    }
    let bounds = ask.bbox.map(|b| grid.cells_in(b));
    if bounds.is_some() {
        sql.push_str(" AND cx >= ? AND cx <= ? AND cy >= ? AND cy <= ?");
    }
    type Row = (i64, i64, i64, String, i64, i64, i64, i64, Option<String>);
    let mut q = sqlx::query_as::<_, Row>(&sql).bind(ask.from.format("%Y-%m-%d").to_string()).bind(ask.to.format("%Y-%m-%d").to_string());
    if let Some(h) = &ask.herd_id {
        q = q.bind(h);
    }
    if let Some((x0, y0, x1, y1)) = bounds {
        q = q.bind(x0).bind(x1).bind(y0).bind(y1);
    }
    let mut out: HashMap<(i64, i64), CellAgg> = HashMap::new();
    for (cx, cy, n, hist, expected, got, fix_attempts, fix_ok, rsrp) in q.fetch_all(ctx.db()).await? {
        let hist: [u64; ACC_BINS] = serde_json::from_str(&hist).unwrap_or_default();
        let a = CellAgg {
            n: n.max(0) as u64,
            hist,
            expected: expected.max(0) as u64,
            got: got.max(0) as u64,
            fix_attempts: fix_attempts.max(0) as u64,
            fix_ok: fix_ok.max(0) as u64,
            rsrp: CellAgg::rsrp_from_json(rsrp.as_deref()),
        };
        out.entry((cx.div_euclid(k), cy.div_euclid(k))).or_default().merge(&a);
    }
    Ok(out)
}

/// 10 m cells inside `[west, south, east, north]` over a time range, for the
/// pre-send check (F): the same tables and rules as `/api/coverage`.
pub async fn grid(ctx: &Ctx, bbox: [f64; 4], range: TimeRange, metric: Metric) -> anyhow::Result<Vec<Cell>> {
    let (from, to) = Ask::dates(&range);
    Ok(coverage(ctx, &Ask { metric, from, to, cell_m: 10, herd_id: None, bbox: Some(bbox) }).await?.cells)
}

// ---------------------------------------------------------------- HTTP

#[derive(Debug, Default, Deserialize)]
pub struct Params {
    pub metric: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub cell_m: Option<String>,
    pub herd_id: Option<String>,
}

impl Params {
    fn ask(&self) -> Result<Ask, ApiError> {
        let metric = Metric::parse(self.metric.as_deref().unwrap_or(""))?;
        let range = TimeRange::parse(self.from.as_deref(), self.to.as_deref(), Duration::days(7), op_core::time::now())?;
        let (from, to) = Ask::dates(&range);
        let cell_m = match self.cell_m.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            None => 10,
            Some(s) => s
                .parse::<u32>()
                .ok()
                .filter(|m| *m >= 10 && *m <= MAX_CELL_M && m % 10 == 0)
                .ok_or_else(|| ApiError::bad_request(format!("`cell_m` must be a multiple of 10 m from 10 to {MAX_CELL_M}.")))?,
        };
        let herd_id = self.herd_id.clone().filter(|h| !h.trim().is_empty());
        Ok(Ask { metric, from, to, cell_m, herd_id, bbox: None })
    }
}

pub fn router() -> axum::Router<Ctx> {
    axum::Router::new().route("/api/coverage", get(get_coverage))
}

async fn get_coverage(State(ctx): State<Ctx>, Query(p): Query<Params>) -> ApiResult<Json<Coverage>> {
    Ok(Json(coverage(&ctx, &p.ask()?).await?))
}

// ---------------------------------------------------------------- MCP

/// Most cells the tool returns; beyond that, the weakest ones.
const TOOL_MAX_CELLS: usize = 500;

pub fn tool() -> ToolSpec {
    ToolSpec {
        name: "get_coverage",
        description: "GNSS coverage across the farm from the collars' fixes, as square cells (default 10 m; cell_m a multiple of 10). metric accuracy: each cell's median fix accuracy in metres (lower is better; above about 5 m is weak). metric fixes: the share of expected fixes that arrived, 0 to 1 (a missed fix counts where the animal was before the gap; below about 0.9 is weak). metric fix_rate: the share of fix attempts the receivers got, 0 to 1, from the collars' health reports (below about 0.9 is weak). metric cell: the median LTE cell signal (RSRP) the collars measured, dBm (higher is better; below about -110 is weak; only collars with a modem report it). Each cell is [longitude, latitude, value, samples] at its centre; cells with under 5 samples are left out. from/to are dates or times (RFC 3339) or relative (-7d); default the last 7 days. With more than 500 cells only the 500 weakest are returned and `truncated` says how many there were.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "metric": { "type": "string", "enum": ["accuracy", "fixes", "fix_rate", "cell"] },
                "from": { "type": "string", "description": "Start: RFC 3339, a date, or relative like -7d." },
                "to": { "type": "string", "description": "End (exclusive): RFC 3339, a date, or now." },
                "cell_m": { "type": "integer", "minimum": 10, "maximum": MAX_CELL_M, "multipleOf": 10 },
                "herd_id": { "type": "string", "description": "Only this herd's fixes." }
            },
            "required": [],
            "additionalProperties": false
        }),
        read: true,
        brain: false,
        min_role: Role::Viewer,
        run: ToolSpec::run_fn(|c: ToolCall| async move { tool_run(&c.ctx, &c.args).await }),
    }
}

async fn tool_run(ctx: &Ctx, args: &Value) -> ApiResult<Value> {
    let s = |k: &str| args.get(k).and_then(|v| v.as_str().map(str::to_owned).or_else(|| v.as_u64().map(|n| n.to_string())));
    let p = Params { metric: s("metric"), from: s("from"), to: s("to"), cell_m: s("cell_m"), herd_id: s("herd_id") };
    let ask = p.ask()?;
    let mut map = coverage(ctx, &ask).await?;
    let total = map.cells.len();
    if total > TOOL_MAX_CELLS {
        // Weakest first: worst accuracy, fewest fixes.
        match ask.metric {
            Metric::Accuracy => map.cells.sort_by(|a, b| b.value.total_cmp(&a.value)),
            Metric::Fixes | Metric::FixRate | Metric::Cell => map.cells.sort_by(|a, b| a.value.total_cmp(&b.value)),
        }
        map.cells.truncate(TOOL_MAX_CELLS);
    }
    let mut out = serde_json::to_value(&map).map_err(anyhow::Error::from)?;
    if total > TOOL_MAX_CELLS {
        out["truncated"] = json!(total);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_of_a_histogram() {
        assert_eq!(hist_median(&[0; 8]), None);
        // All ten between 2 and 3 m: the middle of that bucket.
        assert_eq!(hist_median(&[0, 0, 10, 0, 0, 0, 0, 0]), Some(2.5));
        // Half under 1 m, half 3-5 m: the median sits at the top of the first.
        assert_eq!(hist_median(&[5, 0, 0, 5, 0, 0, 0, 0]), Some(1.0));
        assert_eq!(hist_median(&[1, 0, 0, 0, 0, 0, 0, 9]), Some(20.0));
        let v = hist_median(&[2, 2, 2, 2, 0, 0, 0, 0]).unwrap();
        assert!((v - 2.0).abs() < 1e-12, "{v}");
    }

    #[test]
    fn values_and_the_sample_floor() {
        let a = CellAgg { n: 4, hist: [0, 0, 4, 0, 0, 0, 0, 0], expected: 40, got: 4, ..Default::default() };
        assert_eq!(Metric::Accuracy.value(&a), None);
        assert_eq!(Metric::Fixes.value(&a), Some((0.1, 40)));
        let b = CellAgg { n: 5, hist: [0, 0, 5, 0, 0, 0, 0, 0], expected: 5, got: 5, ..Default::default() };
        assert_eq!(Metric::Accuracy.value(&b), Some((2.5, 5)));
        assert_eq!(Metric::Fixes.value(&b), Some((1.0, 5)));
    }

    #[test]
    fn metric_words() {
        assert_eq!(Metric::parse("").unwrap(), Metric::Accuracy);
        assert_eq!(Metric::parse("fixes").unwrap(), Metric::Fixes);
        assert!(Metric::parse("signal").is_err());
        assert_eq!(serde_json::to_value(Metric::Fixes).unwrap(), json!("fixes"));
        let c = Cell { lon: 1.5, lat: 2.5, value: 3.0, n: 7, bbox: [0.0; 4] };
        assert_eq!(serde_json::to_value(c).unwrap(), json!([1.5, 2.5, 3.0, 7]));
    }

    #[test]
    fn fix_rate_and_cell_signal_values() {
        assert_eq!(Metric::parse("fix_rate").unwrap(), Metric::FixRate);
        assert_eq!(serde_json::to_value(Metric::FixRate).unwrap(), json!("fix_rate"));
        assert_eq!((Metric::Cell.unit(), Metric::FixRate.unit()), ("dBm", "ratio"));
        let rsrp: std::collections::BTreeMap<i32, u64> = [(-110, 2), (-104, 1), (-99, 2)].into_iter().collect();
        let a = CellAgg { fix_attempts: 20, fix_ok: 17, rsrp, ..Default::default() };
        assert_eq!(Metric::FixRate.value(&a), Some((0.85, 20)));
        assert_eq!(Metric::Cell.value(&a), Some((-104.0, 5)));
        assert_eq!(rsrp_median(&[(-100, 1), (-90, 1)].into_iter().collect()), Some(-95.0));
        // Too few, and none.
        let few = CellAgg { fix_attempts: 4, fix_ok: 4, rsrp: [(-90, 4)].into_iter().collect(), ..Default::default() };
        assert_eq!((Metric::FixRate.value(&few), Metric::Cell.value(&few)), (None, None));
        assert_eq!(CellAgg::rsrp_from_json(a.rsrp_json().as_deref()), a.rsrp);
        assert_eq!(CellAgg::default().rsrp_json(), None);
    }
}
