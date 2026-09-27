//! The shape every report shares (contract §2.16): a header block, tables,
//! method notes and signature lines. Values are in the farm's units; each
//! column's `unit` says which.

use chrono::{DateTime, NaiveDate, Utc};
use op_core::Ctx;
use serde::{Deserialize, Serialize};

/// One column of a table. `unit` comes from `op_core::units` ("ac", "lb",
/// "AU/ac"), or names a currency or a percentage; counts and days have none.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Column {
    pub key: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unit: Option<String>,
}

impl Column {
    pub fn new(key: &str, label: &str) -> Self {
        Self { key: key.into(), label: label.into(), unit: None }
    }

    pub fn unit(key: &str, label: &str, unit: impl Into<String>) -> Self {
        Self { key: key.into(), label: label.into(), unit: Some(unit.into()) }
    }
}

/// A titled table. Every row has one value per column; `totals` too.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportSection {
    pub title: String,
    pub columns: Vec<Column>,
    pub rows: Vec<Vec<serde_json::Value>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub totals: Option<Vec<serde_json::Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportDoc {
    pub id: String,
    pub title: String,
    pub farm: String,
    pub from: NaiveDate,
    pub to: NaiveDate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
    pub generated_at: DateTime<Utc>,
    /// Farm, Operator, FSA farm, Dates, Herd: only those known.
    pub header: Vec<(String, String)>,
    pub sections: Vec<ReportSection>,
    /// Method lines.
    pub notes: Vec<String>,
    /// Signature-line labels.
    pub signatures: Vec<String>,
}

/// `from` and `to` are farm-local days, both included.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReportParams {
    pub from: NaiveDate,
    pub to: NaiveDate,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
}

#[async_trait::async_trait]
pub trait Report: Send + Sync {
    fn id(&self) -> &'static str;
    fn title(&self) -> &'static str;
    async fn build(&self, ctx: &Ctx, p: &ReportParams) -> anyhow::Result<ReportDoc>;
}
