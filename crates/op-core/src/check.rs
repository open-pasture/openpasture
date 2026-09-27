//! Findings: one short sentence about a boundary (or anything else checked),
//! with where to draw it and what it is about.
//!
//! Pre-send codes (`docs/API.md`, pre-send check): `no_water`, `water_inside`,
//! `forage_short`, `area_per_head_low`, `overlaps_exclusion`,
//! `overlaps_hazard`, `crosses_farm_boundary`, `crosses_road`,
//! `crosses_neighbour_line`, `weak_coverage`, `collars_offline`,
//! `collars_no_holes`, `rested_short`, `animals_in_new_holes`,
//! `animals_outside`, `simplified`, `slots_full`.
//!
//! @F — what each means and how severe it is (op-ingest `prepare.rs` finds
//! the map and collar ones, op-engine `presend.rs` the forage, rest and GPS
//! ones; `geometry` is what the map draws):
//! - critical: `crosses_road` (the road inside the shape).
//! - warning: `no_water` (the farm has water mapped, none inside),
//!   `forage_short` (grass for under half a day), `area_per_head_low` (under
//!   25 m²/hd for cattle, 4 for sheep and goats), `overlaps_exclusion` when
//!   the exclusion covers all of it and can't be kept out,
//!   `overlaps_hazard` (the overlap), `crosses_neighbour_line` (the line
//!   inside), `crosses_farm_boundary` (the part past it), `weak_coverage`
//!   (10 m cells inside with median accuracy ≥ 5 m or under 90 % of fixes
//!   over the last week, clipped to the shape), `collars_offline` (no report
//!   for 20 min; their last fixes), `collars_no_holes`, `rested_short`
//!   (under 21 days since last grazed, for ground the herd isn't on),
//!   `animals_in_new_holes` and `animals_outside` (their positions; info
//!   when a previewed sweep walks them in), `slots_full` (a staged boundary
//!   some collars have no room for yet).
//! - info: `water_inside` (the water), `overlaps_exclusion` when kept out
//!   (the overlap), `simplified`.

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
