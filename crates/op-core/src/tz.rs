//! The farm's time zone from its location, offline (tzf-rs bundles the
//! boundaries).

use std::sync::OnceLock;

use crate::LonLat;

static FINDER: OnceLock<tzf_rs::DefaultFinder> = OnceLock::new();

/// The IANA zone at `[lon, lat]`, or `None` where there is none (open sea
/// maps to `Etc/GMT±N`, which counts).
pub fn at(p: LonLat) -> Option<String> {
    let name = FINDER.get_or_init(tzf_rs::DefaultFinder::new).get_tz_name(p[0], p[1]);
    (!name.is_empty()).then(|| name.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn farms_get_their_own_zone() {
        assert_eq!(at([-79.2, 38.2]).as_deref(), Some("America/New_York")); // Swoope, Virginia
        assert_eq!(at([-87.6, 41.9]).as_deref(), Some("America/Chicago"));
        assert_eq!(at([174.8, -41.3]).as_deref(), Some("Pacific/Auckland"));
    }
}
