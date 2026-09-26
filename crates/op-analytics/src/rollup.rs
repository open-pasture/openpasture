//! Moves telemetry older than a few days from SQLite to daily Parquet files.
//! Runs from [`crate::start`].

use std::collections::HashSet;
use std::time::Duration;

use chrono::{DateTime, Days, Utc};
use datafusion::arrow::compute::{SortColumn, concat_batches, lexsort_to_indices, take_record_batch};
use datafusion::arrow::record_batch::RecordBatch;
use futures::TryStreamExt;
use op_core::Ctx;

use crate::metrics::{Dwell, PaddockIndex};
use crate::range::{date_of, day_start_ms};
use crate::schema::{BatchBuilder, Cols, TELEMETRY_TABLES, get_i64, normalize, table_schema};
use crate::telemetry::{day_path, each_fix, read_parquet, write_parquet};

/// Setting key: days of telemetry kept in SQLite (default 3, at least 1).
pub const HOT_DAYS_KEY: &str = "analytics.hot_days";
pub const DEFAULT_HOT_DAYS: u32 = 3;

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
/// Each day is written (merged with an existing file, deduplicated by `id`)
/// and synced before its rows are deleted, so a crash never loses data.
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

async fn roll_day(ctx: &Ctx, table: &'static str, date: chrono::NaiveDate) -> anyhow::Result<usize> {
    let (ds, de) = (day_start_ms(date), day_start_ms(date) + 86_400_000);
    let schema = table_schema(ctx.db(), table).await?;
    let max_id: Option<i64> = sqlx::query_scalar(&format!("SELECT MAX(id) FROM {table} WHERE t >= ? AND t < ?")).bind(ds).bind(de).fetch_one(ctx.db()).await?;
    let Some(max_id) = max_id else { return Ok(0) };

    let mut builder = BatchBuilder::new(&schema);
    let sql = format!("SELECT {} FROM {table} WHERE t >= ? AND t < ? AND id <= ?", schema.select_list());
    let mut rows = sqlx::query(&sql).bind(ds).bind(de).bind(max_id).fetch(ctx.db());
    while let Some(r) = rows.try_next().await? {
        builder.push(&r);
    }
    drop(rows);
    let fresh = builder.finish()?;
    let n = fresh.num_rows();

    let path = day_path(ctx.data_dir(), table, date);
    let arrow = schema.arrow.clone();
    let path2 = path.clone();
    let merged = tokio::task::spawn_blocking(move || -> anyhow::Result<RecordBatch> {
        let mut parts = Vec::new();
        let mut seen = HashSet::new();
        if path2.is_file() {
            for b in read_parquet(&path2)? {
                let b = normalize(&b, &arrow)?;
                let ids = Cols::new(&b).i64("id").cloned();
                for i in 0..b.num_rows() {
                    if let Some(id) = get_i64(ids.as_ref(), i) {
                        seen.insert(id);
                    }
                }
                parts.push(b);
            }
        }
        if !seen.is_empty() {
            // Rows already in the file from an earlier run that stopped
            // before deleting them.
            let ids = Cols::new(&fresh).i64("id").cloned();
            let mask: datafusion::arrow::array::BooleanArray =
                (0..fresh.num_rows()).map(|i| Some(get_i64(ids.as_ref(), i).is_none_or(|id| !seen.contains(&id)))).collect();
            parts.push(datafusion::arrow::compute::filter_record_batch(&fresh, &mask)?);
        } else {
            parts.push(fresh);
        }
        let all = concat_batches(&arrow, &parts)?;
        let sorted = sort_by_collar_t(&all)?;
        let chunks: Vec<RecordBatch> = (0..sorted.num_rows())
            .step_by(crate::telemetry::BATCH_ROWS * 8)
            .map(|o| sorted.slice(o, (crate::telemetry::BATCH_ROWS * 8).min(sorted.num_rows() - o)))
            .collect();
        write_parquet(&path2, &arrow, &chunks)?;
        Ok(sorted)
    })
    .await??;

    let summary = if table == "fixes" {
        let paddocks = PaddockIndex::new(&ctx.store().list_paddocks().await?);
        let mut dwell = Dwell::default();
        each_fix(&merged, |f| {
            dwell.push(f.collar_id, f.herd_id, paddocks.resolve(f.paddock_id, [f.lon, f.lat]), f.t);
        });
        Some(dwell.finish(i64::MAX))
    } else {
        None
    };

    let mut tx = ctx.db().begin().await?;
    if let Some(summary) = summary {
        let d = date.format("%Y-%m-%d").to_string();
        sqlx::query("DELETE FROM analytics_paddock_days WHERE date = ?").bind(&d).execute(&mut *tx).await?;
        let day_no = ds.div_euclid(86_400_000);
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
    }
    sqlx::query(&format!("DELETE FROM {table} WHERE t >= ? AND t < ? AND id <= ?")).bind(ds).bind(de).bind(max_id).execute(&mut *tx).await?;
    tx.commit().await?;
    tracing::info!(table, date = %date, rows = n, file = %path.display(), "rolled telemetry to parquet");
    Ok(n)
}

fn sort_by_collar_t(b: &RecordBatch) -> anyhow::Result<RecordBatch> {
    let mut cols = Vec::new();
    for name in ["collar_id", "t", "id"] {
        if let Some(c) = b.column_by_name(name) {
            cols.push(SortColumn { values: c.clone(), options: None });
        }
    }
    if cols.is_empty() || b.num_rows() == 0 {
        return Ok(b.clone());
    }
    let idx = lexsort_to_indices(&cols, None)?;
    Ok(take_record_batch(b, &idx)?)
}

/// Hourly rollup (and once at start) until shutdown.
pub fn spawn_rollup(ctx: Ctx) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tick.tick() => {
                    let keep = hot_days(&ctx).await;
                    match rollup(&ctx, keep, op_core::time::now()).await {
                        Ok(days) if !days.is_empty() => tracing::info!(days = days.len(), "telemetry rollup done"),
                        Ok(_) => {}
                        Err(e) => tracing::warn!("telemetry rollup failed: {e:#}"),
                    }
                }
            }
        }
    });
}
