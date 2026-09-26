//! `GET /api/export`: CSV, GeoJSON or Parquet, streamed as the rows are read.

use std::collections::BTreeMap;

use axum::body::{Body, Bytes};
use axum::extract::{Query, State};
use axum::http::header;
use axum::response::{IntoResponse, Response};
use chrono::Duration;
use datafusion::arrow::datatypes::SchemaRef;
use datafusion::arrow::record_batch::RecordBatch;
use futures::TryStreamExt;
use op_core::{ApiError, ApiResult, Ctx};
use parquet::arrow::ArrowWriter;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::mpsc;

use crate::range::TimeRange;
use crate::schema::{BatchBuilder, Cols, SQL_TABLES, cell_json, get_f64, get_i64, get_str, is_telemetry, table_schema};
use crate::telemetry::{BATCH_ROWS, Scope, Source, scan, writer_props};

#[derive(Debug, Default, Deserialize)]
pub struct ExportParams {
    pub table: Option<String>,
    pub format: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub collar_id: Option<String>,
    pub herd_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Csv,
    GeoJson,
    Parquet,
}

impl Format {
    fn parse(s: Option<&str>) -> Result<Self, ApiError> {
        match s.unwrap_or("csv").to_ascii_lowercase().as_str() {
            "csv" => Ok(Format::Csv),
            "geojson" | "json" => Ok(Format::GeoJson),
            "parquet" => Ok(Format::Parquet),
            other => Err(ApiError::bad_request(format!("Unknown format `{other}`. Use csv, geojson or parquet."))),
        }
    }
    fn ext(self) -> &'static str {
        match self {
            Format::Csv => "csv",
            Format::GeoJson => "geojson",
            Format::Parquet => "parquet",
        }
    }
    fn mime(self) -> &'static str {
        match self {
            Format::Csv => "text/csv; charset=utf-8",
            Format::GeoJson => "application/geo+json",
            Format::Parquet => "application/vnd.apache.parquet",
        }
    }
}

const GEOJSON_TABLES: [&str; 5] = ["fixes", "cues", "tracks", "paddocks", "boundaries"];

type Tx = mpsc::Sender<Result<Bytes, std::io::Error>>;

pub async fn export(State(ctx): State<Ctx>, Query(p): Query<ExportParams>) -> ApiResult<Response> {
    let table = p.table.as_deref().filter(|s| !s.is_empty()).ok_or_else(|| ApiError::bad_request("Say which `table` to export."))?.to_ascii_lowercase();
    let format = Format::parse(p.format.as_deref())?;
    let source: &'static str = match table.as_str() {
        "tracks" => "fixes",
        t => SQL_TABLES
            .iter()
            .find(|n| **n == t)
            .copied()
            .ok_or_else(|| ApiError::bad_request(format!("Unknown table `{t}`. Use one of {} or tracks.", SQL_TABLES.join(", "))))?,
    };
    if format == Format::GeoJson && !GEOJSON_TABLES.contains(&table.as_str()) {
        return Err(ApiError::bad_request(format!("GeoJSON is available for {}.", GEOJSON_TABLES.join(", "))));
    }
    let explicit_range = p.from.is_some() || p.to.is_some();
    let range = TimeRange::parse(p.from.as_deref(), p.to.as_deref(), Duration::hours(24), op_core::time::now())?;
    let scope = Scope { collar_ids: p.collar_id.clone().filter(|s| !s.is_empty()).map(|c| vec![c]), herd_id: p.herd_id.clone().filter(|s| !s.is_empty()) };
    let schema = table_schema(ctx.db(), source).await?;

    let filename = if is_telemetry(source) || (source == "acks" && explicit_range) {
        format!("{table}-{}-{}.{}", range.from.format("%Y%m%dT%H%M"), range.to.format("%Y%m%dT%H%M"), format.ext())
    } else {
        format!("{table}.{}", format.ext())
    };

    let (tx, rx) = mpsc::channel::<Result<Bytes, std::io::Error>>(8);
    let ctx2 = ctx.clone();
    let table2 = table.clone();
    tokio::spawn(async move {
        let batches = batches(&ctx2, source, range, explicit_range, scope);
        let res = match format {
            Format::Csv => write_csv(&schema.arrow, batches, &tx).await,
            Format::Parquet => write_parquet(&schema.arrow, batches, &tx).await,
            Format::GeoJson => write_geojson(&table2, batches, &tx).await,
        };
        if let Err(e) = res {
            tracing::warn!("export of {table2} failed: {e:#}");
            let _ = tx.send(Err(std::io::Error::other(e.to_string()))).await;
        }
    });
    let body = Body::from_stream(futures::stream::unfold(rx, |mut rx| async move { rx.recv().await.map(|item| (item, rx)) }));
    Ok((
        [
            (header::CONTENT_TYPE, format.mime().to_owned()),
            (header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\"")),
            (header::CACHE_CONTROL, "no-store".to_owned()),
        ],
        body,
    )
        .into_response())
}

/// Record batches for any exportable table. Telemetry streams from hot and
/// cold storage; the record tables are small and read from SQLite in one go.
fn batches(ctx: &Ctx, table: &'static str, range: TimeRange, explicit_range: bool, scope: Scope) -> mpsc::Receiver<anyhow::Result<RecordBatch>> {
    if is_telemetry(table) {
        return scan(ctx, table, range, scope, Source::All);
    }
    let (tx, rx) = mpsc::channel(4);
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let res: anyhow::Result<()> = async {
            let schema = table_schema(ctx.db(), table).await?;
            let mut sql = format!("SELECT {} FROM {table}", schema.select_list());
            let acks_range = table == "acks" && explicit_range;
            if acks_range {
                sql.push_str(" WHERE at >= ? AND at < ?");
            }
            if schema.has("id") {
                sql.push_str(" ORDER BY id");
            }
            let mut q = sqlx::query(&sql);
            if acks_range {
                q = q.bind(op_core::time::to_db(&range.from)).bind(op_core::time::to_db(&range.to));
            }
            let mut rows = q.fetch(ctx.db());
            let mut b = BatchBuilder::new(&schema);
            while let Some(r) = rows.try_next().await? {
                b.push(&r);
                if b.len() >= BATCH_ROWS && tx.send(Ok(b.finish()?)).await.is_err() {
                    return Ok(());
                }
            }
            if !b.is_empty() {
                let _ = tx.send(Ok(b.finish()?)).await;
            }
            Ok(())
        }
        .await;
        if let Err(e) = res {
            let _ = tx.send(Err(e)).await;
        }
    });
    rx
}

async fn send(tx: &Tx, bytes: Vec<u8>) -> anyhow::Result<()> {
    if bytes.is_empty() {
        return Ok(());
    }
    tx.send(Ok(Bytes::from(bytes))).await.map_err(|_| anyhow::anyhow!("client went away"))
}

fn csv_field(v: &Value, out: &mut String) {
    match v {
        Value::Null => {}
        Value::String(s) => {
            if s.contains([',', '"', '\n', '\r']) {
                out.push('"');
                out.push_str(&s.replace('"', "\"\""));
                out.push('"');
            } else {
                out.push_str(s);
            }
        }
        other => out.push_str(&other.to_string()),
    }
}

pub async fn write_csv(schema: &SchemaRef, mut rx: mpsc::Receiver<anyhow::Result<RecordBatch>>, tx: &Tx) -> anyhow::Result<()> {
    let mut out = String::new();
    for (i, f) in schema.fields().iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        csv_field(&Value::String(f.name().clone()), &mut out);
    }
    out.push('\n');
    send(tx, std::mem::take(&mut out).into_bytes()).await?;
    while let Some(b) = rx.recv().await {
        let b = b?;
        for row in 0..b.num_rows() {
            for (i, c) in b.columns().iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                csv_field(&cell_json(c.as_ref(), row), &mut out);
            }
            out.push('\n');
        }
        send(tx, std::mem::take(&mut out).into_bytes()).await?;
    }
    Ok(())
}

/// The writer's buffer is drained after every batch, so row groups go out as
/// soon as they are complete; the footer goes last.
pub async fn write_parquet(schema: &SchemaRef, mut rx: mpsc::Receiver<anyhow::Result<RecordBatch>>, tx: &Tx) -> anyhow::Result<()> {
    let mut w = ArrowWriter::try_new(Vec::new(), schema.clone(), Some(writer_props()))?;
    while let Some(b) = rx.recv().await {
        w.write(&b?)?;
        let chunk = std::mem::take(w.inner_mut());
        send(tx, chunk).await?;
    }
    w.finish()?;
    let chunk = std::mem::take(w.inner_mut());
    send(tx, chunk).await
}

fn properties(b: &RecordBatch, row: usize, skip: &[&str]) -> Map<String, Value> {
    let mut props = Map::new();
    for (f, c) in b.schema().fields().iter().zip(b.columns()) {
        if !skip.contains(&f.name().as_str()) {
            props.insert(f.name().clone(), cell_json(c.as_ref(), row));
        }
    }
    props
}

pub async fn write_geojson(table: &str, mut rx: mpsc::Receiver<anyhow::Result<RecordBatch>>, tx: &Tx) -> anyhow::Result<()> {
    send(tx, br#"{"type":"FeatureCollection","features":["#.to_vec()).await?;
    let mut first = true;
    let mut push = |out: &mut Vec<u8>, feature: Value| {
        if !first {
            out.push(b',');
        }
        first = false;
        out.extend_from_slice(feature.to_string().as_bytes());
        out.push(b'\n');
    };
    // collar -> (animal, coordinates, times)
    let mut tracks: BTreeMap<String, (Option<String>, Vec<[f64; 2]>, Vec<i64>)> = BTreeMap::new();
    while let Some(b) = rx.recv().await {
        let b = b?;
        let mut out = Vec::new();
        let c = Cols::new(&b);
        match table {
            "tracks" => {
                let (collar, animal, t, lon, lat) = (c.str("collar_id"), c.str("animal_id"), c.i64("t"), c.f64("lon"), c.f64("lat"));
                for i in 0..b.num_rows() {
                    let (Some(id), Some(t), Some(x), Some(y)) = (get_str(collar, i), get_i64(t, i), get_f64(lon, i), get_f64(lat, i)) else { continue };
                    let e = tracks.entry(id.to_owned()).or_default();
                    if e.0.is_none() {
                        e.0 = get_str(animal, i).map(str::to_owned);
                    }
                    e.1.push([x, y]);
                    e.2.push(t);
                }
            }
            "fixes" | "cues" => {
                let (lon, lat) = (c.f64("lon"), c.f64("lat"));
                for i in 0..b.num_rows() {
                    let (Some(x), Some(y)) = (get_f64(lon, i), get_f64(lat, i)) else { continue };
                    let f =
                        json!({ "type": "Feature", "geometry": { "type": "Point", "coordinates": [x, y] }, "properties": properties(&b, i, &["lon", "lat"]) });
                    push(&mut out, f);
                }
            }
            _ => {
                let geom = c.str("geometry");
                for i in 0..b.num_rows() {
                    let Some(g) = get_str(geom, i).and_then(|s| serde_json::from_str::<Value>(s).ok()) else { continue };
                    let f = json!({ "type": "Feature", "geometry": g, "properties": properties(&b, i, &["geometry"]) });
                    push(&mut out, f);
                }
            }
        }
        send(tx, out).await?;
    }
    let mut out = Vec::new();
    for (collar, (animal, coords, times)) in tracks {
        let geometry =
            if coords.len() == 1 { json!({ "type": "Point", "coordinates": coords[0] }) } else { json!({ "type": "LineString", "coordinates": coords }) };
        let props = json!({
            "collar_id": collar,
            "animal_id": animal,
            "start": times.first().map(|t| op_core::time::to_db(&op_core::time::from_unix_ms(*t))),
            "end": times.last().map(|t| op_core::time::to_db(&op_core::time::from_unix_ms(*t))),
            "points": times.len(),
            "times": times.iter().map(|t| *t as f64 / 1000.0).collect::<Vec<_>>(),
        });
        push(&mut out, json!({ "type": "Feature", "geometry": geometry, "properties": props }));
    }
    out.extend_from_slice(b"]}\n");
    send(tx, out).await
}
