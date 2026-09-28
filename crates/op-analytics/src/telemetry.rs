//! Telemetry across hot (SQLite) and cold (Parquet) storage.
//!
//! Cold files live at `<data_dir>/telemetry/<table>/date=YYYY-MM-DD/part.parquet`,
//! one per UTC day, rows sorted by `collar_id, t`. [`scan`] streams record
//! batches for a time range from both, so callers never care where a row is.

use std::collections::HashSet;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::Context;
use chrono::NaiveDate;
use datafusion::arrow::array::{Array, BooleanArray};
use datafusion::arrow::compute::filter_record_batch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use futures::TryStreamExt;
use op_core::Ctx;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use parquet::basic::{Compression, ZstdLevel};
use parquet::file::properties::WriterProperties;
use tokio::sync::mpsc;

use crate::range::{TimeRange, date_of};
use crate::schema::{BatchBuilder, Cols, TableSchema, get_str, normalize, table_schema};

pub const BATCH_ROWS: usize = 8192;
const ROW_GROUP_ROWS: usize = 128 * 1024;

pub fn table_dir(data_dir: &Path, table: &str) -> PathBuf {
    data_dir.join("telemetry").join(table)
}

pub fn day_path(data_dir: &Path, table: &str, date: NaiveDate) -> PathBuf {
    table_dir(data_dir, table).join(format!("date={}", date.format("%Y-%m-%d"))).join("part.parquet")
}

/// Cold day files for a table, oldest first.
pub fn list_days(data_dir: &Path, table: &str) -> Vec<(NaiveDate, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(table_dir(data_dir, table)) else {
        return Vec::new();
    };
    let mut days: Vec<(NaiveDate, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let date = NaiveDate::parse_from_str(name.strip_prefix("date=")?, "%Y-%m-%d").ok()?;
            let file = e.path().join("part.parquet");
            file.is_file().then_some((date, file))
        })
        .collect();
    days.sort_by_key(|(d, _)| *d);
    days
}

pub fn read_parquet(path: &Path) -> anyhow::Result<Vec<RecordBatch>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let reader = ParquetRecordBatchReaderBuilder::try_new(file)?.with_batch_size(BATCH_ROWS).build()?;
    reader.map(|b| b.map_err(anyhow::Error::from)).collect()
}

pub fn writer_props() -> WriterProperties {
    WriterProperties::builder().set_compression(Compression::ZSTD(ZstdLevel::default())).set_max_row_group_row_count(Some(ROW_GROUP_ROWS)).build()
}

/// Write atomically: a temp file, fsync, then rename over `path`.
pub fn write_parquet(path: &Path, schema: &SchemaRef, batches: &[RecordBatch]) -> anyhow::Result<()> {
    let dir = path.parent().context("parquet path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".part.{}.tmp", std::process::id()));
    {
        let file = File::create(&tmp)?;
        let mut w = ArrowWriter::try_new(file, schema.clone(), Some(writer_props()))?;
        for b in batches {
            w.write(b)?;
        }
        let mut file = w.into_inner()?;
        file.flush()?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    // The rename itself is only durable once the directory is synced; the
    // rollup deletes the SQLite rows right after this returns.
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(())
}

/// Which rows a scan returns.
#[derive(Debug, Clone, Default)]
pub struct Scope {
    /// Only these collars. `Some(empty)` matches nothing.
    pub collar_ids: Option<Vec<String>>,
    /// Only rows recorded for this herd (the herd the collar was in then).
    pub herd_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    All,
    Hot,
    Cold,
}

/// Stream a telemetry table's rows in `[from, to)`. Cold days come first,
/// then SQLite; within each, rows are ordered by `collar_id, t`, so per collar
/// the rows are in time order. Batches match the current SQLite schema.
pub fn scan(ctx: &Ctx, table: &'static str, range: TimeRange, scope: Scope, source: Source) -> mpsc::Receiver<anyhow::Result<RecordBatch>> {
    scan_in(ctx, table, range, scope, source, Order::Collar)
}

// @L
/// How the SQLite rows of a scan come.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    /// By `collar_id, t`: all of a collar's rows, then the next collar's.
    Collar,
    /// By `t`: per collar still in time order, but collars interleave. It
    /// reads the table in about the order it was written, so a day of 250
    /// collars costs one pass over its pages instead of one per collar (the
    /// rows of 250 collars reporting together share every page).
    Time,
}

/// [`scan`] with the SQLite rows in `order` (cold days are always by
/// collar, then time, and come first, so per collar every row still comes
/// in time order).
pub fn scan_in(ctx: &Ctx, table: &'static str, range: TimeRange, scope: Scope, source: Source, order: Order) -> mpsc::Receiver<anyhow::Result<RecordBatch>> {
    let (tx, rx) = mpsc::channel(4);
    let ctx = ctx.clone();
    tokio::spawn(async move {
        if let Err(e) = scan_into(&ctx, table, range, &scope, source, order, &tx).await {
            let _ = tx.send(Err(e)).await;
        }
    });
    rx
}

async fn scan_into(
    ctx: &Ctx,
    table: &'static str,
    range: TimeRange,
    scope: &Scope,
    source: Source,
    order: Order,
    tx: &mpsc::Sender<anyhow::Result<RecordBatch>>,
) -> anyhow::Result<()> {
    if scope.collar_ids.as_ref().is_some_and(|c| c.is_empty()) {
        return Ok(());
    }
    let schema = table_schema(ctx.db(), table).await?;
    let (from, to) = (range.from_ms(), range.to_ms());
    // @L: days read from Parquet, with the highest row id each file holds.
    let mut rolled: Vec<Rolled> = Vec::new();
    if source != Source::Hot {
        let (first, last) = (date_of(from), date_of(to - 1));
        for (date, path) in list_days(ctx.data_dir(), table) {
            if date < first || date > last {
                continue;
            }
            let arrow = schema.arrow.clone();
            let scope = scope.clone();
            let (batches, max_id) = tokio::task::spawn_blocking(move || -> anyhow::Result<(Vec<RecordBatch>, Option<i64>)> {
                let mut out = Vec::new();
                let mut max_id: Option<i64> = None;
                for b in read_parquet(&path)? {
                    let b = normalize(&b, &arrow)?;
                    if let Some(ids) = Cols::new(&b).i64("id") {
                        max_id = max_id.max(datafusion::arrow::compute::max(ids));
                    }
                    let b = filter_batch(&b, from, to, &scope)?;
                    if b.num_rows() > 0 {
                        out.push(b);
                    }
                }
                Ok((out, max_id))
            })
            .await??;
            if let Some(max_id) = max_id {
                let start = crate::range::day_start_ms(date);
                rolled.push(Rolled { from: start, to: start + 86_400_000, max_id });
            }
            for b in batches {
                if tx.send(Ok(b)).await.is_err() {
                    return Ok(());
                }
            }
        }
    }
    if source != Source::Cold {
        hot_scan(ctx, &schema, from, to, scope, order, &rolled, tx).await?;
    }
    Ok(())
}

// @L
/// A day in Parquet: its span and the highest row id its file holds. While
/// the rollup deletes a day it has written (a transaction of rows at a
/// time), those rows are still in SQLite too; ids only grow
/// (`AUTOINCREMENT`), so a hot row of that day at or below the file's
/// highest id is one of them, and a scan of both counts it once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rolled {
    pub from: i64,
    pub to: i64,
    pub max_id: i64,
}

/// SQL keeping rows of `rolled` days out (three binds each: from, to, max id).
pub fn not_rolled_sql(n: usize) -> String {
    " AND NOT (t >= ? AND t < ? AND id <= ?)".repeat(n)
}

/// The days of `table` in Parquet (overlapping `range`) with their highest
/// row id, from the files' statistics (else the id column).
pub fn rolled_days(data_dir: &Path, table: &str, range: Option<&TimeRange>) -> anyhow::Result<Vec<Rolled>> {
    let mut out = Vec::new();
    for (date, path) in list_days(data_dir, table) {
        if range.is_some_and(|r| date < date_of(r.from_ms()) || date > date_of(r.to_ms() - 1)) {
            continue;
        }
        if let Some(max_id) = file_max_id(&path)? {
            let start = crate::range::day_start_ms(date);
            out.push(Rolled { from: start, to: start + 86_400_000, max_id });
        }
    }
    Ok(out)
}

fn file_max_id(path: &Path) -> anyhow::Result<Option<i64>> {
    use parquet::arrow::ProjectionMask;
    use parquet::file::statistics::Statistics;
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
    let meta = builder.metadata().clone();
    let Some(col) = meta.file_metadata().schema_descr().columns().iter().position(|c| c.name() == "id") else { return Ok(None) };
    let from_stats: Option<Vec<i64>> = meta
        .row_groups()
        .iter()
        .map(|rg| match rg.column(col).statistics() {
            Some(Statistics::Int64(s)) => s.max_opt().copied(),
            _ => None,
        })
        .collect();
    if let Some(v) = from_stats {
        return Ok(v.into_iter().max());
    }
    let mask = ProjectionMask::leaves(meta.file_metadata().schema_descr(), [col]);
    let mut max_id = None;
    for b in builder.with_projection(mask).build()? {
        let b = b?;
        if let Some(ids) = b.column(0).as_any().downcast_ref::<datafusion::arrow::array::Int64Array>() {
            max_id = max_id.max(datafusion::arrow::compute::max(ids));
        }
    }
    Ok(max_id)
}

/// The SQL of a hot scan. By time, the collar list is only a filter (`+`
/// keeps SQLite from walking the collar index and sorting what it found).
/// `rolled` days' rolled rows are left out (see [`Rolled`]).
pub fn hot_scan_sql(select: &str, table: &str, herd: bool, collars: Option<usize>, order: Order, rolled: usize) -> String {
    let mut sql = format!("SELECT {select} FROM {table} WHERE t >= ? AND t < ?");
    if herd {
        sql.push_str(" AND herd_id = ?");
    }
    if let Some(n) = collars {
        let col = if order == Order::Time { "+collar_id" } else { "collar_id" };
        sql.push_str(&format!(" AND {col} IN ({})", vec!["?"; n].join(",")));
    }
    sql.push_str(&not_rolled_sql(rolled));
    sql.push_str(match order {
        Order::Collar => " ORDER BY collar_id, t, id",
        Order::Time => " ORDER BY t, id",
    });
    sql
}

async fn hot_scan(
    ctx: &Ctx,
    schema: &TableSchema,
    from: i64,
    to: i64,
    scope: &Scope,
    order: Order,
    rolled: &[Rolled],
    tx: &mpsc::Sender<anyhow::Result<RecordBatch>>,
) -> anyhow::Result<()> {
    let rolled: Vec<Rolled> = rolled.iter().filter(|r| r.from < to && r.to > from).copied().collect();
    let sql = hot_scan_sql(&schema.select_list(), &schema.table, scope.herd_id.is_some(), scope.collar_ids.as_ref().map(Vec::len), order, rolled.len());
    let mut q = sqlx::query(&sql).bind(from).bind(to);
    if let Some(h) = &scope.herd_id {
        q = q.bind(h);
    }
    if let Some(ids) = &scope.collar_ids {
        for id in ids {
            q = q.bind(id);
        }
    }
    for r in &rolled {
        q = q.bind(r.from).bind(r.to).bind(r.max_id);
    }
    let mut rows = q.fetch(ctx.db());
    let mut b = BatchBuilder::new(schema);
    while let Some(row) = rows.try_next().await? {
        b.push(&row);
        if b.len() >= BATCH_ROWS && tx.send(Ok(b.finish()?)).await.is_err() {
            return Ok(());
        }
    }
    if !b.is_empty() {
        let _ = tx.send(Ok(b.finish()?)).await;
    }
    Ok(())
}

fn filter_batch(b: &RecordBatch, from: i64, to: i64, scope: &Scope) -> anyhow::Result<RecordBatch> {
    let cols = Cols::new(b);
    let t = cols.i64("t").context("telemetry batch without t")?;
    let collar = cols.str("collar_id");
    let herd = cols.str("herd_id");
    let ids: Option<HashSet<&str>> = scope.collar_ids.as_ref().map(|v| v.iter().map(String::as_str).collect());
    let mask: BooleanArray = (0..b.num_rows())
        .map(|i| {
            let ok = t.is_valid(i)
                && t.value(i) >= from
                && t.value(i) < to
                && scope.herd_id.as_deref().is_none_or(|h| get_str(herd, i) == Some(h))
                && ids.as_ref().is_none_or(|s| get_str(collar, i).is_some_and(|c| s.contains(c)));
            Some(ok)
        })
        .collect();
    if mask.true_count() == b.num_rows() {
        return Ok(b.clone());
    }
    Ok(filter_record_batch(b, &mask)?)
}

/// Run `f` on every batch of a scan.
pub async fn for_each(mut rx: mpsc::Receiver<anyhow::Result<RecordBatch>>, mut f: impl FnMut(&RecordBatch)) -> anyhow::Result<()> {
    while let Some(b) = rx.recv().await {
        f(&b?);
    }
    Ok(())
}

/// Everything a scan returns, as one list of batches.
pub async fn collect(rx: mpsc::Receiver<anyhow::Result<RecordBatch>>) -> anyhow::Result<Vec<RecordBatch>> {
    let mut out = Vec::new();
    for_each(rx, |b| out.push(b.clone())).await?;
    Ok(out)
}

/// A fix as the analytics code sees it.
#[derive(Debug, Clone, Copy)]
pub struct FixRow<'a> {
    pub collar_id: &'a str,
    pub herd_id: Option<&'a str>,
    pub animal_id: Option<&'a str>,
    pub t: i64,
    pub lon: f64,
    pub lat: f64,
    pub accuracy_m: Option<f64>,
    pub sats: Option<i64>,
    pub cn0: Option<f64>,
    pub ttf_s: Option<f64>,
    pub paddock_id: Option<&'a str>,
}

/// Call `f` for each fix in a batch that has a collar, time and position.
pub fn each_fix<'a>(b: &'a RecordBatch, mut f: impl FnMut(FixRow<'a>)) {
    use crate::schema::{get_f64, get_i64};
    let c = Cols::new(b);
    let (collar, herd, animal, t, lon, lat) = (c.str("collar_id"), c.str("herd_id"), c.str("animal_id"), c.i64("t"), c.f64("lon"), c.f64("lat"));
    let (acc, sats, cn0, ttf, paddock) = (c.f64("accuracy_m"), c.i64("sats"), c.f64("cn0"), c.f64("ttf_s"), c.str("paddock_id"));
    for i in 0..b.num_rows() {
        let (Some(collar_id), Some(t), Some(lon), Some(lat)) = (get_str(collar, i), get_i64(t, i), get_f64(lon, i), get_f64(lat, i)) else {
            continue;
        };
        f(FixRow {
            collar_id,
            herd_id: get_str(herd, i),
            animal_id: get_str(animal, i),
            t,
            lon,
            lat,
            accuracy_m: get_f64(acc, i),
            sats: get_i64(sats, i),
            cn0: get_f64(cn0, i),
            ttf_s: get_f64(ttf, i),
            paddock_id: get_str(paddock, i),
        });
    }
}

/// A cue: collar, time, level, margin, position if known.
#[derive(Debug, Clone, Copy)]
pub struct CueRow<'a> {
    pub collar_id: &'a str,
    pub animal_id: Option<&'a str>,
    pub t: i64,
    pub level: Option<i64>,
    pub margin_m: Option<f64>,
}

pub fn each_cue<'a>(b: &'a RecordBatch, mut f: impl FnMut(CueRow<'a>)) {
    use crate::schema::{get_f64, get_i64};
    let c = Cols::new(b);
    let (collar, animal, t, level, margin) = (c.str("collar_id"), c.str("animal_id"), c.i64("t"), c.i64("level"), c.f64("margin_m"));
    for i in 0..b.num_rows() {
        let (Some(collar_id), Some(t)) = (get_str(collar, i), get_i64(t, i)) else {
            continue;
        };
        f(CueRow { collar_id, animal_id: get_str(animal, i), t, level: get_i64(level, i), margin_m: get_f64(margin, i) });
    }
}

/// Cold file URLs for DataFusion, optionally only days overlapping a range.
pub fn cold_files(data_dir: &Path, table: &str, range: Option<&TimeRange>) -> Vec<PathBuf> {
    list_days(data_dir, table)
        .into_iter()
        .filter(|(d, _)| range.is_none_or(|r| *d >= date_of(r.from_ms()) && *d <= date_of(r.to_ms() - 1)))
        .map(|(_, p)| p)
        .collect()
}
