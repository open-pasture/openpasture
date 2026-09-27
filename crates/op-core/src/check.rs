//! Findings: one short sentence about a boundary (or anything else checked),
//! with where to draw it and what it is about.
//!
//! Pre-send codes (`docs/API.md`, pre-send check): `no_water`, `water_inside`,
//! `forage_short`, `area_per_head_low`, `overlaps_exclusion`,
//! `overlaps_hazard`, `crosses_farm_boundary`, `crosses_road`,
//! `crosses_neighbour_line`, `weak_coverage`, `collars_offline`,
//! `collars_no_holes`, `rested_short`, `animals_in_new_holes`,
//! `animals_outside`, `simplified`, `slots_full`.

use serde::{Deserialize, Serialize};

use crate::severity::Severity;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Finding {
    /// snake_case, e.g. `no_water`.
    pub code: String,
    pub severity: Severity,
    /// One short sentence, e.g. "No water inside".
    pub text: String,
    /// GeoJSON geometry to draw.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<serde_json::Value>,
    /// `("collar", id)`, `("feature", id)`, …
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targets: Vec<(String, String)>,
}
