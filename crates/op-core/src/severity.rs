//! How much something matters: alerts and pre-send findings share it.

use serde::{Deserialize, Serialize};

use crate::domain::DbEnum;

/// Ordered: `Info < Warning < Critical`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl DbEnum for Severity {}
