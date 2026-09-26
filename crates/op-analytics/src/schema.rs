//! SQLite tables as Arrow: schemas read from `PRAGMA table_info` (so columns
//! other crates add later show up), rows to record batches, and Arrow cells
//! back to JSON.

use std::sync::Arc;

use anyhow::Context;
use datafusion::arrow::array::{
    Array, ArrayRef, AsArray, BooleanArray, Float64Array, Float64Builder, Int64Array, Int64Builder, StringArray, StringBuilder, new_null_array,
};
use datafusion::arrow::compute::cast;
use datafusion::arrow::datatypes::{
    DataType, Field, Float32Type, Float64Type, Int8Type, Int16Type, Int32Type, Int64Type, Schema, SchemaRef, UInt8Type, UInt16Type, UInt32Type, UInt64Type,
};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::arrow::util::display::{ArrayFormatter, FormatOptions};
use serde_json::{Number, Value};
use sqlx::sqlite::SqliteRow;
use sqlx::{Row, SqlitePool};

/// Tables with a `t` column that roll over to Parquet.
pub const TELEMETRY_TABLES: [&str; 2] = ["fixes", "cues"];

/// Tables the SQL console and export can read.
pub const SQL_TABLES: [&str; 9] = ["fixes", "cues", "acks", "boundaries", "decisions", "collars", "animals", "paddocks", "herds"];

/// Columns never exposed.
const HIDDEN: &[(&str, &str)] = &[("collars", "key_hash")];

pub fn is_telemetry(table: &str) -> bool {
    TELEMETRY_TABLES.contains(&table)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int,
    Real,
    Text,
}

impl Kind {
    /// SQLite affinity rules, roughly.
    fn from_decl(decl: &str) -> Kind {
        let d = decl.to_ascii_uppercase();
        if d.contains("INT") {
            Kind::Int
        } else if d.contains("REAL") || d.contains("FLOA") || d.contains("DOUB") {
            Kind::Real
        } else {
            Kind::Text
        }
    }

    fn data_type(self) -> DataType {
        match self {
            Kind::Int => DataType::Int64,
            Kind::Real => DataType::Float64,
            Kind::Text => DataType::Utf8,
        }
    }
}

#[derive(Debug, Clone)]
pub struct TableSchema {
    pub table: String,
    pub columns: Vec<(String, Kind)>,
    pub arrow: SchemaRef,
}

impl TableSchema {
    /// `"a", "b", …` for a SELECT.
    pub fn select_list(&self) -> String {
        self.columns.iter().map(|(c, _)| format!("\"{c}\"")).collect::<Vec<_>>().join(", ")
    }

    pub fn has(&self, column: &str) -> bool {
        self.columns.iter().any(|(c, _)| c == column)
    }
}

/// Schema of a whitelisted table. Every field is nullable so Parquet files
/// written before a column existed still read.
pub async fn table_schema(pool: &SqlitePool, table: &str) -> anyhow::Result<TableSchema> {
    anyhow::ensure!(SQL_TABLES.contains(&table), "unknown table {table}");
    let rows = sqlx::query(&format!("PRAGMA table_info(\"{table}\")")).fetch_all(pool).await?;
    let mut columns = Vec::with_capacity(rows.len());
    for r in &rows {
        let name: String = r.try_get("name")?;
        if HIDDEN.contains(&(table, name.as_str())) {
            continue;
        }
        let decl: String = r.try_get("type")?;
        columns.push((name, Kind::from_decl(&decl)));
    }
    anyhow::ensure!(!columns.is_empty(), "table {table} has no columns");
    let fields: Vec<Field> = columns.iter().map(|(n, k)| Field::new(n, k.data_type(), true)).collect();
    Ok(TableSchema { table: table.to_owned(), columns, arrow: Arc::new(Schema::new(fields)) })
}

enum ColBuilder {
    Int(Int64Builder),
    Real(Float64Builder),
    Text(StringBuilder),
}

/// Collects SQLite rows (selected with [`TableSchema::select_list`]) into
/// record batches.
pub struct BatchBuilder {
    schema: TableSchema,
    cols: Vec<ColBuilder>,
    len: usize,
}

impl BatchBuilder {
    pub fn new(schema: &TableSchema) -> Self {
        let cols = schema
            .columns
            .iter()
            .map(|(_, k)| match k {
                Kind::Int => ColBuilder::Int(Int64Builder::new()),
                Kind::Real => ColBuilder::Real(Float64Builder::new()),
                Kind::Text => ColBuilder::Text(StringBuilder::new()),
            })
            .collect();
        Self { schema: schema.clone(), cols, len: 0 }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Unchecked decodes coerce like SQLite does (an integer in a REAL
    /// column reads as a float).
    pub fn push(&mut self, row: &SqliteRow) {
        for (i, c) in self.cols.iter_mut().enumerate() {
            match c {
                ColBuilder::Int(b) => b.append_option(row.try_get_unchecked::<Option<i64>, _>(i).ok().flatten()),
                ColBuilder::Real(b) => b.append_option(row.try_get_unchecked::<Option<f64>, _>(i).ok().flatten()),
                ColBuilder::Text(b) => b.append_option(row.try_get_unchecked::<Option<String>, _>(i).ok().flatten()),
            }
        }
        self.len += 1;
    }

    pub fn finish(&mut self) -> anyhow::Result<RecordBatch> {
        let arrays: Vec<ArrayRef> = self
            .cols
            .iter_mut()
            .map(|c| -> ArrayRef {
                match c {
                    ColBuilder::Int(b) => Arc::new(b.finish()),
                    ColBuilder::Real(b) => Arc::new(b.finish()),
                    ColBuilder::Text(b) => Arc::new(b.finish()),
                }
            })
            .collect();
        self.len = 0;
        RecordBatch::try_new(self.schema.arrow.clone(), arrays).context("building record batch")
    }
}

/// Reshape a batch to `schema`: columns by name, cast when the type
/// differs, nulls for columns the batch lacks.
pub fn normalize(batch: &RecordBatch, schema: &SchemaRef) -> anyhow::Result<RecordBatch> {
    if batch.schema().fields() == schema.fields() {
        return Ok(batch.clone());
    }
    let mut arrays = Vec::with_capacity(schema.fields().len());
    for f in schema.fields() {
        let a = match batch.column_by_name(f.name()) {
            Some(a) if a.data_type() == f.data_type() => a.clone(),
            Some(a) => cast(a, f.data_type())?,
            None => new_null_array(f.data_type(), batch.num_rows()),
        };
        arrays.push(a);
    }
    Ok(RecordBatch::try_new(schema.clone(), arrays)?)
}

/// Typed column access on a normalized batch.
pub struct Cols<'a> {
    batch: &'a RecordBatch,
}

impl<'a> Cols<'a> {
    pub fn new(batch: &'a RecordBatch) -> Self {
        Self { batch }
    }
    pub fn i64(&self, name: &str) -> Option<&'a Int64Array> {
        self.batch.column_by_name(name).and_then(|a| a.as_primitive_opt::<Int64Type>())
    }
    pub fn f64(&self, name: &str) -> Option<&'a Float64Array> {
        self.batch.column_by_name(name).and_then(|a| a.as_primitive_opt::<Float64Type>())
    }
    pub fn str(&self, name: &str) -> Option<&'a StringArray> {
        self.batch.column_by_name(name).and_then(|a| a.as_string_opt::<i32>())
    }
}

pub fn get_i64(a: Option<&Int64Array>, i: usize) -> Option<i64> {
    a.filter(|a| a.is_valid(i)).map(|a| a.value(i))
}
pub fn get_f64(a: Option<&Float64Array>, i: usize) -> Option<f64> {
    a.filter(|a| a.is_valid(i)).map(|a| a.value(i)).filter(|v| v.is_finite())
}
pub fn get_str(a: Option<&StringArray>, i: usize) -> Option<&str> {
    a.filter(|a| a.is_valid(i)).map(|a| a.value(i))
}

fn num_f64(v: f64) -> Value {
    Number::from_f64(v).map(Value::Number).unwrap_or(Value::Null)
}

/// One Arrow cell as JSON: numbers stay numbers, NaN and infinities become
/// null, everything else is its display string.
pub fn cell_json(a: &dyn Array, i: usize) -> Value {
    if a.is_null(i) {
        return Value::Null;
    }
    match a.data_type() {
        DataType::Null => Value::Null,
        DataType::Boolean => Value::Bool(a.as_any().downcast_ref::<BooleanArray>().map(|b| b.value(i)).unwrap_or(false)),
        DataType::Int8 => a.as_primitive::<Int8Type>().value(i).into(),
        DataType::Int16 => a.as_primitive::<Int16Type>().value(i).into(),
        DataType::Int32 => a.as_primitive::<Int32Type>().value(i).into(),
        DataType::Int64 => a.as_primitive::<Int64Type>().value(i).into(),
        DataType::UInt8 => a.as_primitive::<UInt8Type>().value(i).into(),
        DataType::UInt16 => a.as_primitive::<UInt16Type>().value(i).into(),
        DataType::UInt32 => a.as_primitive::<UInt32Type>().value(i).into(),
        DataType::UInt64 => a.as_primitive::<UInt64Type>().value(i).into(),
        DataType::Float32 => num_f64(a.as_primitive::<Float32Type>().value(i) as f64),
        DataType::Float64 => num_f64(a.as_primitive::<Float64Type>().value(i)),
        DataType::Utf8 => Value::String(a.as_string::<i32>().value(i).to_owned()),
        DataType::LargeUtf8 => Value::String(a.as_string::<i64>().value(i).to_owned()),
        DataType::Utf8View => Value::String(a.as_string_view().value(i).to_owned()),
        _ => {
            let opts = FormatOptions::default();
            match ArrayFormatter::try_new(a, &opts) {
                Ok(f) => Value::String(f.value(i).to_string()),
                Err(_) => Value::Null,
            }
        }
    }
}
