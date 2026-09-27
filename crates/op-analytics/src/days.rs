//! Day tables for coverage and fleet care (field-ready G), kept by a
//! background task so `/api/coverage` and `/api/fleet` never read raw
//! telemetry.
//!
//! - `battery_days`: per collar per UTC day, the battery readings in
//!   op-ingest's `health` as min, max, mean and last.
//! - `coverage_days`: per herd per UTC day per 10 m cell, the fixes that
//!   landed there, their accuracy histogram, and how many fixes the collars'
//!   cadence called for. A fix that never came counts in the cell of the fix
//!   before the gap.
//!
//! A run redoes exactly the days that changed: days holding rows added since
//! the last run (by row id), and days whose Parquet file is new or gained
//! rows. Each day is read from SQLite and, once rolled up, from its day file,
//! inside one read snapshot, so where a row lives never matters and a rollup
//! running at the same time can't hide rows. The first run covers every day
//! there is data for, Parquet included (the backfill). Today is redone as it
//! fills, so it is always the partial day so far.

use std::collections::{BTreeSet, HashMap};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Days, NaiveDate, Utc};
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use futures::TryStreamExt;
use op_core::Ctx;
use op_geo::{LonLat, Projection};
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::file::statistics::Statistics;
use sqlx::{Row, SqliteConnection};

use crate::metrics::Cadence;
use crate::range::day_start_ms;
use crate::schema::{Cols, get_f64, get_i64, get_str, normalize};
use crate::telemetry::{BATCH_ROWS, day_path, list_days};

/// Side of a coverage cell, metres. Coarser cells are blocks of these.
pub const CELL_M: f64 = 10.0;
/// Upper edges of the accuracy histogram's buckets, metres; the eighth
/// bucket holds everything from 20 m up.
pub const ACC_EDGES_M: [f64; 7] = [1.0, 2.0, 3.0, 5.0, 8.0, 12.0, 20.0];
pub const ACC_BINS: usize = 8;
/// A gap between two fixes longer than this means the collar was off
/// (parked, on the charger, flat), not that fixes went missing.
pub const MAX_GAP_MS: i64 = 6 * 3_600_000;
const DAY_MS: i64 = 86_400_000;

/// How often the task runs: every 10 minutes, less often when a run is
/// expensive (it never takes more than a twentieth of the time), at most
/// hourly.
const EVERY: Duration = Duration::from_secs(600);
const AT_MOST: Duration = Duration::from_secs(3600);
const FIRST_AFTER: Duration = Duration::from_secs(60);

// ---------------------------------------------------------------- grid

/// The coverage grid: 10 m cells counted east and north from a fixed origin
/// (`coverage_grid`), on the same local projection the rest of op-analytics
/// uses. The origin never moves, so a cell is the same ground on every day.
#[derive(Debug, Clone, Copy)]
pub struct Grid {
    proj: Projection,
}

impl Grid {
    pub fn new(origin: LonLat) -> Self {
        Self { proj: Projection::new(origin) }
    }

    pub fn origin(&self) -> LonLat {
        [self.proj.lon0, self.proj.lat0]
    }

    /// The 10 m cell holding a point.
    pub fn cell(&self, p: LonLat) -> (i64, i64) {
        let [x, y] = self.proj.forward(p);
        ((x / CELL_M).floor() as i64, (y / CELL_M).floor() as i64)
    }

    /// `[west, south, east, north]` of block `(bx, by)` of `k` × `k` cells.
    pub fn block_bbox(&self, bx: i64, by: i64, k: i64) -> [f64; 4] {
        let s = CELL_M * k as f64;
        let sw = self.proj.inverse([bx as f64 * s, by as f64 * s]);
        let ne = self.proj.inverse([(bx + 1) as f64 * s, (by + 1) as f64 * s]);
        [sw[0], sw[1], ne[0], ne[1]]
    }

    /// Centre of block `(bx, by)` of `k` × `k` cells.
    pub fn block_center(&self, bx: i64, by: i64, k: i64) -> LonLat {
        let s = CELL_M * k as f64;
        self.proj.inverse([(bx as f64 + 0.5) * s, (by as f64 + 0.5) * s])
    }

    /// Degrees of longitude and latitude a block of `k` × `k` cells spans.
    pub fn block_size_deg(&self, k: i64) -> [f64; 2] {
        let s = CELL_M * k as f64;
        [s / self.proj.m_per_deg_lon, self.proj.offset(0.0, s)[1] - self.proj.lat0]
    }

    /// The 10 m cells a `[west, south, east, north]` box touches, as
    /// `(cx_min, cy_min, cx_max, cy_max)`.
    pub fn cells_in(&self, bbox: [f64; 4]) -> (i64, i64, i64, i64) {
        let (x0, y0) = self.cell([bbox[0], bbox[1]]);
        let (x1, y1) = self.cell([bbox[2], bbox[3]]);
        (x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1))
    }
}

/// The stored grid, if any fix has been aggregated yet.
pub async fn grid(ctx: &Ctx) -> anyhow::Result<Option<Grid>> {
    let row: Option<(f64, f64)> = sqlx::query_as("SELECT lon0, lat0 FROM coverage_grid WHERE id = 1").fetch_optional(ctx.db()).await?;
    Ok(row.map(|(lon, lat)| Grid::new([lon, lat])))
}

/// The grid, set on first use from the farm's centre (else the first fix),
/// rounded to 0.01°.
async fn ensure_grid(ctx: &Ctx, fallback: Option<LonLat>) -> anyhow::Result<Option<Grid>> {
    if let Some(g) = grid(ctx).await? {
        return Ok(Some(g));
    }
    let Some(o) = ctx.store().get_farm().await?.map(|f| f.center).or(fallback) else {
        return Ok(None);
    };
    let o = [(o[0] * 100.0).round() / 100.0, (o[1] * 100.0).round() / 100.0];
    sqlx::query("INSERT OR IGNORE INTO coverage_grid (id, lon0, lat0, created_at) VALUES (1, ?, ?, ?)")
        .bind(o[0])
        .bind(o[1])
        .bind(op_core::time::to_db(&op_core::time::now()))
        .execute(ctx.db())
        .await?;
    grid(ctx).await
}

// ---------------------------------------------------------------- cells

/// What one cell holds for one herd on one day.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CellAgg {
    /// Fixes with an accuracy (the histogram's total).
    pub n: u64,
    pub hist: [u64; ACC_BINS],
    /// Fixes the collars' cadence called for: those that came plus those
    /// that didn't.
    pub expected: u64,
    /// Fixes that came.
    pub got: u64,
}

impl CellAgg {
    fn add_fix(&mut self, acc_m: f32) {
        self.got += 1;
        self.expected += 1;
        if acc_m.is_finite() && acc_m >= 0.0 {
            self.n += 1;
            self.hist[acc_bin(acc_m as f64)] += 1;
        }
    }

    pub fn merge(&mut self, o: &CellAgg) {
        self.n += o.n;
        self.got += o.got;
        self.expected += o.expected;
        for (a, b) in self.hist.iter_mut().zip(o.hist) {
            *a += b;
        }
    }
}

/// Histogram bucket of an accuracy in metres.
pub fn acc_bin(acc_m: f64) -> usize {
    ACC_EDGES_M.iter().position(|e| acc_m < *e).unwrap_or(ACC_BINS - 1)
}

/// A fix as the aggregator keeps it: time, cell, accuracy and herd (an index
/// into the day's [`Herds`]). Small, because a cold day is held whole.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Fx {
    t: i64,
    cx: i32,
    cy: i32,
    acc: f32,
    herd: u32,
}

type CellKey = (u32, i32, i32);

/// Herd ids of one day, interned.
#[derive(Debug, Default)]
struct Herds {
    ids: HashMap<String, u32>,
    names: Vec<String>,
}

impl Herds {
    fn get(&mut self, herd: Option<&str>) -> u32 {
        let h = herd.unwrap_or("");
        if let Some(i) = self.ids.get(h) {
            return *i;
        }
        let i = self.names.len() as u32;
        self.names.push(h.to_owned());
        self.ids.insert(h.to_owned(), i);
        i
    }
}

fn ceil_div(a: i64, b: i64) -> i64 {
    -((-a).div_euclid(b))
}

/// One collar's day. Every fix counts in the cell it landed in. Between two
/// fixes more than a cadence apart (the collar's median gap that day), each
/// fix that should have come counts as expected in the cell of the fix
/// before the gap, if its expected time falls inside the day. `prev` is the
/// collar's last fix before the day and `next_t` the time of its first fix
/// after it (both within [`MAX_GAP_MS`]), so gaps across midnight split
/// between the two days. Gaps over [`MAX_GAP_MS`] mean the collar was off.
fn collar_day(day: (i64, i64), prev: Option<Fx>, fixes: &mut Vec<Fx>, next_t: Option<i64>, cells: &mut HashMap<CellKey, CellAgg>) {
    fixes.sort_by_key(|f| f.t);
    // The same fix stored twice (a report sent again) counts once.
    fixes.dedup_by_key(|f| f.t);
    let mut cadence = Cadence::default();
    for f in fixes.iter() {
        cadence.push(f.t);
        cells.entry((f.herd, f.cx, f.cy)).or_default().add_fix(f.acc);
    }
    let Some(c_ms) = cadence.median_s().map(|s| (s * 1000.0) as i64).filter(|c| *c > 0) else {
        return;
    };
    let mut missed = |a: &Fx, b_t: i64| {
        let gap = b_t - a.t;
        if gap <= c_ms || gap > MAX_GAP_MS {
            return;
        }
        let m = (gap as f64 / c_ms as f64).round() as i64 - 1;
        if m <= 0 {
            return;
        }
        // Missed fix k (1..=m) was due at a.t + k·cadence; count those due today.
        let lo = ceil_div(day.0 - a.t, c_ms).max(1);
        let hi = (ceil_div(day.1 - a.t, c_ms) - 1).min(m);
        if hi >= lo {
            cells.entry((a.herd, a.cx, a.cy)).or_default().expected += (hi - lo + 1) as u64;
        }
    };
    let first = fixes.first().copied();
    if let (Some(p), Some(f)) = (prev, first) {
        if p.t < f.t {
            missed(&p, f.t);
        }
    }
    for w in fixes.windows(2) {
        missed(&w[0], w[1].t);
    }
    if let (Some(l), Some(n)) = (fixes.last(), next_t) {
        if n > l.t {
            missed(l, n);
        }
    }
}

// ---------------------------------------------------------------- day files

fn fix_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("collar_id", DataType::Utf8, true),
        Field::new("herd_id", DataType::Utf8, true),
        Field::new("t", DataType::Int64, true),
        Field::new("lon", DataType::Float64, true),
        Field::new("lat", DataType::Float64, true),
        Field::new("accuracy_m", DataType::Float64, true),
    ]))
}

fn battery_schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Int64, true),
        Field::new("collar_id", DataType::Utf8, true),
        Field::new("t", DataType::Int64, true),
        Field::new("battery", DataType::Float64, true),
    ]))
}

/// Read only `target`'s columns of a day file (those it has; the others
/// read as null), batch by batch.
fn read_day_file(path: &Path, target: &SchemaRef, mut f: impl FnMut(&RecordBatch)) -> anyhow::Result<()> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
    let have = builder.schema().clone();
    let roots: Vec<usize> = target.fields().iter().filter_map(|t| have.index_of(t.name()).ok()).collect();
    let mask = ProjectionMask::roots(builder.parquet_schema(), roots);
    for b in builder.with_projection(mask).with_batch_size(BATCH_ROWS).build()? {
        f(&normalize(&b?, target)?);
    }
    Ok(())
}

/// Highest row id in a day file, from its footer statistics when they are
/// there, else by reading the column.
fn file_max_id(path: &Path) -> anyhow::Result<i64> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
    if let Ok(idx) = builder.schema().index_of("id") {
        let mut max = i64::MIN;
        let mut all = true;
        for rg in builder.metadata().row_groups() {
            match rg.column(idx).statistics() {
                Some(Statistics::Int64(s)) if s.max_opt().is_some() => max = max.max(*s.max_opt().unwrap_or(&i64::MIN)),
                _ if rg.num_rows() == 0 => {}
                _ => all = false,
            }
        }
        if all {
            return Ok(max.max(0));
        }
    }
    let target = Arc::new(Schema::new(vec![Field::new("id", DataType::Int64, true)]));
    let mut max = 0;
    read_day_file(path, &target, |b| {
        let ids = Cols::new(b).i64("id");
        for i in 0..b.num_rows() {
            if let Some(id) = get_i64(ids, i) {
                max = max.max(id);
            }
        }
    })?;
    Ok(max)
}

fn mtime_ms(path: &Path) -> Option<i64> {
    let m = std::fs::metadata(path).ok()?.modified().ok()?;
    Some(DateTime::<Utc>::from(m).timestamp_millis())
}

/// A collar's fix at the edge of a neighbouring day.
#[derive(Debug, Clone)]
struct Edge {
    t: i64,
    point: LonLat,
    herd: Option<String>,
}

/// Per collar, the latest (`last`) or earliest fix in `[lo, hi)` of a day file.
fn file_edges(path: &Path, lo: i64, hi: i64, last: bool) -> anyhow::Result<HashMap<String, Edge>> {
    let mut out: HashMap<String, Edge> = HashMap::new();
    read_day_file(path, &fix_schema(), |b| {
        let c = Cols::new(b);
        let (collar, herd, t, lon, lat) = (c.str("collar_id"), c.str("herd_id"), c.i64("t"), c.f64("lon"), c.f64("lat"));
        for i in 0..b.num_rows() {
            let (Some(id), Some(t), Some(lon), Some(lat)) = (get_str(collar, i), get_i64(t, i), get_f64(lon, i), get_f64(lat, i)) else {
                continue;
            };
            if t < lo || t >= hi {
                continue;
            }
            let better = out.get(id).is_none_or(|e| if last { t > e.t } else { t < e.t });
            if better {
                out.insert(id.to_owned(), Edge { t, point: [lon, lat], herd: get_str(herd, i).map(str::to_owned) });
            }
        }
    })?;
    Ok(out)
}

/// A day file's fixes grouped by collar, and its highest row id.
fn file_fixes(path: &Path, grid: Grid, herds: &mut Herds) -> anyhow::Result<(HashMap<String, Vec<Fx>>, i64)> {
    let mut by: HashMap<String, Vec<Fx>> = HashMap::new();
    let mut max_id = 0;
    read_day_file(path, &fix_schema(), |b| {
        let c = Cols::new(b);
        let (ids, collar, herd, t, lon, lat, acc) =
            (c.i64("id"), c.str("collar_id"), c.str("herd_id"), c.i64("t"), c.f64("lon"), c.f64("lat"), c.f64("accuracy_m"));
        for i in 0..b.num_rows() {
            if let Some(id) = get_i64(ids, i) {
                max_id = max_id.max(id);
            }
            let (Some(cid), Some(t), Some(lon), Some(lat)) = (get_str(collar, i), get_i64(t, i), get_f64(lon, i), get_f64(lat, i)) else {
                continue;
            };
            let (cx, cy) = grid.cell([lon, lat]);
            let fx = Fx { t, cx: cx as i32, cy: cy as i32, acc: get_f64(acc, i).map_or(f32::NAN, |a| a as f32), herd: herds.get(get_str(herd, i)) };
            match by.get_mut(cid) {
                Some(v) => v.push(fx),
                None => {
                    by.insert(cid.to_owned(), vec![fx]);
                }
            }
        }
    })?;
    Ok((by, max_id))
}

/// A day file's battery readings grouped by collar, and its highest row id.
fn file_battery(path: &Path) -> anyhow::Result<(HashMap<String, Vec<(i64, f64)>>, i64)> {
    let mut by: HashMap<String, Vec<(i64, f64)>> = HashMap::new();
    let mut max_id = 0;
    read_day_file(path, &battery_schema(), |b| {
        let c = Cols::new(b);
        let (ids, collar, t, bat) = (c.i64("id"), c.str("collar_id"), c.i64("t"), c.f64("battery"));
        for i in 0..b.num_rows() {
            if let Some(id) = get_i64(ids, i) {
                max_id = max_id.max(id);
            }
            if let (Some(cid), Some(t), Some(v)) = (get_str(collar, i), get_i64(t, i), get_f64(bat, i)) {
                by.entry(cid.to_owned()).or_default().push((t, v));
            }
        }
    })?;
    Ok((by, max_id))
}

async fn blocking<T: Send + 'static>(f: impl FnOnce() -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
    tokio::task::spawn_blocking(f).await?
}

fn existing(path: PathBuf) -> Option<PathBuf> {
    path.is_file().then_some(path)
}

// ---------------------------------------------------------------- SQLite

/// Collars with rows in `table`, by a loose scan of its `(collar_id, t)`
/// index: one seek per collar, whatever the table holds.
async fn hot_collars(conn: &mut SqliteConnection, table: &str) -> anyhow::Result<Vec<String>> {
    let sql = format!(
        "WITH RECURSIVE c(id) AS (SELECT (SELECT MIN(collar_id) FROM {table}) \
         UNION ALL SELECT (SELECT MIN(collar_id) FROM {table} WHERE collar_id > c.id) FROM c WHERE c.id IS NOT NULL) \
         SELECT id FROM c WHERE id IS NOT NULL"
    );
    Ok(sqlx::query_scalar(&sql).fetch_all(conn).await?)
}

/// A read transaction whose snapshot is taken now (SQLite takes it at the
/// first read, not at BEGIN). Dropped without commit, it rolls back.
async fn snapshot(ctx: &Ctx, table: &str) -> anyhow::Result<sqlx::Transaction<'static, sqlx::Sqlite>> {
    let mut tx = ctx.db().begin().await?;
    sqlx::query(&format!("SELECT 1 FROM {table} LIMIT 1")).fetch_optional(&mut *tx).await?;
    Ok(tx)
}

/// The largest row id `table` has ever handed out (rows rolled to Parquet
/// included).
async fn top_id(ctx: &Ctx, table: &str) -> anyhow::Result<i64> {
    let seq: Option<i64> = sqlx::query_scalar("SELECT seq FROM sqlite_sequence WHERE name = ?").bind(table).fetch_optional(ctx.db()).await?;
    Ok(seq.unwrap_or(0))
}

fn day_of(n: i64) -> Option<NaiveDate> {
    NaiveDate::from_num_days_from_ce_opt(i32::try_from(n + 719_163).ok()?)
}

// ---------------------------------------------------------------- one day

/// Rewrite `coverage_days` for one date. Returns the number of cells.
async fn coverage_day(ctx: &Ctx, grid: Grid, date: NaiveDate) -> anyhow::Result<usize> {
    let (ds, de) = (day_start_ms(date), day_start_ms(date) + DAY_MS);
    let mut read = snapshot(ctx, "fixes").await?;
    let res = coverage_read(ctx, &mut read, grid, date, ds, de).await;
    read.rollback().await?;
    let (herds, cells, max_id, mtime) = res?;
    if cells.is_empty() && max_id == 0 && mtime.is_none() {
        // No fix that day (a neighbour of a day that changed).
        return Ok(0);
    }

    let d = date.format("%Y-%m-%d").to_string();
    let mut rows: Vec<(&str, i32, i32, &CellAgg)> = cells.iter().map(|((h, x, y), a)| (herds.names[*h as usize].as_str(), *x, *y, a)).collect();
    rows.sort_by(|a, b| (a.0, a.1, a.2).cmp(&(b.0, b.1, b.2)));
    let mut tx = ctx.db().begin().await?;
    sqlx::query("DELETE FROM coverage_days WHERE date = ?").bind(&d).execute(&mut *tx).await?;
    for chunk in rows.chunks(200) {
        let sql = format!(
            "INSERT INTO coverage_days (date, herd_id, cx, cy, n, acc_hist, expected, got) VALUES {}",
            vec!["(?, ?, ?, ?, ?, ?, ?, ?)"; chunk.len()].join(", ")
        );
        let mut q = sqlx::query(&sql);
        for (h, x, y, a) in chunk {
            q = q
                .bind(&d)
                .bind(*h)
                .bind(*x as i64)
                .bind(*y as i64)
                .bind(a.n as i64)
                .bind(serde_json::to_string(&a.hist)?)
                .bind(a.expected as i64)
                .bind(a.got as i64);
        }
        q.execute(&mut *tx).await?;
    }
    record_day(&mut *tx, "fixes", &d, max_id, mtime).await?;
    tx.commit().await?;
    Ok(rows.len())
}

type CoverageRead = (Herds, HashMap<CellKey, CellAgg>, i64, Option<i64>);

async fn coverage_read(ctx: &Ctx, conn: &mut SqliteConnection, grid: Grid, date: NaiveDate, ds: i64, de: i64) -> anyhow::Result<CoverageRead> {
    let dir = ctx.data_dir().to_path_buf();
    let file = existing(day_path(&dir, "fixes", date));
    let mtime = file.as_deref().and_then(mtime_ms);
    let mut herds = Herds::default();
    let (mut cold, cold_max) = match file.clone() {
        Some(p) => {
            let (by, max, h) = blocking(move || {
                let mut h = Herds::default();
                let (by, max) = file_fixes(&p, grid, &mut h)?;
                Ok((by, max, h))
            })
            .await?;
            herds = h;
            (by, max)
        }
        None => (HashMap::new(), 0),
    };
    let prev_file = date.checked_sub_days(Days::new(1)).and_then(|d| existing(day_path(&dir, "fixes", d)));
    let next_file = date.checked_add_days(Days::new(1)).and_then(|d| existing(day_path(&dir, "fixes", d)));
    let cold_prev = match prev_file {
        Some(p) => blocking(move || file_edges(&p, ds - MAX_GAP_MS, ds, true)).await?,
        None => HashMap::new(),
    };
    let cold_next = match next_file {
        Some(p) => blocking(move || file_edges(&p, de, de + MAX_GAP_MS, false)).await?,
        None => HashMap::new(),
    };

    let mut collars: BTreeSet<String> = cold.keys().cloned().collect();
    collars.extend(hot_collars(conn, "fixes").await?);
    let mut cells: HashMap<CellKey, CellAgg> = HashMap::new();
    let mut max_id = cold_max;
    for collar in &collars {
        let mut fixes = cold.remove(collar).unwrap_or_default();
        // Rows a rollup has written to the day file but not yet deleted are
        // the file's; newer ones are late arrivals.
        let mut rows = sqlx::query("SELECT id, herd_id, t, lon, lat, accuracy_m FROM fixes WHERE collar_id = ? AND t >= ? AND t < ? AND id > ? ORDER BY t")
            .bind(collar)
            .bind(ds)
            .bind(de)
            .bind(cold_max)
            .fetch(&mut *conn);
        while let Some(r) = rows.try_next().await? {
            let id: i64 = r.try_get(0)?;
            max_id = max_id.max(id);
            let herd: Option<String> = r.try_get(1)?;
            let (cx, cy) = grid.cell([r.try_get(3)?, r.try_get(4)?]);
            let acc: Option<f64> = r.try_get_unchecked(5).ok().flatten();
            fixes.push(Fx { t: r.try_get(2)?, cx: cx as i32, cy: cy as i32, acc: acc.map_or(f32::NAN, |a| a as f32), herd: herds.get(herd.as_deref()) });
        }
        drop(rows);
        if fixes.is_empty() {
            continue;
        }

        let hot_prev: Option<(i64, f64, f64, Option<String>)> =
            sqlx::query_as("SELECT t, lon, lat, herd_id FROM fixes WHERE collar_id = ? AND t >= ? AND t < ? ORDER BY t DESC LIMIT 1")
                .bind(collar)
                .bind(ds - MAX_GAP_MS)
                .bind(ds)
                .fetch_optional(&mut *conn)
                .await?;
        let mut prev = cold_prev.get(collar).cloned();
        if let Some((t, lon, lat, herd)) = hot_prev {
            if prev.as_ref().is_none_or(|p| t > p.t) {
                prev = Some(Edge { t, point: [lon, lat], herd });
            }
        }
        let prev = prev.map(|e| {
            let (cx, cy) = grid.cell(e.point);
            Fx { t: e.t, cx: cx as i32, cy: cy as i32, acc: f32::NAN, herd: herds.get(e.herd.as_deref()) }
        });
        let hot_next: Option<i64> = sqlx::query_scalar("SELECT MIN(t) FROM fixes WHERE collar_id = ? AND t >= ? AND t < ?")
            .bind(collar)
            .bind(de)
            .bind(de + MAX_GAP_MS)
            .fetch_one(&mut *conn)
            .await?;
        let next = match (hot_next, cold_next.get(collar).map(|e| e.t)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        collar_day((ds, de), prev, &mut fixes, next, &mut cells);
    }
    Ok((herds, cells, max_id, mtime))
}

/// Rewrite `battery_days` for one date. Returns the number of collars.
async fn battery_day(ctx: &Ctx, date: NaiveDate) -> anyhow::Result<usize> {
    let (ds, de) = (day_start_ms(date), day_start_ms(date) + DAY_MS);
    let mut read = snapshot(ctx, "health").await?;
    let res = battery_read(ctx, &mut read, date, ds, de).await;
    read.rollback().await?;
    let (days, max_id, mtime) = res?;
    if days.is_empty() && max_id == 0 && mtime.is_none() {
        return Ok(0);
    }

    let d = date.format("%Y-%m-%d").to_string();
    let mut tx = ctx.db().begin().await?;
    sqlx::query("DELETE FROM battery_days WHERE date = ?").bind(&d).execute(&mut *tx).await?;
    for chunk in days.chunks(200) {
        let sql = format!(
            "INSERT INTO battery_days (collar_id, date, min, max, mean, last, n, first_t, last_t) VALUES {}",
            vec!["(?, ?, ?, ?, ?, ?, ?, ?, ?)"; chunk.len()].join(", ")
        );
        let mut q = sqlx::query(&sql);
        for (collar, b) in chunk {
            q = q.bind(collar).bind(&d).bind(b.min).bind(b.max).bind(b.mean).bind(b.last).bind(b.n as i64).bind(b.first_t).bind(b.last_t);
        }
        q.execute(&mut *tx).await?;
    }
    record_day(&mut *tx, "health", &d, max_id, mtime).await?;
    tx.commit().await?;
    Ok(days.len())
}

/// One collar's battery on one day.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DayBattery {
    pub min: f64,
    pub max: f64,
    pub mean: f64,
    pub last: f64,
    pub n: u32,
    pub first_t: i64,
    pub last_t: i64,
}

impl DayBattery {
    /// From readings `(t, battery)` in any order; `None` when none is a number.
    pub fn of(readings: &mut [(i64, f64)]) -> Option<Self> {
        readings.sort_by_key(|r| r.0);
        let v: Vec<(i64, f64)> = readings.iter().copied().filter(|r| r.1.is_finite()).collect();
        let (first, last) = (v.first()?, v.last()?);
        Some(Self {
            min: v.iter().map(|r| r.1).fold(f64::INFINITY, f64::min),
            max: v.iter().map(|r| r.1).fold(f64::NEG_INFINITY, f64::max),
            mean: v.iter().map(|r| r.1).sum::<f64>() / v.len() as f64,
            last: last.1,
            n: v.len() as u32,
            first_t: first.0,
            last_t: last.0,
        })
    }
}

type BatteryRead = (Vec<(String, DayBattery)>, i64, Option<i64>);

async fn battery_read(ctx: &Ctx, conn: &mut SqliteConnection, date: NaiveDate, ds: i64, de: i64) -> anyhow::Result<BatteryRead> {
    let file = existing(day_path(ctx.data_dir(), "health", date));
    let mtime = file.as_deref().and_then(mtime_ms);
    let (mut cold, cold_max) = match file {
        Some(p) => blocking(move || file_battery(&p)).await?,
        None => (HashMap::new(), 0),
    };
    let mut collars: BTreeSet<String> = cold.keys().cloned().collect();
    collars.extend(hot_collars(conn, "health").await?);
    let mut out = Vec::new();
    let mut max_id = cold_max;
    for collar in collars {
        let mut readings = cold.remove(&collar).unwrap_or_default();
        let hot: Vec<(i64, i64, f64)> =
            sqlx::query_as("SELECT id, t, battery FROM health WHERE collar_id = ? AND t >= ? AND t < ? AND id > ? AND battery IS NOT NULL ORDER BY t")
                .bind(&collar)
                .bind(ds)
                .bind(de)
                .bind(cold_max)
                .fetch_all(&mut *conn)
                .await?;
        for (id, t, b) in hot {
            max_id = max_id.max(id);
            readings.push((t, b));
        }
        if let Some(b) = DayBattery::of(&mut readings) {
            out.push((collar, b));
        }
    }
    Ok((out, max_id, mtime))
}

async fn record_day(tx: &mut SqliteConnection, source: &str, date: &str, max_id: i64, mtime: Option<i64>) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO analytics_days (source, date, max_id, file_mtime, updated_at) VALUES (?, ?, ?, ?, ?) \
         ON CONFLICT(source, date) DO UPDATE SET max_id = MAX(analytics_days.max_id, excluded.max_id), file_mtime = excluded.file_mtime, updated_at = excluded.updated_at",
    )
    .bind(source)
    .bind(date)
    .bind(max_id)
    .bind(mtime)
    .bind(op_core::time::to_db(&op_core::time::now()))
    .execute(tx)
    .await?;
    Ok(())
}

// ---------------------------------------------------------------- which days

/// Days of `table` that changed since the last run: those holding rows with
/// an id above the mark (and, with `lookback_ms`, the days whose gaps those
/// rows close), and days whose file is new or holds rows the day's
/// aggregate hasn't seen. With no mark yet, every day there is data for.
async fn changed_days(ctx: &Ctx, table: &str, top: i64, lookback_ms: i64) -> anyhow::Result<BTreeSet<NaiveDate>> {
    let mark: Option<i64> = sqlx::query_scalar("SELECT max_id FROM analytics_day_marks WHERE source = ?").bind(table).fetch_optional(ctx.db()).await?;
    let mut days = BTreeSet::new();
    let nums: Vec<i64> = match mark {
        None => sqlx::query_scalar(&format!("SELECT DISTINCT t / {DAY_MS} FROM {table}")).fetch_all(ctx.db()).await?,
        Some(m) if m < top => {
            sqlx::query_scalar(&format!(
                "SELECT t / {DAY_MS} FROM {table} WHERE id > ?1 AND id <= ?2 UNION SELECT (t - ?3) / {DAY_MS} FROM {table} WHERE id > ?1 AND id <= ?2"
            ))
            .bind(m)
            .bind(top)
            .bind(lookback_ms)
            .fetch_all(ctx.db())
            .await?
        }
        Some(_) => Vec::new(),
    };
    days.extend(nums.into_iter().filter_map(day_of));

    let done: HashMap<String, (i64, Option<i64>)> =
        sqlx::query_as::<_, (String, i64, Option<i64>)>("SELECT date, max_id, file_mtime FROM analytics_days WHERE source = ?")
            .bind(table)
            .fetch_all(ctx.db())
            .await?
            .into_iter()
            .map(|(d, m, t)| (d, (m, t)))
            .collect();
    for (date, path) in list_days(ctx.data_dir(), table) {
        let key = date.format("%Y-%m-%d").to_string();
        let mtime = mtime_ms(&path);
        let redo = match done.get(&key) {
            None => true,
            Some((_, seen)) if *seen == mtime => false,
            Some((max_id, _)) => {
                let p = path.clone();
                let file_max = blocking(move || file_max_id(&p)).await?;
                if file_max > *max_id {
                    true
                } else {
                    // A rollup moved rows the aggregate already has.
                    sqlx::query("UPDATE analytics_days SET file_mtime = ? WHERE source = ? AND date = ?")
                        .bind(mtime)
                        .bind(table)
                        .bind(&key)
                        .execute(ctx.db())
                        .await?;
                    false
                }
            }
        };
        if redo {
            days.insert(date);
            if lookback_ms > 0 {
                days.extend(date.pred_opt());
            }
        }
    }
    Ok(days)
}

async fn set_mark(ctx: &Ctx, table: &str, top: i64) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO analytics_day_marks (source, max_id, updated_at) VALUES (?, ?, ?) \
         ON CONFLICT(source) DO UPDATE SET max_id = excluded.max_id, updated_at = excluded.updated_at",
    )
    .bind(table)
    .bind(top)
    .bind(op_core::time::to_db(&op_core::time::now()))
    .execute(ctx.db())
    .await?;
    Ok(())
}

/// A first fix anywhere, for a grid origin when there is no farm.
async fn any_fix(ctx: &Ctx) -> anyhow::Result<Option<LonLat>> {
    let hot: Option<(f64, f64)> = sqlx::query_as("SELECT lon, lat FROM fixes LIMIT 1").fetch_optional(ctx.db()).await?;
    if let Some((lon, lat)) = hot {
        return Ok(Some([lon, lat]));
    }
    let Some((_, path)) = list_days(ctx.data_dir(), "fixes").into_iter().next() else {
        return Ok(None);
    };
    blocking(move || {
        let mut out = None;
        read_day_file(&path, &fix_schema(), |b| {
            let c = Cols::new(b);
            if out.is_none() && b.num_rows() > 0 {
                if let (Some(lon), Some(lat)) = (get_f64(c.f64("lon"), 0), get_f64(c.f64("lat"), 0)) {
                    out = Some([lon, lat]);
                }
            }
        })?;
        Ok(out)
    })
    .await
}

// ---------------------------------------------------------------- runs

/// Days one run rewrote (with anything in them).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Aggregated {
    pub coverage: Vec<NaiveDate>,
    pub battery: Vec<NaiveDate>,
}

/// One pass: every day that changed since the last pass, oldest first,
/// today included as far as it has gone. Days after tomorrow (a collar
/// with a bad clock) are left alone.
pub async fn aggregate(ctx: &Ctx, now: DateTime<Utc>) -> anyhow::Result<Aggregated> {
    let last = now.date_naive().succ_opt().unwrap_or(now.date_naive());
    let mut done = Aggregated::default();

    // A day that fails is logged and tried again next run: the mark only
    // moves once every changed day has been written.
    let top = top_id(ctx, "fixes").await?;
    let days: Vec<NaiveDate> = changed_days(ctx, "fixes", top, MAX_GAP_MS).await?.into_iter().filter(|d| *d <= last).collect();
    let mut ok = true;
    if !days.is_empty() {
        let grid = match grid(ctx).await? {
            Some(g) => Some(g),
            None => ensure_grid(ctx, any_fix(ctx).await?).await?,
        };
        if let Some(grid) = grid {
            for d in days {
                match coverage_day(ctx, grid, d).await {
                    Ok(0) => {}
                    Ok(_) => done.coverage.push(d),
                    Err(e) => {
                        ok = false;
                        tracing::warn!(date = %d, "coverage day: {e:#}");
                    }
                }
            }
        }
    }
    if ok {
        set_mark(ctx, "fixes", top).await?;
    }

    let top = top_id(ctx, "health").await?;
    let days: Vec<NaiveDate> = changed_days(ctx, "health", top, 0).await?.into_iter().filter(|d| *d <= last).collect();
    let mut ok = true;
    for d in days {
        match battery_day(ctx, d).await {
            Ok(0) => {}
            Ok(_) => done.battery.push(d),
            Err(e) => {
                ok = false;
                tracing::warn!(date = %d, "battery day: {e:#}");
            }
        }
    }
    if ok {
        set_mark(ctx, "health", top).await?;
    }
    Ok(done)
}

/// The background task: a first pass a minute after start, then every 10
/// minutes (see [`EVERY`]) until shutdown.
pub fn spawn(ctx: Ctx) {
    tokio::spawn(async move {
        let mut wait = FIRST_AFTER;
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tokio::time::sleep(wait) => {}
            }
            let started = Instant::now();
            match aggregate(&ctx, op_core::time::now()).await {
                Ok(a) if !a.coverage.is_empty() || !a.battery.is_empty() => {
                    tracing::debug!(coverage = a.coverage.len(), battery = a.battery.len(), ms = started.elapsed().as_millis() as u64, "day tables updated")
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("day tables: {e:#}"),
            }
            wait = (started.elapsed() * 20).clamp(EVERY, AT_MOST);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::range::date_of;

    const DAY0: i64 = 1_790_035_200_000; // a UTC midnight

    fn fx(t: i64, cx: i32) -> Fx {
        Fx { t, cx, cy: 0, acc: 2.5, herd: 0 }
    }

    #[test]
    fn midnight_is_a_whole_day() {
        assert_eq!(DAY0 % DAY_MS, 0);
        assert_eq!(day_of(DAY0 / DAY_MS).unwrap(), date_of(DAY0));
    }

    #[test]
    fn every_fix_counts_where_it_landed() {
        let mut cells = HashMap::new();
        let mut v: Vec<Fx> = (0..10).map(|i| fx(DAY0 + i * 5000, (i / 5) as i32)).collect();
        collar_day((DAY0, DAY0 + DAY_MS), None, &mut v, None, &mut cells);
        let a = &cells[&(0, 0, 0)];
        assert_eq!((a.got, a.expected, a.n), (5, 5, 5));
        assert_eq!(a.hist[acc_bin(2.5)], 5);
        assert_eq!(cells[&(0, 1, 0)].got, 5);
    }

    #[test]
    fn a_gap_counts_where_the_animal_was_before_it() {
        let mut cells = HashMap::new();
        // Fixes every 5 s in cell 0, then 60 s of nothing, then cell 1.
        let mut v: Vec<Fx> = (0..20).map(|i| fx(DAY0 + i * 5000, 0)).collect();
        v.push(fx(DAY0 + 19 * 5000 + 60_000, 1));
        collar_day((DAY0, DAY0 + DAY_MS), None, &mut v, None, &mut cells);
        // 11 fixes were due in the gap and never came.
        assert_eq!(cells[&(0, 0, 0)].expected, 20 + 11);
        assert_eq!(cells[&(0, 0, 0)].got, 20);
        assert_eq!(cells[&(0, 1, 0)].expected, 1);
    }

    #[test]
    fn gaps_across_midnight_split_between_the_days() {
        // Cadence 5 s; the collar's last fix before midnight is 30 s before it,
        // and its first today 30 s after: 11 misses, 5 before midnight, 6 after
        // (the one due exactly at midnight is today's).
        let prev = Fx { t: DAY0 - 30_000, cx: 7, cy: 7, acc: f32::NAN, herd: 0 };
        let mut v: Vec<Fx> = (0..20).map(|i| fx(DAY0 + 30_000 + i * 5000, 0)).collect();
        let mut cells = HashMap::new();
        collar_day((DAY0, DAY0 + DAY_MS), Some(prev), &mut v, None, &mut cells);
        assert_eq!(cells[&(0, 7, 7)].expected, 6);
        assert_eq!(cells[&(0, 7, 7)].got, 0);
        // The day before sees the other five from its side.
        let mut w: Vec<Fx> = (0..20).map(|i| Fx { t: DAY0 - 30_000 - (19 - i) * 5000, cx: 7, cy: 7, acc: 2.0, herd: 0 }).collect();
        let mut before = HashMap::new();
        collar_day((DAY0 - DAY_MS, DAY0), None, &mut w, Some(DAY0 + 30_000), &mut before);
        assert_eq!(before[&(0, 7, 7)].expected, 20 + 5);
    }

    #[test]
    fn a_long_gap_means_the_collar_was_off() {
        let mut cells = HashMap::new();
        let mut v: Vec<Fx> = (0..20).map(|i| fx(DAY0 + i * 5000, 0)).collect();
        v.push(fx(DAY0 + 19 * 5000 + MAX_GAP_MS + 1, 1));
        collar_day((DAY0, DAY0 + DAY_MS), None, &mut v, None, &mut cells);
        assert_eq!(cells[&(0, 0, 0)].expected, 20);
    }

    #[test]
    fn duplicates_count_once_and_one_fix_has_no_cadence() {
        let mut cells = HashMap::new();
        let mut v = vec![fx(DAY0, 0), fx(DAY0, 0), fx(DAY0 + 5000, 0)];
        collar_day((DAY0, DAY0 + DAY_MS), None, &mut v, Some(DAY0 + 600_000), &mut cells);
        // Two fixes 5 s apart, then 595 s to the next fix: 118 were due.
        assert_eq!(cells[&(0, 0, 0)].got, 2);
        assert_eq!(cells[&(0, 0, 0)].expected, 2 + 118);
        let mut cells = HashMap::new();
        let mut one = vec![fx(DAY0, 0)];
        collar_day((DAY0, DAY0 + DAY_MS), None, &mut one, Some(DAY0 + 600_000), &mut cells);
        assert_eq!(cells[&(0, 0, 0)].expected, 1);
    }

    #[test]
    fn accuracy_buckets() {
        assert_eq!(acc_bin(0.0), 0);
        assert_eq!(acc_bin(0.99), 0);
        assert_eq!(acc_bin(1.0), 1);
        assert_eq!(acc_bin(4.99), 3);
        assert_eq!(acc_bin(5.0), 4);
        assert_eq!(acc_bin(19.9), 6);
        assert_eq!(acc_bin(20.0), 7);
        assert_eq!(acc_bin(500.0), 7);
    }

    #[test]
    fn grid_cells_tile_exactly() {
        let g = Grid::new([-93.62, 42.03]);
        let p = Projection::new([-93.62, 42.03]).offset(25.0, -3.0);
        assert_eq!(g.cell(p), (2, -1));
        let b = g.block_bbox(2, -1, 1);
        let c = g.block_center(2, -1, 1);
        assert!(b[0] < c[0] && c[0] < b[2] && b[1] < c[1] && c[1] < b[3]);
        let s = g.block_size_deg(1);
        assert!((b[2] - b[0] - s[0]).abs() < 2e-7 && (b[3] - b[1] - s[1]).abs() < 2e-7);
        // A block of 3 holds cells 0..3 of its row.
        assert_eq!(g.block_bbox(0, 0, 3)[2], g.block_bbox(2, 0, 1)[2]);
        assert_eq!(g.cells_in([b[0] + 1e-7, b[1] + 1e-7, b[2] - 1e-7, b[3] - 1e-7]), (2, -1, 2, -1));
    }

    #[test]
    fn battery_of_a_day() {
        let mut r = vec![(30, 0.8), (10, 0.9), (20, f64::NAN), (40, 0.7)];
        let b = DayBattery::of(&mut r).unwrap();
        assert_eq!((b.min, b.max, b.last, b.n, b.first_t, b.last_t), (0.7, 0.9, 0.7, 3, 10, 40));
        assert!((b.mean - 0.8).abs() < 1e-12);
        assert!(DayBattery::of(&mut []).is_none());
    }
}
