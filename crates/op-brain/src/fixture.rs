//! A small decision context in op-engine's shape, for tests only.

use serde_json::{Value, json};

fn square(lon: f64, lat: f64) -> Value {
    let d = 0.004;
    json!({ "type": "Polygon", "coordinates": [[[lon, lat], [lon + d, lat], [lon + d, lat + d], [lon, lat + d], [lon, lat]]] })
}

/// A herd in Home with short, trampled grass; Creek rested 34 days, Ridge 12.
pub fn context() -> Value {
    json!({
        "as_of": "2026-09-26T12:00:00Z",
        "farm": { "id": "farm_test", "name": "Test farm", "timezone": "America/Chicago", "center": [-92.405, 38.125], "created_at": "2026-09-01T00:00:00Z" },
        "herd": { "id": "herd_test", "name": "Cows", "species": "cattle", "count": 24, "paddock_id": "pad_home", "autonomy": "propose", "timer_minutes": 60, "created_at": "2026-09-01T00:00:00Z", "animal_units": 24.0 },
        "autonomy": { "mode": "propose", "timer_minutes": 60 },
        "current_paddock_id": "pad_home",
        "position_source": "collar",
        "paddocks": [
            { "id": "pad_home", "name": "Home", "status": "grazing", "area_ha": 3.5, "geometry": square(-92.41, 38.12), "rest_days": null, "grazed_until": "2026-09-27T18:00:00Z" },
            { "id": "pad_creek", "name": "Creek", "status": "resting", "area_ha": 3.5, "geometry": square(-92.406, 38.12), "rest_days": 34.0 },
            { "id": "pad_ridge", "name": "Ridge", "status": "resting", "area_ha": 3.5, "geometry": square(-92.41, 38.124), "rest_days": 12.0 }
        ],
        "candidate_paddock_ids": ["pad_creek", "pad_ridge"],
        "collars": { "count": 3, "reporting_24h": 3, "fixes_24h": 900, "cues_24h": 4, "dominant_paddock_id": "pad_home", "paddock_fix_counts": { "pad_home": 870, "pad_creek": 30 } },
        "signals": {
            "rest_days": { "pad_home": null, "pad_creek": 34.0, "pad_ridge": 12.0 },
            "forage": {},
            "risk_flags": []
        },
        "observations": [
            { "content": "Grass is short and trampled near the water.", "paddock_id": "pad_home", "source": "field", "at": "2026-09-26T07:30:00Z" }
        ],
        "knowledge": [],
        "history": []
    })
}
