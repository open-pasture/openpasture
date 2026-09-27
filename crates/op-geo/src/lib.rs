//! Geometry shared by openpasture and OpenCollar.
//!
//! - [`ring`]: cleaning, area, point in ring, self-intersection, validation.
//! - [`clip`]: union and difference of simple rings (Greiner-Hormann), ported
//!   from the agent kit's `collars/rings.py`.
//! - [`geofence`] and [`cue`]: the firmware's fence and cue logic, ported from
//!   `opencollar/firmware/src/geofence.c` and `cue.c`, with holes, cue kinds,
//!   track mode and episodes (protocol v1).
//! - [`polygon`]: the GeoJSON Polygon type the app stores and serves.
//! - [`limits`]: what each collar generation holds.
//! - [`shape`]: the shape rules a collar checks, and fitting a shape to a
//!   collar's limits.
//! - [`exclude`]: exclusions cut, holed, joined or dropped on a boundary.
//!
//! Coordinates are `[longitude, latitude]`, WGS 84, as in GeoJSON.

pub mod clip;
pub mod cue;
pub mod exclude;
pub mod geofence;
pub mod limits;
pub mod polygon;
pub mod projection;
pub mod ring;
pub mod shape;

pub use clip::{RingError, subtract_ring, union_rings};
pub use cue::{Cue, CueCommand, CueConfig, CueKind, CueMode, Episode, EpisodeOutcome, EpisodeTracker};
pub use exclude::{Placement, Shaped, shape_target};
pub use geofence::{Geofence, GeofenceConfig, GeofenceResult, GeofenceState};
pub use limits::CollarLimits;
pub use polygon::{Polygon, PolygonType};
pub use projection::Projection;
pub use ring::{clean_ring, point_in_ring, ring_is_simple, signed_area, validate_ring};
pub use shape::ShapeCode;

/// `[longitude, latitude]` in degrees.
pub type LonLat = [f64; 2];

/// Most vertices a firmware 0.1 collar holds (`GEOFENCE_MAX_VERTICES`), the
/// same as [`CollarLimits::LEGACY`]`.outer`. Kept for callers that predate
/// [`CollarLimits`].
pub const MAX_COLLAR_VERTICES: usize = 64;

#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum GeoError {
    #[error("The shape needs at least 3 corners.")]
    TooFewVertices,
    #[error("The shape has {0} corners. Collars hold at most {1}; simplify the shape.")]
    TooManyVertices(usize, usize),
    #[error("The shape has a coordinate out of range.")]
    OutOfRange,
    #[error("The shape crosses itself.")]
    SelfIntersecting,
    #[error("The shape has no area.")]
    ZeroArea,
    #[error("The shape must be a Polygon with at least one ring.")]
    Empty,
    #[error("The shape has {0} holes. Collars hold at most {1}.")]
    TooManyHoles(usize, usize),
}
