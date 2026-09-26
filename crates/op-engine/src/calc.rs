//! Grazing calculations, ported from the kit's `calculations.py`. Pure
//! functions that turn land and collar data into signals. The numbers are
//! rules of thumb for a first read, not measurements. Output shapes and
//! rounding match the kit so both give the same answers on the same data.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

pub const ACRES_PER_HECTARE: f64 = 2.47105;
/// About 300 lb of dry matter per acre per inch in a reasonably dense stand.
pub const KG_DM_PER_HA_PER_INCH: f64 = 336.0;
/// A 1,000 lb animal unit eats about 2.6% of body weight a day.
pub const DEFAULT_INTAKE_KG_DM_PER_AU_DAY: f64 = 11.8;
pub const DEFAULT_RESIDUAL_INCHES: f64 = 3.0;
pub const DEFAULT_UTILIZATION: f64 = 0.6;

/// Python's `round(x, n)`: correctly rounded, ties to even.
pub fn round(x: f64, digits: usize) -> f64 {
    format!("{x:.digits$}").parse().unwrap_or(x)
}

fn per_head(species: &str) -> f64 {
    match species.trim().to_lowercase().as_str() {
        "cattle" | "cow" | "beef" | "bison" => 1.0,
        "dairy" => 1.4,
        "horse" | "horses" => 1.25,
        "sheep" => 0.2,
        "goat" | "goats" => 0.15,
        _ => 1.0,
    }
}

/// Herd size in animal units. Uses the farmer's number when given.
pub fn animal_units(species: &str, count: i64, stated: Option<f64>) -> f64 {
    if let Some(s) = stated {
        return s;
    }
    round(per_head(species) * count.max(0) as f64, 2)
}

/// Animal-days and animal-days per acre for each paddock, from collar fix
/// counts. Assumes collared animals move like the rest of the herd and fixes
/// are spread evenly in time.
pub fn grazing_pressure(fix_counts: &BTreeMap<String, i64>, herd_count: i64, window_days: f64, area_ha: &BTreeMap<String, Option<f64>>) -> Map<String, Value> {
    let total: i64 = fix_counts.values().filter(|c| **c > 0).sum();
    let mut out = Map::new();
    if total == 0 {
        return out;
    }
    for (id, count) in fix_counts {
        let share = *count as f64 / total as f64;
        let days = herd_count as f64 * window_days * share;
        let acres = area_ha.get(id).copied().flatten().filter(|h| *h != 0.0).map(|h| h * ACRES_PER_HECTARE);
        out.insert(
            id.clone(),
            json!({
                "share_of_time": round(share, 4),
                "animal_days": round(days, 2),
                "animal_days_per_acre": acres.map(|a| round(days / a, 2)),
            }),
        );
    }
    out
}

/// Days since each paddock was last grazed; `None` when there is no record.
pub fn rest_days(last_grazed: &BTreeMap<String, Option<DateTime<Utc>>>, as_of: DateTime<Utc>) -> BTreeMap<String, Option<f64>> {
    last_grazed
        .iter()
        .map(|(id, at)| {
            let days = at.map(|at| {
                let secs = (as_of - at).num_milliseconds() as f64 / 1000.0;
                round(secs.max(0.0) / 86_400.0, 1)
            });
            (id.clone(), days)
        })
        .collect()
}

/// Standing forage above the residual target, kg DM/ha. A measured height
/// wins; otherwise NDVI maps linearly from 0.2 (bare) to 0.8 (~10 inches),
/// always low confidence.
pub fn forage_estimate(ndvi_mean: Option<f64>, height_inches: Option<f64>, residual_target_inches: f64) -> Value {
    let (height, source, confidence) = match (height_inches, ndvi_mean) {
        (Some(h), _) => (h, "farmer", "medium"),
        (None, Some(n)) => (((n - 0.2) / 0.6 * 10.0).max(0.0), "imagery", "low"),
        (None, None) => {
            return json!({ "height_inches": null, "available_kg_dm_per_ha": null, "source": null, "confidence": null });
        }
    };
    let available = (height - residual_target_inches).max(0.0) * KG_DM_PER_HA_PER_INCH;
    json!({
        "height_inches": round(height, 1),
        "available_kg_dm_per_ha": round(available, 0),
        "residual_target_inches": residual_target_inches,
        "source": source,
        "confidence": confidence,
    })
}

/// NDVI slope per week by least squares: recovering, flat or declining.
pub fn recovery_trend(history: &[(DateTime<Utc>, f64)]) -> Value {
    let mut points = history.to_vec();
    points.sort_by_key(|p| p.0);
    let n = points.len();
    let unknown = json!({ "trend": "unknown", "ndvi_slope_per_week": null, "readings": n });
    if n < 2 {
        return unknown;
    }
    let origin = points[0].0;
    let xs: Vec<f64> = points.iter().map(|(at, _)| (*at - origin).num_milliseconds() as f64 / 1000.0 / 604_800.0).collect();
    let ys: Vec<f64> = points.iter().map(|p| p.1).collect();
    let mx = xs.iter().sum::<f64>() / n as f64;
    let my = ys.iter().sum::<f64>() / n as f64;
    let den: f64 = xs.iter().map(|x| (x - mx).powi(2)).sum();
    if den == 0.0 {
        return unknown;
    }
    let slope = xs.iter().zip(&ys).map(|(x, y)| (x - mx) * (y - my)).sum::<f64>() / den;
    let trend = if slope > 0.01 {
        "recovering"
    } else if slope < -0.01 {
        "declining"
    } else {
        "flat"
    };
    json!({ "trend": trend, "ndvi_slope_per_week": round(slope, 4), "readings": n })
}

/// Days of grazing the forage supports. Only `utilization` of standing forage
/// counts as eaten; regrowth offsets demand. `None` with no demand, capped at 365.
pub fn feed_budget_days(available_kg_dm: f64, herd_animal_units: f64, intake: f64, utilization: f64, daily_growth_kg_dm: f64) -> Option<f64> {
    let demand = herd_animal_units * intake;
    if demand <= 0.0 {
        return None;
    }
    let net = demand - daily_growth_kg_dm.max(0.0);
    if net <= 0.0 {
        return Some(365.0);
    }
    Some(round((available_kg_dm.max(0.0) * utilization / net).min(365.0), 1))
}

fn rows<'a>(section: Option<&'a Value>, key: &str) -> Vec<&'a Map<String, Value>> {
    section.and_then(|s| s.get(key)).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_object).collect()).unwrap_or_default()
}

fn num(row: &Map<String, Value>, key: &str) -> Option<f64> {
    row.get(key).and_then(Value::as_f64)
}

fn flag(kind: &str, level: &str, reason: String) -> Value {
    json!({ "type": kind, "level": level, "reason": reason })
}

/// Python's `f"{x:.0f}"` (ties to even).
fn f0(x: f64) -> String {
    format!("{x:.0}")
}

/// Heat, cold, flood, wet-ground and drought flags from land report sections
/// (section name -> data, `None`/null when unavailable). Thresholds are for
/// cattle on pasture and deliberately conservative.
pub fn risk_flags(sections: &Map<String, Value>) -> Vec<Value> {
    let mut flags = Vec::new();
    let weather = sections.get("weather").filter(|v| v.is_object());
    let forecast: Vec<_> = rows(weather, "forecast").into_iter().take(3).collect();
    let history = rows(weather, "history");

    let highs: Vec<f64> = forecast.iter().filter_map(|r| num(r, "temp_max_c")).collect();
    let lows: Vec<f64> = forecast.iter().filter_map(|r| num(r, "temp_min_c")).collect();
    let rain: Vec<f64> = forecast.iter().map(|r| num(r, "precip_mm").unwrap_or(0.0)).collect();
    let max = |v: &[f64]| v.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let min = |v: &[f64]| v.iter().copied().fold(f64::INFINITY, f64::min);
    let rain_sum: f64 = rain.iter().sum();

    if !highs.is_empty() && max(&highs) >= 32.0 {
        let level = if max(&highs) >= 35.0 { "high" } else { "medium" };
        flags.push(flag("heat", level, format!("Highs up to {}C in the next 3 days. Shade and water matter.", f0(max(&highs)))));
    }
    if !lows.is_empty() && min(&lows) <= -12.0 {
        flags.push(flag("cold", "high", format!("Lows down to {}C in the next 3 days.", f0(min(&lows)))));
    } else if !lows.is_empty() && min(&lows) <= 0.0 && rain_sum >= 5.0 {
        flags.push(flag("cold", "medium", "Cold rain in the forecast. Watch for wind exposure.".into()));
    }

    let in_floodplain =
        sections.get("water").and_then(|w| w.get("floodplain")).and_then(|f| f.get("in_floodplain")).is_some_and(|v| v.as_bool().unwrap_or(!v.is_null()));
    if rain_sum >= 50.0 || (!rain.is_empty() && max(&rain) >= 25.0) {
        let level = if rain_sum >= 50.0 || in_floodplain { "high" } else { "medium" };
        let tail = if in_floodplain { " on ground in a floodplain." } else { "." };
        flags.push(flag("flood", level, format!("{} mm of rain forecast over 3 days{tail}", f0(rain_sum))));
    }

    let recent: f64 = history.iter().rev().take(3).map(|r| num(r, "precip_mm").unwrap_or(0.0)).sum();
    if recent >= 25.0 {
        flags.push(flag("wet_ground", "medium", format!("{} mm fell in the last 3 days. Low ground may pug.", f0(recent))));
    }

    let category = sections
        .get("climate")
        .and_then(|c| c.get("drought"))
        .filter(|d| d.is_object())
        .map(|d| match d.get("category") {
            Some(Value::String(s)) => s.to_lowercase(),
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_string().to_lowercase(),
        })
        .unwrap_or_default();
    if ["d2", "d3", "d4", "severe", "extreme", "exceptional"].contains(&category.as_str()) {
        flags.push(flag("drought", "high", format!("Drought category {}.", category.to_uppercase())));
    } else if ["d1", "moderate"].contains(&category.as_str()) {
        flags.push(flag("drought", "medium", format!("Drought category {}.", category.to_uppercase())));
    } else if history.len() >= 20 {
        let month = &history[history.len().saturating_sub(30)..];
        let precip: f64 = month.iter().map(|r| num(r, "precip_mm").unwrap_or(0.0)).sum();
        let et0: f64 = month.iter().map(|r| num(r, "et0_mm").unwrap_or(0.0)).sum();
        if precip < 15.0 && et0 > 60.0 {
            flags.push(flag("drought", "medium", format!("Only {} mm of rain against {} mm of evapotranspiration this month.", f0(precip), f0(et0))));
        }
    }
    flags
}

/// Cue pressure on the line. Collars don't report activity yet, so the
/// grazing and resting shares stay `None` until they do.
/// `fix_collars`: collar id per fix; `cues`: (collar id, margin_m).
pub fn behavior(fix_collars: &[String], cues: &[(String, Option<f64>)], window_days: f64) -> Value {
    let mut collars: std::collections::BTreeSet<&str> = fix_collars.iter().map(String::as_str).collect();
    collars.extend(cues.iter().map(|c| c.0.as_str()));
    let collar_days = collars.len() as f64 * window_days;
    json!({
        "grazing_share": null,
        "resting_share": null,
        "cue_count": cues.len(),
        "cues_per_collar_day": (collar_days > 0.0).then(|| round(cues.len() as f64 / collar_days, 2)),
        "cues_past_line": cues.iter().filter(|c| c.1.is_some_and(|m| m < 0.0)).count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_rounding() {
        assert_eq!(round(2.675, 2), 2.67);
        assert_eq!(round(0.125, 2), 0.12);
        assert_eq!(round(2.5, 0), 2.0);
        assert_eq!(round(3.5, 0), 4.0);
        assert_eq!(f0(36.5), "36");
    }
}
