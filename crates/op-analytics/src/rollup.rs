//! Moves telemetry older than a few days from SQLite to daily Parquet files.
//! Runs from [`crate::start`].
//!
//! A day streams through: its rows are read per collar in `(collar_id, t, id)`
//! order, [`CHUNK_ROWS`] at a time, merged with the day's existing file (if an
//! earlier run or a late fix left one, deduplicated as [`Dedupe`] says), and written one
//! Parquet row group per chunk; pasture dwell is summed on the same stream. At
//! 250 collars and a fix every 5 s a day is 4.3 M fixes, so nothing ever holds
//! the whole day. Once the file is durable the rows leave SQLite in short
//! write transactions of at most [`DELETE_ROWS`], with a pause after each, so
//! collar reports keep landing while it runs.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Days, NaiveDate, Utc};
use datafusion::arrow::array::{Array, Int64Array, StringArray};
use datafusion::arrow::compute::interleave_record_batch;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use futures::TryStreamExt;
use op_core::Ctx;
use parquet::arrow::ArrowWriter;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use tokio::sync::mpsc;

use crate::metrics::{Dwell, DwellAgg, DwellKey, PaddockIndex};
use crate::range::{date_of, day_start_ms};
use crate::schema::{BatchBuilder, Cols, TELEMETRY_TABLES, TableSchema, normalize, table_schema};
use crate::telemetry::{BATCH_ROWS, day_path, each_fix, list_days, writer_props};

/// Setting key: days of telemetry kept in SQLite (default 3, at least 1).
pub const HOT_DAYS_KEY: &str = "analytics.hot_days";
pub const DEFAULT_HOT_DAYS: u32 = 3;
/// Rows read from SQLite, merged and written as one Parquet row group at a time.
pub const CHUNK_ROWS: usize = 65_536;
/// Most rows one delete transaction removes.
pub const DELETE_ROWS: i64 = 5_000;
/// Pause after each delete transaction. SQLite's busy handler retries at
/// most 25 ms apart for the first 100 ms of a wait (then 50 and 100 ms), so
/// a writer that queued behind a transaction gets the lock in this gap; after
/// a transaction that ran long the gap is [`DELETE_PAUSE_LONG`].
const DELETE_PAUSE: Duration = Duration::from_millis(25);
const DELETE_PAUSE_LONG: Duration = Duration::from_millis(100);
/// A delete transaction longer than this halves the next one.
const DELETE_SLOW: Duration = Duration::from_millis(60);

/// Setting key: how long raw telemetry is kept ([`Retention`]).
pub const RETENTION_KEY: &str = "analytics.retention";
/// The shortest retention accepted, in days.
pub const MIN_RETENTION_DAYS: u32 = 7;

/// Days of raw rows kept per table, in SQLite or Parquet alike; `None`
/// (the default) keeps them for good. Day summaries (dwell, coverage,
/// battery, welfare marks) and episodes are kept whatever this says. The
/// welfare record and grazing reports read raw cues and fixes, so a short
/// retention shortens what those can cover.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Retention {
    #[serde(default)]
    pub fixes_days: Option<u32>,
    #[serde(default)]
    pub cues_days: Option<u32>,
    #[serde(default)]
    pub health_days: Option<u32>,
}

impl Retention {
    pub fn days(&self, table: &str) -> Option<u32> {
        match table {
            "fixes" => self.fixes_days,
            "cues" => self.cues_days,
            "health" => self.health_days,
            _ => None,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        for (name, days) in [("fixes_days", self.fixes_days), ("cues_days", self.cues_days), ("health_days", self.health_days)] {
            if days.is_some_and(|d| d < MIN_RETENTION_DAYS) {
                return Err(format!("{name} must be at least {MIN_RETENTION_DAYS} days, or null to keep everything."));
            }
        }
        Ok(())
    }
}

/// The stored retention. An unreadable value is an error, so nothing is
/// deleted on a guess.
pub async fn retention(ctx: &Ctx) -> anyhow::Result<Retention> {
    Ok(ctx.store().get_setting::<Retention>(RETENTION_KEY).await.context("reading analytics.retention")?.unwrap_or_default())
}

/// What [`prune`] removed from one table.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pruned {
    pub table: &'static str,
    pub days: usize,
    pub rows: u64,
}

/// Delete raw telemetry older than its retention: whole Parquet days before
/// the cutoff day, and SQLite rows before it (when retention is shorter than
/// the hot days).
pub async fn prune(ctx: &Ctx, keep: &Retention, now: DateTime<Utc>) -> anyhow::Result<Vec<Pruned>> {
    let mut done = Vec::new();
    for table in TELEMETRY_TABLES {
        let Some(days) = keep.days(table) else { continue };
        let cutoff = now.date_naive().checked_sub_days(Days::new(days as u64)).unwrap_or(now.date_naive());
        let mut gone_days = 0;
        for (date, path) in list_days(ctx.data_dir(), table) {
            if date >= cutoff {
                continue;
            }
            let dir = path.parent().unwrap_or(&path).to_path_buf();
            let removed = if dir.is_dir() { std::fs::remove_dir_all(&dir) } else { std::fs::remove_file(&path) };
            match removed {
                Ok(()) => gone_days += 1,
                Err(e) => tracing::warn!(file = %dir.display(), "removing expired telemetry: {e}"),
            }
        }
        let sql = format!("DELETE FROM {table} WHERE id IN (SELECT id FROM {table} WHERE t < ? LIMIT ?)");
        let mut rows = 0;
        loop {
            let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
            let n = sqlx::query(&sql).bind(day_start_ms(cutoff)).bind(DELETE_ROWS).execute(&mut *tx).await?.rows_affected();
            tx.commit().await?;
            rows += n;
            if n < DELETE_ROWS as u64 {
                break;
            }
            tokio::time::sleep(DELETE_PAUSE).await;
        }
        if gone_days > 0 || rows > 0 {
            tracing::info!(table, days = gone_days, rows, before = %cutoff, "expired raw telemetry removed");
        }
        done.push(Pruned { table, days: gone_days, rows });
    }
    Ok(done)
}

pub async fn hot_days(ctx: &Ctx) -> u32 {
    match ctx.store().get_setting::<u32>(HOT_DAYS_KEY).await {
        Ok(Some(n)) => n.max(1),
        _ => DEFAULT_HOT_DAYS,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RolledDay {
    pub table: &'static str,
    pub date: String,
    pub rows: usize,
}

/// Roll every whole UTC day before `now - keep_days` (midnight) to Parquet.
/// Each day is written (merged with an existing file, deduplicated) and
/// synced before its rows are deleted, so a crash never loses data; a crash
/// part way through the deletes leaves rows that the next run merges again.
pub async fn rollup(ctx: &Ctx, keep_days: u32, now: DateTime<Utc>) -> anyhow::Result<Vec<RolledDay>> {
    let cutoff_day = now.date_naive().checked_sub_days(Days::new(keep_days as u64)).unwrap_or(now.date_naive());
    let cutoff_ms = day_start_ms(cutoff_day);
    let mut done = Vec::new();
    for table in TELEMETRY_TABLES {
        loop {
            let min_t: Option<i64> = sqlx::query_scalar(&format!("SELECT MIN(t) FROM {table} WHERE t < ?")).bind(cutoff_ms).fetch_one(ctx.db()).await?;
            let Some(min_t) = min_t else { break };
            let date = date_of(min_t);
            let rows = roll_day(ctx, table, date).await?;
            done.push(RolledDay { table, date: date.format("%Y-%m-%d").to_string(), rows });
        }
    }
    Ok(done)
}

async fn roll_day(ctx: &Ctx, table: &'static str, date: NaiveDate) -> anyhow::Result<usize> {
    let (ds, de) = (day_start_ms(date), day_start_ms(date) + 86_400_000);
    let schema = table_schema(ctx.db(), table).await?;
    // Rows stored from here on wait for the next run.
    let max_id: Option<i64> = sqlx::query_scalar(&format!("SELECT MAX(id) FROM {table}")).fetch_one(ctx.db()).await?;
    let Some(max_id) = max_id else { return Ok(0) };

    let path = day_path(ctx.data_dir(), table, date);
    let paddocks = if table == "fixes" { Some(PaddockIndex::new(&ctx.store().list_paddocks().await?)) } else { None };
    let (tx, rx) = mpsc::channel::<anyhow::Result<RecordBatch>>(2);
    let arrow = schema.arrow.clone();
    let path2 = path.clone();
    let dedupe = Dedupe::of(table);
    let writer = tokio::task::spawn_blocking(move || write_day(&path2, arrow, rx, paddocks, dedupe));

    let n = match read_day(ctx, &schema, ds, de, max_id, &tx).await {
        Ok(n) => n,
        Err(e) => {
            // The writer drops its temp file instead of replacing the day's.
            let _ = tx.send(Err(anyhow::anyhow!("reading the day failed"))).await;
            drop(tx);
            let _ = writer.await;
            return Err(e);
        }
    };
    drop(tx);
    let summary = writer.await.context("rollup writer panicked")??;

    if let Some(summary) = summary {
        let d = date.format("%Y-%m-%d").to_string();
        let day_no = ds.div_euclid(86_400_000);
        let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
        sqlx::query("DELETE FROM analytics_paddock_days WHERE date = ?").bind(&d).execute(&mut *tx).await?;
        for ((day, herd, collar, paddock), agg) in summary {
            if day != day_no {
                continue;
            }
            sqlx::query("INSERT INTO analytics_paddock_days (date, herd_id, collar_id, paddock_id, fixes, dwell_s, last_t) VALUES (?, ?, ?, ?, ?, ?, ?)")
                .bind(&d)
                .bind(herd)
                .bind(collar)
                .bind(paddock)
                .bind(agg.fixes)
                .bind(agg.dwell_ms as f64 / 1000.0)
                .bind(agg.last_t)
                .execute(&mut *tx)
                .await?;
        }
        tx.commit().await?;
    }
    delete_day(ctx, table, ds, de, max_id).await?;
    tracing::info!(table, date = %date, rows = n, file = %path.display(), "rolled telemetry to parquet");
    Ok(n)
}

/// The day's rows with `id <= max_id`, per collar in `(t, id)` order (collars
/// in byte order), sent on in chunks of [`CHUNK_ROWS`]. Returns how many.
async fn read_day(ctx: &Ctx, schema: &TableSchema, ds: i64, de: i64, max_id: i64, tx: &mpsc::Sender<anyhow::Result<RecordBatch>>) -> anyhow::Result<usize> {
    let table = &schema.table;
    let sql = read_sql(schema);
    let send = |b: RecordBatch| async move { tx.send(Ok(b)).await.map_err(|_| anyhow::anyhow!("rollup writer stopped")) };
    let mut builder = BatchBuilder::new(schema);
    let mut n = 0;
    let mut after = String::new();
    // Each collar in turn by index seeks, so the day never needs a sort.
    while let Some(collar) = next_collar(ctx, table, &after).await? {
        let mut rows = sqlx::query(&sql).bind(&collar).bind(ds).bind(de).bind(max_id).fetch(ctx.db());
        while let Some(r) = rows.try_next().await? {
            builder.push(&r);
            n += 1;
            if builder.len() >= CHUNK_ROWS {
                send(builder.finish()?).await?;
            }
        }
        drop(rows);
        after = collar;
    }
    if !builder.is_empty() {
        send(builder.finish()?).await?;
    }
    Ok(n)
}

/// One collar's rows of a day up to a row id, in order (by `<table>_collar_t`).
pub fn read_sql(schema: &TableSchema) -> String {
    format!("SELECT {} FROM {} WHERE collar_id = ? AND t >= ? AND t < ? AND id <= ? ORDER BY t, id", schema.select_list(), schema.table)
}

/// Up to `?4` rolled rows of a day (by `<table>_t`).
pub fn delete_sql(table: &str) -> String {
    format!("DELETE FROM {table} WHERE id IN (SELECT id FROM {table} WHERE t >= ? AND t < ? AND id <= ? LIMIT ?)")
}

/// The next collar id in `table` after `after`, by the `(collar_id, t)` index.
pub async fn next_collar(ctx: &Ctx, table: &str, after: &str) -> anyhow::Result<Option<String>> {
    Ok(sqlx::query_scalar(&format!("SELECT collar_id FROM {table} WHERE collar_id > ? ORDER BY collar_id LIMIT 1"))
        .bind(after)
        .fetch_optional(ctx.db())
        .await?)
}

/// Delete what was rolled, a short write transaction at a time, pausing
/// after each so a report waiting on the lock gets in. A transaction that
/// runs long halves the next one.
async fn delete_day(ctx: &Ctx, table: &str, ds: i64, de: i64, max_id: i64) -> anyhow::Result<()> {
    let sql = delete_sql(table);
    let mut size = DELETE_ROWS;
    loop {
        let started = std::time::Instant::now();
        let mut tx = op_core::store::begin_immediate(ctx.db()).await?;
        let gone = sqlx::query(&sql).bind(ds).bind(de).bind(max_id).bind(size).execute(&mut *tx).await?.rows_affected();
        tx.commit().await?;
        let took = started.elapsed();
        if gone < size as u64 {
            return Ok(());
        }
        size = if took > DELETE_SLOW {
            (size / 2).max(DELETE_ROWS / 10)
        } else if took < DELETE_SLOW / 2 {
            (size + size / 4).min(DELETE_ROWS)
        } else {
            size
        };
        tokio::time::sleep(if took > DELETE_SLOW + DELETE_SLOW / 2 { DELETE_PAUSE_LONG } else { DELETE_PAUSE }).await;
    }
}

/// Which rows of the day file and the fresh rows are the same record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dedupe {
    /// The same row id: rows an earlier run wrote but stopped before deleting.
    Row,
    /// The same collar and instant, and these columns equal: a resent report
    /// stored again after its day was rolled keeps the copy already in the
    /// file. A different record at the same instant is kept.
    Same(&'static [&'static str]),
}

/// What makes two fixes at one instant the same fix (migration 0890's key).
pub const FIX_IDENTITY: &[&str] = &["lon", "lat", "accuracy_m"];
/// What makes two cues at one instant the same cue (migration 0890's key).
pub const CUE_IDENTITY: &[&str] = &["kind", "level", "margin_m", "lon", "lat"];

impl Dedupe {
    /// Matches the unique keys on `fixes` and `cues` (migration 0890).
    pub fn of(table: &str) -> Self {
        match table {
            "fixes" => Dedupe::Same(FIX_IDENTITY),
            "cues" => Dedupe::Same(CUE_IDENTITY),
            _ => Dedupe::Row,
        }
    }
}

/// Sort key of a row: collar (bytes), time, id; and the identity columns.
struct Keys {
    collar: StringArray,
    t: Int64Array,
    id: Int64Array,
    batch: RecordBatch,
}

impl Keys {
    fn of(b: &RecordBatch) -> anyhow::Result<Self> {
        let c = Cols::new(b);
        Ok(Self {
            collar: c.str("collar_id").context("telemetry without collar_id")?.clone(),
            t: c.i64("t").context("telemetry without t")?.clone(),
            id: c.i64("id").context("telemetry without id")?.clone(),
            batch: b.clone(),
        })
    }

    /// The row's identity columns as text (exact: floats print so they
    /// read back the same); a column the file lacks or a null is empty.
    fn identity(&self, i: usize, cols: &[&str]) -> Vec<String> {
        cols.iter()
            .map(|name| match self.batch.column_by_name(name) {
                Some(col) if col.is_valid(i) => datafusion::arrow::util::display::array_value_to_string(col, i).unwrap_or_default(),
                _ => String::new(),
            })
            .collect()
    }

    fn key(&self, i: usize) -> (&[u8], i64, i64) {
        let collar = if self.collar.is_valid(i) { self.collar.value(i).as_bytes() } else { &[] };
        (collar, if self.t.is_valid(i) { self.t.value(i) } else { i64::MIN }, if self.id.is_valid(i) { self.id.value(i) } else { i64::MIN })
    }
}

/// One sorted input: its current batch, keys and row.
struct Cursor<I: Iterator<Item = anyhow::Result<RecordBatch>>> {
    source: I,
    cur: Option<(usize, Keys)>,
    row: usize,
}

impl<I: Iterator<Item = anyhow::Result<RecordBatch>>> Cursor<I> {
    /// Move to the next non-empty batch; its slot in `held` is returned in `cur`.
    fn advance(&mut self, held: &mut Vec<RecordBatch>) -> anyhow::Result<()> {
        self.cur = None;
        self.row = 0;
        for b in self.source.by_ref() {
            let b = b?;
            if b.num_rows() == 0 {
                continue;
            }
            let keys = Keys::of(&b)?;
            held.push(b);
            self.cur = Some((held.len() - 1, keys));
            break;
        }
        Ok(())
    }

    fn head(&self) -> Option<(usize, usize, (&[u8], i64, i64))> {
        self.cur.as_ref().map(|(slot, k)| (*slot, self.row, k.key(self.row)))
    }

    /// Whether the current row is the collar and instant given, and its identity.
    fn at(&self, collar: &[u8], t: i64, cols: &[&str]) -> Option<Vec<String>> {
        let (_, k) = self.cur.as_ref()?;
        let (c, rt, _) = k.key(self.row);
        (c == collar && rt == t).then(|| k.identity(self.row, cols))
    }

    fn step(&mut self, held: &mut Vec<RecordBatch>) -> anyhow::Result<()> {
        self.row += 1;
        let len = self.cur.as_ref().map_or(0, |(slot, _)| held[*slot].num_rows());
        if self.row >= len { self.advance(held) } else { Ok(()) }
    }
}

/// Write the day file from the fresh rows (and the existing file, merged),
/// one row group per chunk, and sum dwell on the way when `paddocks` is given.
/// The file is replaced atomically and synced before this returns.
fn write_day(
    path: &Path,
    arrow: SchemaRef,
    mut fresh: mpsc::Receiver<anyhow::Result<RecordBatch>>,
    paddocks: Option<PaddockIndex>,
    dedupe: Dedupe,
) -> anyhow::Result<Option<HashMap<DwellKey, DwellAgg>>> {
    let dir = path.parent().context("parquet path has no parent")?;
    std::fs::create_dir_all(dir)?;
    let tmp: PathBuf = dir.join(format!(".part.{}.tmp", std::process::id()));
    let mut dwell = paddocks.as_ref().map(|_| Dwell::default());
    let written = (|| -> anyhow::Result<()> {
        let mut out = ArrowWriter::try_new(File::create(&tmp)?, arrow.clone(), Some(writer_props()))?;
        {
            let mut emit = |chunk: RecordBatch| -> anyhow::Result<()> {
                if let (Some(d), Some(p)) = (dwell.as_mut(), paddocks.as_ref()) {
                    each_fix(&chunk, |f| d.push(f.collar_id, f.herd_id, p.resolve(f.paddock_id, [f.lon, f.lat]), f.t));
                }
                out.write(&chunk)?;
                out.flush()?;
                Ok(())
            };
            let fresh_rows = std::iter::from_fn(|| fresh.blocking_recv());
            if path.is_file() {
                let reader = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?.with_batch_size(BATCH_ROWS).build()?;
                let old = reader.map(|b| b.map_err(anyhow::Error::from).and_then(|b| normalize(&b, &arrow)));
                merge(old, fresh_rows, dedupe, &mut emit)?;
            } else {
                // Nothing to merge with: the fresh rows already come in order.
                for b in fresh_rows {
                    emit(b?)?;
                }
            }
        }
        let mut file = out.into_inner()?;
        file.flush()?;
        file.sync_all()?;
        Ok(())
    })();
    if let Err(e) = written {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    std::fs::rename(&tmp, path)?;
    // The rename is only durable once the directory is synced; the rows are
    // deleted from SQLite right after this returns.
    #[cfg(unix)]
    File::open(dir)?.sync_all()?;
    Ok(dwell.map(|d| d.finish(i64::MAX)))
}

/// Merge two inputs sorted by `(collar_id, t, id)` into chunks of
/// [`CHUNK_ROWS`], keeping one copy of a row both hold (an earlier run that
/// stopped before its deletes) and, by `dedupe`, of a fix or cue both hold
/// under different ids (`a`'s copy is kept).
fn merge<A, B>(a: A, b: B, dedupe: Dedupe, emit: &mut dyn FnMut(RecordBatch) -> anyhow::Result<()>) -> anyhow::Result<()>
where
    A: Iterator<Item = anyhow::Result<RecordBatch>>,
    B: Iterator<Item = anyhow::Result<RecordBatch>>,
{
    let mut held: Vec<RecordBatch> = Vec::new();
    let mut a = Cursor { source: a, cur: None, row: 0 };
    let mut b = Cursor { source: b, cur: None, row: 0 };
    a.advance(&mut held)?;
    b.advance(&mut held)?;
    let mut picks: Vec<(usize, usize)> = Vec::with_capacity(CHUNK_ROWS);
    loop {
        let group = match (a.head(), b.head(), dedupe) {
            (Some((_, _, ka)), Some((_, _, kb)), Dedupe::Same(cols)) if (ka.0, ka.1) == (kb.0, kb.1) => Some((ka.0.to_vec(), ka.1, cols)),
            _ => None,
        };
        if let Some((collar, t, cols)) = group {
            // The same collar and instant in both: all of a's rows there,
            // then b's that aren't the same record as one of them.
            let mut seen = Vec::new();
            while let Some(id) = a.at(&collar, t, cols) {
                let (slot, row, _) = a.head().expect("a row");
                picks.push((slot, row));
                seen.push(id);
                a.step(&mut held)?;
            }
            while let Some(id) = b.at(&collar, t, cols) {
                let (slot, row, _) = b.head().expect("a row");
                if !seen.contains(&id) {
                    picks.push((slot, row));
                    seen.push(id);
                }
                b.step(&mut held)?;
            }
            if picks.len() >= CHUNK_ROWS {
                flush_picks(&mut picks, &mut held, &mut a, &mut b, emit)?;
            }
            continue;
        }
        let ha = a.head().map(|(slot, row, _)| (slot, row));
        let hb = b.head().map(|(slot, row, _)| (slot, row));
        let order = match (a.head(), b.head()) {
            (Some((_, _, ka)), Some((_, _, kb))) => Some(ka.cmp(&kb)),
            _ => None,
        };
        let pick = match (ha, hb, order) {
            (None, None, _) => break,
            (Some(p), None, _) | (Some(p), Some(_), Some(Ordering::Less)) => {
                a.step(&mut held)?;
                p
            }
            (None, Some(p), _) | (Some(_), Some(p), Some(Ordering::Greater)) => {
                b.step(&mut held)?;
                p
            }
            (Some(p), Some(_), _) => {
                // The same row in both: keep one.
                a.step(&mut held)?;
                b.step(&mut held)?;
                p
            }
        };
        picks.push(pick);
        if picks.len() >= CHUNK_ROWS {
            flush_picks(&mut picks, &mut held, &mut a, &mut b, emit)?;
        }
    }
    if !picks.is_empty() {
        flush_picks(&mut picks, &mut held, &mut a, &mut b, emit)?;
    }
    Ok(())
}

/// Build and emit the picked rows, then keep only the batches the cursors are still on.
fn flush_picks<A, B>(
    picks: &mut Vec<(usize, usize)>,
    held: &mut Vec<RecordBatch>,
    a: &mut Cursor<A>,
    b: &mut Cursor<B>,
    emit: &mut dyn FnMut(RecordBatch) -> anyhow::Result<()>,
) -> anyhow::Result<()>
where
    A: Iterator<Item = anyhow::Result<RecordBatch>>,
    B: Iterator<Item = anyhow::Result<RecordBatch>>,
{
    let refs: Vec<&RecordBatch> = held.iter().collect();
    emit(interleave_record_batch(&refs, picks)?)?;
    picks.clear();
    let mut kept = Vec::new();
    for cur in [&mut a.cur, &mut b.cur] {
        if let Some((slot, _)) = cur.as_mut() {
            kept.push(held[*slot].clone());
            *slot = kept.len() - 1;
        }
    }
    *held = kept;
    Ok(())
}

/// How often the rollup (and then retention) runs.
pub const ROLLUP_EVERY: Duration = Duration::from_secs(3600);

/// Hourly rollup, then retention (and once at start), until shutdown.
pub fn spawn_rollup(ctx: Ctx) {
    ctx.supervise("analytics.rollup", Some(ROLLUP_EVERY), |ctx| async move {
        let mut tick = tokio::time::interval(ROLLUP_EVERY);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tick.tick() => {
                    ctx.beat("analytics.rollup");
                    let keep = hot_days(&ctx).await;
                    match rollup(&ctx, keep, op_core::time::now()).await {
                        Ok(days) if !days.is_empty() => tracing::info!(days = days.len(), "telemetry rollup done"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("telemetry rollup failed: {e:#}"),
                    }
                    match retention(&ctx).await {
                        Ok(r) if r != Retention::default() => {
                            if let Err(e) = prune(&ctx, &r, op_core::time::now()).await {
                                tracing::warn!("telemetry retention failed: {e:#}");
                            }
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("telemetry retention skipped: {e:#}"),
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use datafusion::arrow::array::{Int64Array, StringArray};
    use datafusion::arrow::datatypes::{DataType, Field, Schema};

    use super::*;

    fn batch(rows: &[(&str, i64, i64)]) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("collar_id", DataType::Utf8, true),
            Field::new("t", DataType::Int64, true),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(rows.iter().map(|r| r.2).collect::<Vec<_>>())),
                Arc::new(StringArray::from(rows.iter().map(|r| r.0).collect::<Vec<_>>())),
                Arc::new(Int64Array::from(rows.iter().map(|r| r.1).collect::<Vec<_>>())),
            ],
        )
        .unwrap()
    }

    fn rows(b: &[RecordBatch]) -> Vec<(String, i64, i64)> {
        b.iter()
            .flat_map(|b| {
                let k = Keys::of(b).unwrap();
                (0..b.num_rows()).map(move |i| (k.collar.value(i).to_owned(), k.t.value(i), k.id.value(i))).collect::<Vec<_>>()
            })
            .collect()
    }

    #[test]
    fn merge_interleaves_two_sorted_inputs_and_keeps_one_copy_of_a_shared_row() {
        let old = vec![batch(&[("a", 1, 1), ("a", 5, 3)]), batch(&[]), batch(&[("b", 2, 2)])];
        let fresh = vec![batch(&[("a", 3, 9), ("a", 5, 3)]), batch(&[("b", 1, 7), ("c", 0, 8)])];
        let mut out = Vec::new();
        merge(old.into_iter().map(Ok), fresh.into_iter().map(Ok), Dedupe::Row, &mut |b| {
            out.push(b);
            Ok(())
        })
        .unwrap();
        assert_eq!(rows(&out), [("a", 1, 1), ("a", 3, 9), ("a", 5, 3), ("b", 1, 7), ("b", 2, 2), ("c", 0, 8)].map(|(c, t, i)| (c.to_owned(), t, i)).to_vec());
    }

    fn cue_batch(rows: &[(&str, i64, i64, Option<&str>)]) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("collar_id", DataType::Utf8, true),
            Field::new("t", DataType::Int64, true),
            Field::new("kind", DataType::Utf8, true),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(rows.iter().map(|r| r.2).collect::<Vec<_>>())),
                Arc::new(StringArray::from(rows.iter().map(|r| r.0).collect::<Vec<_>>())),
                Arc::new(Int64Array::from(rows.iter().map(|r| r.1).collect::<Vec<_>>())),
                Arc::new(StringArray::from(rows.iter().map(|r| r.3).collect::<Vec<_>>())),
            ],
        )
        .unwrap()
    }

    fn merged(old: Vec<RecordBatch>, fresh: Vec<RecordBatch>, dedupe: Dedupe) -> Vec<(String, i64, i64)> {
        let mut out = Vec::new();
        merge(old.into_iter().map(Ok), fresh.into_iter().map(Ok), dedupe, &mut |b| {
            out.push(b);
            Ok(())
        })
        .unwrap();
        rows(&out)
    }

    #[test]
    fn a_fix_resent_after_its_day_rolled_keeps_the_rolled_copy() {
        // The file holds a's fixes at t 1 and 5; a resend stored them again as ids 8 and 9.
        let old = vec![batch(&[("a", 1, 1), ("a", 5, 3)]), batch(&[("b", 2, 2)])];
        let fresh = vec![batch(&[("a", 1, 8), ("a", 3, 7), ("a", 5, 9)]), batch(&[("b", 2, 2), ("c", 0, 10)])];
        assert_eq!(
            merged(old, fresh, Dedupe::Same(&[])),
            [("a", 1, 1), ("a", 3, 7), ("a", 5, 3), ("b", 2, 2), ("c", 0, 10)].map(|(c, t, i)| (c.to_owned(), t, i)).to_vec()
        );
    }

    #[test]
    fn a_resent_cue_is_one_cue_but_another_kind_at_the_same_instant_is_kept() {
        let old = vec![cue_batch(&[("a", 5, 1, Some("warn")), ("a", 6, 2, None)])];
        let fresh = vec![cue_batch(&[("a", 5, 7, Some("outside")), ("a", 5, 8, Some("warn")), ("a", 6, 9, None)])];
        assert_eq!(merged(old, fresh, Dedupe::Same(&["kind"])), [("a", 5, 1), ("a", 5, 7), ("a", 6, 2)].map(|(c, t, i)| (c.to_owned(), t, i)).to_vec());
    }

    fn fix_batch(rows: &[(&str, i64, i64, f64)]) -> RecordBatch {
        let schema = Arc::new(Schema::new(vec![
            Field::new("id", DataType::Int64, true),
            Field::new("collar_id", DataType::Utf8, true),
            Field::new("t", DataType::Int64, true),
            Field::new("lon", DataType::Float64, true),
            Field::new("lat", DataType::Float64, true),
            Field::new("accuracy_m", DataType::Float64, true),
        ]));
        RecordBatch::try_new(
            schema,
            vec![
                Arc::new(Int64Array::from(rows.iter().map(|r| r.2).collect::<Vec<_>>())),
                Arc::new(StringArray::from(rows.iter().map(|r| r.0).collect::<Vec<_>>())),
                Arc::new(Int64Array::from(rows.iter().map(|r| r.1).collect::<Vec<_>>())),
                Arc::new(datafusion::arrow::array::Float64Array::from(rows.iter().map(|r| r.3).collect::<Vec<_>>())),
                Arc::new(datafusion::arrow::array::Float64Array::from(vec![42.03; rows.len()])),
                Arc::new(datafusion::arrow::array::Float64Array::from(vec![3.0; rows.len()])),
            ],
        )
        .unwrap()
    }

    #[test]
    fn a_different_fix_at_a_rolled_instant_is_kept_but_a_resend_is_not() {
        // The file holds a's fix at t 5 (lon -93.62). Fresh: the same fix resent (id 8)
        // and a different one at the same instant (id 9, lon -93.61).
        let old = vec![fix_batch(&[("a", 5, 1, -93.62)])];
        let fresh = vec![fix_batch(&[("a", 5, 8, -93.62), ("a", 5, 9, -93.61)])];
        assert_eq!(merged(old, fresh, Dedupe::of("fixes")), [("a", 5, 1), ("a", 5, 9)].map(|(c, t, i)| (c.to_owned(), t, i)).to_vec());
    }
}
