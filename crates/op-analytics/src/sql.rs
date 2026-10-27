//! Read-only SQL over the farm's tables with DataFusion.
//!
//! Each query gets a fresh session with only the tables it names. Record
//! tables and hot telemetry are read from SQLite into memory tables (capped);
//! cold telemetry is a Parquet listing table, unioned with the hot rows so
//! `fixes` and `cues` look like one table each.
//!
//! One query can't take the server down: the session has no table functions
//! (`range`, `generate_series`), no recursive CTEs, and none of the scalar
//! functions that make a value far bigger than what they read
//! ([`AMPLIFYING`]: `repeat`, the pads, `replace`, `array_repeat`, …): the
//! memory pool only counts joins, sorts and aggregates, not a scalar's
//! result, so a value is only ever as big as the rows it came from. Its
//! operators share a [`MEMORY_LIMIT`] pool (a query past it fails,
//! "resources exhausted"), the answer is at most [`MAX_ROWS`] rows and
//! [`MAX_RESULT_BYTES`] of JSON, at most [`AT_ONCE`] queries run at a time
//! (others wait up to [`QUEUE_WAIT`], then 503), and each has [`TIMEOUT`].
//! This is not a process memory limit: an isolated query worker with an OS
//! limit would be (docs/internal/fix-2026-10-27/safety.md).

use std::sync::Arc;
use std::time::{Duration, Instant};

use datafusion::arrow::record_batch::RecordBatch;
use datafusion::catalog::TableProvider;
use datafusion::common::config::Dialect;
use datafusion::datasource::MemTable;
use datafusion::datasource::file_format::parquet::ParquetFormat;
use datafusion::datasource::listing::{ListingOptions, ListingTable, ListingTableConfig, ListingTableUrl};
use datafusion::execution::context::SQLOptions;
use datafusion::execution::runtime_env::RuntimeEnvBuilder;
use datafusion::prelude::{SessionConfig, SessionContext};
use datafusion::sql::parser::Statement as DfStatement;
use datafusion::sql::sqlparser::ast::Statement as SqlStatement;
use futures::{StreamExt, TryStreamExt};
use op_core::{ApiError, Ctx};
use serde::Serialize;
use serde_json::Value;

use crate::range::TimeRange;
use crate::schema::{BatchBuilder, SQL_TABLES, TableSchema, cell_json, is_telemetry, table_schema};
use crate::telemetry::{BATCH_ROWS, Rolled, not_rolled_sql};

/// Most rows a query returns.
pub const MAX_ROWS: usize = 10_000;
/// Most JSON a query's answer holds (bytes); the rows after are cut (`truncated`).
pub const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;
/// Longest query text.
pub const MAX_QUERY_BYTES: usize = 16 * 1024;
/// Scalar functions whose result can be far bigger than their input (or
/// nested, grow without bound): not in a `run_sql` session.
pub const AMPLIFYING: [&str; 11] =
    ["repeat", "lpad", "rpad", "replace", "regexp_replace", "array_repeat", "array_resize", "list_repeat", "list_resize", "range", "generate_series"];
pub const TIMEOUT: Duration = Duration::from_secs(20);
/// Memory one query's joins, sorts and aggregates may hold.
pub const MEMORY_LIMIT: usize = 256 * 1024 * 1024;
/// Queries running at once.
pub const AT_ONCE: usize = 2;
/// How long a query waits for its turn.
pub const QUEUE_WAIT: Duration = Duration::from_secs(5);

static SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(AT_ONCE);
/// Most rows read into memory from SQLite per record table.
const RECORD_CAP: usize = 100_000;
/// Most hot telemetry rows read into memory per table (newest kept). Hot
/// telemetry is a few days at most (older days are in Parquet), so this is
/// plenty for a farm and keeps one query from holding gigabytes.
const HOT_CAP: usize = 500_000;

#[derive(Debug, Serialize)]
pub struct SqlResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub ms: u64,
    /// More rows than returned (or than could be loaded from SQLite).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub truncated: bool,
}

/// Reject anything that isn't one SELECT / WITH / VALUES / EXPLAIN of one.
fn check_read_only(stmt: &DfStatement) -> Result<(), ApiError> {
    fn sql_ok(s: &SqlStatement) -> bool {
        match s {
            SqlStatement::Query(_) => true,
            SqlStatement::Explain { statement, .. } => sql_ok(statement),
            _ => false,
        }
    }
    let ok = match stmt {
        DfStatement::Statement(s) => sql_ok(s),
        DfStatement::Explain(e) => matches!(e.statement.as_ref(), DfStatement::Statement(s) if sql_ok(s)),
        _ => false,
    };
    if ok { Ok(()) } else { Err(ApiError::bad_request("Only a single read-only SELECT, WITH or EXPLAIN query is allowed.")) }
}

fn df_err(e: datafusion::error::DataFusionError) -> ApiError {
    // DataFusion messages are already readable; drop the error-kind prefix noise.
    ApiError::bad_request(e.to_string())
}

pub async fn run(ctx: &Ctx, query: &str, max_rows: usize) -> Result<SqlResult, ApiError> {
    let started = Instant::now();
    let max_rows = max_rows.clamp(1, MAX_ROWS);
    let Ok(Ok(_slot)) = tokio::time::timeout(QUEUE_WAIT, SLOTS.acquire()).await else {
        return Err(ApiError::new(axum::http::StatusCode::SERVICE_UNAVAILABLE, "Other queries are running. Try again in a moment."));
    };
    match tokio::time::timeout(TIMEOUT, run_inner(ctx, query, max_rows)).await {
        Ok(Ok((columns, rows, truncated))) => Ok(SqlResult { columns, rows, ms: started.elapsed().as_millis() as u64, truncated }),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(ApiError::new(
            axum::http::StatusCode::REQUEST_TIMEOUT,
            format!("The query took longer than {} s. Narrow it with a WHERE on t.", TIMEOUT.as_secs()),
        )),
    }
}

/// A session for one query: memory capped, no table functions, no
/// recursive CTEs, no [`AMPLIFYING`] scalar functions.
fn session() -> Result<SessionContext, ApiError> {
    let runtime = RuntimeEnvBuilder::new().with_memory_limit(MEMORY_LIMIT, 1.0).build_arc().map_err(df_err)?;
    let config = SessionConfig::new().set_bool("datafusion.execution.enable_recursive_ctes", false);
    let session = SessionContext::new_with_config_rt(config, runtime);
    let functions: Vec<String> = session.state().table_functions().keys().cloned().collect();
    for f in functions {
        session.deregister_udtf(&f);
    }
    for f in AMPLIFYING {
        session.deregister_udf(f);
    }
    Ok(session)
}

async fn run_inner(ctx: &Ctx, query: &str, max_rows: usize) -> Result<(Vec<String>, Vec<Vec<Value>>, bool), ApiError> {
    if query.len() > MAX_QUERY_BYTES {
        return Err(ApiError::bad_request(format!("The query is longer than {} KB.", MAX_QUERY_BYTES / 1024)));
    }
    let session = session()?;
    let state = session.state();
    let stmt = state.sql_to_statement(query, &Dialect::Generic).map_err(df_err)?;
    check_read_only(&stmt)?;

    let mut truncated = false;
    for r in state.resolve_table_references(&stmt).map_err(df_err)? {
        let name = r.table().to_ascii_lowercase();
        if !SQL_TABLES.contains(&name.as_str()) || session.table_exist(name.as_str()).unwrap_or(false) {
            continue;
        }
        let (provider, cut) = provider(ctx, &session, &name, None).await?;
        truncated |= cut;
        session.register_table(name.as_str(), provider).map_err(df_err)?;
    }

    let plan = session.state().statement_to_plan(stmt).await.map_err(df_err)?;
    SQLOptions::new().with_allow_ddl(false).with_allow_dml(false).with_allow_statements(false).verify_plan(&plan).map_err(df_err)?;
    let df = session.execute_logical_plan(plan).await.map_err(df_err)?;
    let columns: Vec<String> = df.schema().fields().iter().map(|f| f.name().clone()).collect();
    let mut stream = df.execute_stream().await.map_err(df_err)?;
    let mut rows = Vec::new();
    let mut bytes = 0usize;
    while let Some(batch) = stream.next().await {
        let batch = batch.map_err(df_err)?;
        for i in 0..batch.num_rows() {
            if rows.len() >= max_rows {
                return Ok((columns, rows, true));
            }
            let row: Vec<Value> = batch.columns().iter().map(|c| cell_json(c.as_ref(), i)).collect();
            bytes += serde_json::to_vec(&row).map_or(0, |v| v.len());
            if bytes > MAX_RESULT_BYTES {
                return Ok((columns, rows, true));
            }
            rows.push(row);
        }
    }
    Ok((columns, rows, truncated))
}

/// A DataFusion table for one of [`SQL_TABLES`]. For telemetry, `range`
/// limits both the SQLite read and the Parquet files. Returns whether the
/// SQLite read hit its cap.
pub async fn provider(ctx: &Ctx, session: &SessionContext, table: &str, range: Option<&TimeRange>) -> Result<(Arc<dyn TableProvider>, bool), ApiError> {
    let schema = table_schema(ctx.db(), table).await?;
    let cap = if is_telemetry(table) { HOT_CAP } else { RECORD_CAP };
    // @L: rows the rollup has written to a day's file and not yet deleted are read from the file only.
    let (files, rolled) = if is_telemetry(table) {
        let (dir, t, r) = (ctx.data_dir().to_path_buf(), table.to_owned(), range.copied());
        let (files, rolled) = tokio::task::spawn_blocking(move || crate::telemetry::day_files(&dir, &t, r.as_ref())).await.map_err(anyhow::Error::from)??;
        (files, crate::telemetry::live(ctx, table, rolled).await?)
    } else {
        (Vec::new(), Vec::new())
    };
    let (batches, cut) = load_sqlite(ctx, &schema, range, cap, &rolled).await?;
    let hot: Arc<dyn TableProvider> = Arc::new(MemTable::try_new(schema.arrow.clone(), vec![batches]).map_err(df_err)?);
    if !is_telemetry(table) {
        return Ok((hot, cut));
    }
    if files.is_empty() {
        return Ok((hot, cut));
    }
    let urls = files.iter().map(|p| ListingTableUrl::parse(p.to_string_lossy())).collect::<Result<Vec<_>, _>>().map_err(df_err)?;
    let options = ListingOptions::new(Arc::new(ParquetFormat::default())).with_file_extension(".parquet");
    let config = ListingTableConfig::new_with_multi_paths(urls).with_listing_options(options).with_schema(schema.arrow.clone());
    let cold: Arc<dyn TableProvider> = Arc::new(ListingTable::try_new(config).map_err(df_err)?);
    let mut cold_df = session.read_table(cold).map_err(df_err)?;
    if let Some(r) = range {
        use datafusion::prelude::{col, lit};
        cold_df = cold_df.filter(col("t").gt_eq(lit(r.from_ms())).and(col("t").lt(lit(r.to_ms())))).map_err(df_err)?;
    }
    let union = session.read_table(hot).map_err(df_err)?.union(cold_df).map_err(df_err)?;
    Ok((union.into_view(), cut))
}

/// Read a table from SQLite into record batches, at most `cap` rows (for
/// telemetry the newest).
async fn load_sqlite(ctx: &Ctx, schema: &TableSchema, range: Option<&TimeRange>, cap: usize, rolled: &[Rolled]) -> Result<(Vec<RecordBatch>, bool), ApiError> {
    let telemetry = is_telemetry(&schema.table);
    let mut sql = format!("SELECT {} FROM {} WHERE 1", schema.select_list(), schema.table);
    if telemetry && range.is_some() {
        sql.push_str(" AND t >= ? AND t < ?");
    }
    sql.push_str(&not_rolled_sql(rolled.len()));
    if telemetry {
        sql.push_str(" ORDER BY id DESC");
    }
    sql.push_str(" LIMIT ?");
    let mut q = sqlx::query(&sql);
    if let (true, Some(r)) = (telemetry, range) {
        q = q.bind(r.from_ms()).bind(r.to_ms());
    }
    for r in rolled {
        q = q.bind(r.from).bind(r.to).bind(r.max_id);
    }
    q = q.bind(cap as i64 + 1);
    let mut rows = q.fetch(ctx.db());
    let mut b = BatchBuilder::new(schema);
    let mut out = Vec::new();
    let mut n = 0usize;
    let mut cut = false;
    while let Some(row) = rows.try_next().await? {
        if n == cap {
            cut = true;
            break;
        }
        b.push(&row);
        n += 1;
        if b.len() >= BATCH_ROWS {
            out.push(b.finish()?);
        }
    }
    if !b.is_empty() {
        out.push(b.finish()?);
    }
    Ok((out, cut))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(q: &str) -> Result<(), ApiError> {
        let s = SessionContext::new().state();
        let stmt = s.sql_to_statement(q, &Dialect::Generic).map_err(df_err)?;
        check_read_only(&stmt)
    }

    #[tokio::test]
    async fn a_session_has_no_table_functions_and_a_memory_cap() {
        let s = session().unwrap();
        assert!(s.state().table_functions().is_empty());
        let e = s.sql("SELECT count(*) FROM range(1, 10000000000)").await.expect_err("no range()");
        assert!(e.to_string().contains("range"), "{e}");
        assert!(matches!(s.runtime_env().memory_pool.memory_limit(), datafusion::execution::memory_pool::MemoryLimit::Finite(n) if n == MEMORY_LIMIT));
    }

    /// Astra 02 F14: nothing in a query makes a value bigger than its rows.
    #[tokio::test]
    async fn no_value_grows_past_what_it_reads() {
        let s = session().unwrap();
        for q in [
            "SELECT length(repeat('x', 300000000)) AS bytes",
            "SELECT length(lpad('x', 300000000)) AS bytes",
            "SELECT length(replace(replace('x', 'x', 'xxxxxxxxxx'), 'x', 'xxxxxxxxxx'))",
            "SELECT cardinality(array_repeat(1, 300000000))",
            "WITH RECURSIVE r(s) AS (SELECT 'x' UNION ALL SELECT s || s FROM r) SELECT length(s) FROM r LIMIT 40",
        ] {
            let r = match s.sql(q).await {
                Ok(df) => df.collect().await.map(|_| ()),
                Err(e) => Err(e),
            };
            assert!(r.is_err(), "{q} ran");
        }
        // Ordinary functions are still there.
        let ok = s.sql("SELECT upper(concat('a', 'b')) AS x, length('abc') AS n").await.unwrap().collect().await.unwrap();
        assert_eq!(ok[0].num_rows(), 1);
    }

    #[test]
    fn read_only_statements() {
        assert!(parse("SELECT 1").is_ok());
        assert!(parse("WITH a AS (SELECT 1 AS x) SELECT x FROM a").is_ok());
        assert!(parse("EXPLAIN SELECT * FROM fixes").is_ok());
        for bad in [
            "INSERT INTO fixes VALUES (1)",
            "DELETE FROM fixes",
            "UPDATE fixes SET lon = 0",
            "DROP TABLE fixes",
            "CREATE TABLE x (a INT)",
            "CREATE EXTERNAL TABLE x STORED AS PARQUET LOCATION '/tmp/x'",
            "COPY fixes TO '/tmp/x.csv'",
            "SET datafusion.execution.batch_size = 1",
            "SELECT 1; DELETE FROM fixes",
            "EXPLAIN INSERT INTO fixes VALUES (1)",
        ] {
            assert!(parse(bad).is_err(), "{bad} should be rejected");
        }
    }
}
