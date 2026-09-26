//! Geometry shared by openpasture and OpenCollar.
//!
//! - [`ring`]: cleaning, area, point in ring, self-intersection, validation.
//! - [`clip`]: union and difference of simple rings (Greiner-Hormann), ported
//!   from the agent kit's `collars/rings.py`.
//! - [`geofence`] and [`cue`]: the firmware's fence and cue logic, ported from
//!   `opencollar/firmware/src/geofence.c` and `cue.c`.
//! - [`polygon`]: the GeoJSON Polygon type the app stores and serves.
//!
//! Coordinates are `[longitude, latitude]`, WGS 84, as in GeoJSON.

pub mod clip;
pub mod cue;
pub mod geofence;
pub mod polygon;
pub mod projection;
pub mod ring;

pub use clip::{RingError, subtract_ring, union_rings};
pub use cue::{Cue, CueCommand, CueConfig};
pub use geofence::{Geofence, GeofenceConfig, GeofenceResult, GeofenceState};
pub use polygon::{Polygon, PolygonType};
pub use projection::Projection;
pub use ring::{clean_ring, point_in_ring, ring_is_simple, signed_area, validate_ring};

/// `[longitude, latitude]` in degrees.
pub type LonLat = [f64; 2];

/// Most vertices a V0 collar holds (firmware `GEOFENCE_MAX_VERTICES`).
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
}
