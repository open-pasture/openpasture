//! What a collar can hold, by firmware generation (protocol v1, §3.1).
//!
//! A collar reports its limits in `device.limits`; one that reports no `caps`
//! at all is [`CollarLimits::LEGACY`] (firmware 0.1: one ring of 64 corners).

use serde::{Deserialize, Serialize};

/// Vertex, hole and slot budgets of one collar.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CollarLimits {
    /// Most vertices in the outer ring.
    pub outer: usize,
    /// Most holes.
    pub holes: usize,
    /// Most vertices in one hole.
    pub hole_vertices: usize,
    /// Most vertices over every ring.
    pub total: usize,
    /// Boundaries held at once: the active one plus staged ones.
    pub slots: usize,
    /// Bytes available for every held slot record; 0 = unknown (no byte limit).
    pub slot_bytes: usize,
}

impl CollarLimits {
    /// Firmware 0.1, no `caps` reported.
    pub const LEGACY: Self = Self { outer: 64, holes: 0, hole_vertices: 0, total: 64, slots: 2, slot_bytes: 0 };
    /// nRF9151 with slots in internal flash (firmware 0.2).
    pub const V0: Self = Self { outer: 128, holes: 16, hole_vertices: 32, total: 384, slots: 16, slot_bytes: 24_576 };
    /// V0 plus 16 MB NOR flash for slots.
    pub const V1: Self = Self { outer: 128, holes: 16, hole_vertices: 32, total: 384, slots: 32, slot_bytes: 262_144 };

    /// Size of a slot record header in flash.
    pub const RECORD_HEADER_BYTES: usize = 192;
    /// Flash bytes per vertex: int32 lon and lat, × 1e7.
    pub const BYTES_PER_VERTEX: usize = 8;

    /// Flash bytes one slot record takes for a boundary of `total_vertices`
    /// vertices over all its rings.
    pub fn record_bytes(total_vertices: usize) -> usize {
        Self::RECORD_HEADER_BYTES + Self::BYTES_PER_VERTEX * total_vertices
    }

    /// Field-wise minimum, for the strictest limits of a herd. A `slot_bytes`
    /// of 0 means unknown, so the other side's value is kept.
    pub fn min(&self, other: &Self) -> Self {
        let bytes = match (self.slot_bytes, other.slot_bytes) {
            (0, b) | (b, 0) => b,
            (a, b) => a.min(b),
        };
        Self {
            outer: self.outer.min(other.outer),
            holes: self.holes.min(other.holes),
            hole_vertices: self.hole_vertices.min(other.hole_vertices),
            total: self.total.min(other.total),
            slots: self.slots.min(other.slots),
            slot_bytes: bytes,
        }
    }

    /// Whether a shape of `outer` vertices and holes of `holes` vertices each
    /// fits these limits by count.
    pub fn fits(&self, outer: usize, holes: &[usize]) -> bool {
        outer <= self.outer && holes.len() <= self.holes && holes.iter().all(|h| *h <= self.hole_vertices) && outer + holes.iter().sum::<usize>() <= self.total
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_bytes_and_budget() {
        assert_eq!(CollarLimits::record_bytes(0), 192);
        assert_eq!(CollarLimits::record_bytes(384), 3_264);
        // Seven worst-case records fit V0's slot bytes, eight don't.
        assert!(7 * CollarLimits::record_bytes(384) <= CollarLimits::V0.slot_bytes);
        assert!(8 * CollarLimits::record_bytes(384) > CollarLimits::V0.slot_bytes);
    }

    #[test]
    fn serializes_as_the_device_reports_it() {
        let json = serde_json::to_string(&CollarLimits::V0).unwrap();
        assert_eq!(json, r#"{"outer":128,"holes":16,"hole_vertices":32,"total":384,"slots":16,"slot_bytes":24576}"#);
        assert_eq!(serde_json::from_str::<CollarLimits>(&json).unwrap(), CollarLimits::V0);
    }

    #[test]
    fn min_is_field_wise_and_keeps_known_bytes() {
        let small = CollarLimits { outer: 100, holes: 20, hole_vertices: 8, total: 400, slots: 4, slot_bytes: 0 };
        let m = CollarLimits::V0.min(&small);
        assert_eq!(m, CollarLimits { outer: 100, holes: 16, hole_vertices: 8, total: 384, slots: 4, slot_bytes: 24_576 });
        assert_eq!(CollarLimits::V1.min(&CollarLimits::V0), CollarLimits::V0);
    }

    #[test]
    fn fits_by_count() {
        assert!(CollarLimits::V0.fits(128, &[32; 8]));
        assert!(!CollarLimits::V0.fits(129, &[]));
        assert!(!CollarLimits::V0.fits(4, &[33]));
        assert!(!CollarLimits::V0.fits(4, &[4; 17]));
        assert!(!CollarLimits::V0.fits(128, &[32; 9]), "total 416 > 384");
        assert!(!CollarLimits::LEGACY.fits(4, &[4]));
    }
}
