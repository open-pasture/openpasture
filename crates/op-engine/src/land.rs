//! Land reports: what is known about a paddock. Ported from the kit's
//! `land/`. With a `firecrawl_api_key` secret the report comes from Alexandria
//! (`alexandria.py`); without it, the open-data fallback (`open_data.py`)
//! fills weather from Open-Meteo and marks every other section unavailable.
//! Reports are cached in SQLite (`land_reports`) and reused for six hours.
//!
//! A report is JSON in the kit's contract shape:
//! `{ report_id, paddock_id, source, as_of, geometry, sections: { name: { status: "ok", ...data, sources } | { status: "unavailable", reason } } }`.

use std::time::Duration;

use chrono::{DateTime, Utc};
use op_core::{Ctx, Paddock, Polygon, time};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use sqlx::Row;

pub const MAX_AGE_HOURS: i64 = 6;
pub const SECRET: &str = "firecrawl_api_key";
const ALEXANDRIA_URL: &str = "https://api.firecrawl.dev/v2/scrape";
const OPEN_METEO_URL: &str = "https://api.open-meteo.com/v1/forecast";
const NEEDS_ALEXANDRIA: &str = "Only available through Alexandria. Add a Firecrawl API key to enable it.";
/// Snow deeper than this hides the grass from imagery: forage only from a measured height.
pub const SNOW_DEPTH_CM: f64 = 2.0;
/// A mean air temperature below this over the last [`DORMANT_DAYS`] days: grass is dormant.
pub const DORMANT_MEAN_C: f64 = 5.0;
pub const DORMANT_DAYS: usize = 7;

/// The sections a decision asks for (kit `DEFAULT_DECISION_SECTIONS`).
pub fn decision_sections() -> Value {
    json!({
        "weather": { "history_days": 30, "forecast_days": 7 },
        "imagery": { "latest": true, "history_days": 90, "products": ["ndvi"] },
        "climate": { "include": ["drought"] },
        "water": { "include": ["floodplain"] },
    })
}

/// Section data when `status` is ok, without status/sources/reason.
pub fn ok_section<'a>(report: &'a Value, name: &str) -> Option<Map<String, Value>> {
    let s = report.get("sections")?.get(name)?.as_object()?;
    if s.get("status").and_then(Value::as_str) != Some("ok") {
        return None;
    }
    Some(s.iter().filter(|(k, _)| !matches!(k.as_str(), "status" | "sources" | "reason")).map(|(k, v)| (k.clone(), v.clone())).collect())
}

/// All ok sections as name -> data (null for unavailable), for `calc::risk_flags`.
pub fn section_data(report: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    if let Some(sections) = report.get("sections").and_then(Value::as_object) {
        for name in sections.keys() {
            out.insert(name.clone(), ok_section(report, name).map(Value::Object).unwrap_or(Value::Null));
        }
    }
    out
}

fn round6(v: &Value) -> Value {
    match v {
        Value::Number(n) if n.is_f64() => json!(crate::calc::round(n.as_f64().unwrap_or_default(), 6)),
        Value::Array(a) => Value::Array(a.iter().map(round6).collect()),
        other => other.clone(),
    }
}

/// Key over geometry and requested sections (kit `LandReportRequest.cache_key`).
pub fn cache_key(geometry: &Polygon, sections: &Value) -> String {
    let geometry = serde_json::to_value(geometry).unwrap_or_default();
    let geometry = json!({ "type": geometry["type"], "coordinates": round6(&geometry["coordinates"]) });
    let material = json!({ "buffer_meters": null, "geometry": geometry, "sections": sections }).to_string();
    let digest = Sha256::digest(material.as_bytes());
    digest.iter().take(16).map(|b| format!("{b:02x}")).collect()
}

fn unavailable(reason: impl Into<String>) -> Value {
    json!({ "status": "unavailable", "reason": reason.into() })
}

fn http() -> reqwest::Client {
    reqwest::Client::builder().timeout(Duration::from_secs(60)).build().unwrap_or_default()
}

/// Latest cached report for the paddock, any age.
pub async fn latest(ctx: &Ctx, paddock_id: &str) -> anyhow::Result<Option<Value>> {
    let row = sqlx::query("SELECT report FROM land_reports WHERE paddock_id = ? ORDER BY as_of DESC LIMIT 1").bind(paddock_id).fetch_optional(ctx.db()).await?;
    Ok(row.map(|r| serde_json::from_str(&r.get::<String, _>(0))).transpose()?)
}

/// The report for a paddock: a cached one within six hours, else a fresh fetch
/// (saved). Returns `(report, from_cache)`. Never fails for provider errors;
/// those come back as unavailable sections.
pub async fn report_for_paddock(ctx: &Ctx, paddock: &Paddock, refresh: bool) -> anyhow::Result<(Value, bool)> {
    let sections = decision_sections();
    let key = cache_key(&paddock.geometry, &sections);
    let now = time::now();
    if !refresh {
        let since = time::to_db(&(now - chrono::Duration::hours(MAX_AGE_HOURS)));
        let row = sqlx::query("SELECT report FROM land_reports WHERE cache_key = ? AND as_of >= ? ORDER BY as_of DESC LIMIT 1")
            .bind(&key)
            .bind(since)
            .fetch_optional(ctx.db())
            .await?;
        if let Some(r) = row {
            let mut report: Value = serde_json::from_str(&r.get::<String, _>(0))?;
            report["paddock_id"] = json!(paddock.id);
            return Ok((report, true));
        }
    }
    let api_key = ctx.secrets().get(SECRET).ok().flatten().filter(|k| !k.trim().is_empty());
    let (source, sections_out, report_id) = match api_key {
        Some(k) => {
            let (s, id) = alexandria(&k, &paddock.geometry, &sections, now).await;
            ("alexandria", s, id)
        }
        None => ("open_data", open_data(&paddock.geometry, &sections).await, None),
    };
    let report_id = report_id.unwrap_or_else(|| op_core::id::new_id("lr"));
    let report = json!({
        "report_id": report_id,
        "paddock_id": paddock.id,
        "source": source,
        "as_of": time::to_db(&now),
        "geometry": paddock.geometry,
        "sections": sections_out,
    });
    save(ctx, &report, Some(&paddock.id), &key, source, now).await?;
    Ok((report, false))
}

async fn save(ctx: &Ctx, report: &Value, paddock_id: Option<&str>, key: &str, source: &str, as_of: DateTime<Utc>) -> anyhow::Result<()> {
    sqlx::query("INSERT OR REPLACE INTO land_reports (id, paddock_id, cache_key, source, as_of, report, created_at) VALUES (?, ?, ?, ?, ?, ?, ?)")
        .bind(report["report_id"].as_str().unwrap_or_default())
        .bind(paddock_id)
        .bind(key)
        .bind(source)
        .bind(time::to_db(&as_of))
        .bind(report.to_string())
        .bind(time::to_db(&time::now()))
        .execute(ctx.db())
        .await?;
    Ok(())
}

// Alexandria (kit alexandria.py): Firecrawl's scrape endpoint with an
// `alexandria` parameter naming provider and capability.

async fn alexandria(api_key: &str, geometry: &Polygon, sections: &Value, as_of: DateTime<Utc>) -> (Map<String, Value>, Option<String>) {
    let body = json!({
        "alexandria": {
            "provider": "alexandria",
            "capability": "land_report",
            "options": { "geometry": geometry, "sections": sections, "as_of": as_of.to_rfc3339() },
        }
    });
    let res = async {
        let resp = http().post(ALEXANDRIA_URL).bearer_auth(api_key.trim()).json(&body).send().await?.error_for_status()?;
        anyhow::Ok(resp.json::<Value>().await?)
    }
    .await;
    match res {
        Ok(payload) => map_alexandria(sections, &payload),
        Err(e) => {
            tracing::warn!("Alexandria land report failed: {e}");
            let reason = format!("Alexandria request failed: {e}");
            (names(sections).map(|n| (n, unavailable(&reason))).collect(), None)
        }
    }
}

fn names(sections: &Value) -> impl Iterator<Item = String> + '_ {
    sections.as_object().into_iter().flat_map(|m| m.keys().cloned())
}

/// Map an Alexandria response to the requested sections only.
pub fn map_alexandria(sections: &Value, payload: &Value) -> (Map<String, Value>, Option<String>) {
    let report = find_report(payload);
    let raw = report.get("sections").filter(|s| s.is_object()).unwrap_or(&report);
    let default_sources = report.get("sources").filter(|s| s.is_array()).cloned();
    let mut out = Map::new();
    for name in names(sections) {
        let Some(section) = raw.get(&name).and_then(Value::as_object) else {
            out.insert(name, unavailable("Alexandria returned no data for this section."));
            continue;
        };
        let status = section.get("status").and_then(Value::as_str).unwrap_or("ok");
        if status != "ok" {
            let reason = section.get("reason").and_then(Value::as_str).unwrap_or("Section was not filled.");
            out.insert(name, unavailable(reason));
            continue;
        }
        let mut s = section.clone();
        s.insert("status".into(), json!("ok"));
        s.remove("reason");
        let has_sources = s.get("sources").and_then(Value::as_array).is_some_and(|a| !a.is_empty());
        if !has_sources {
            let sources = default_sources
                .clone()
                .unwrap_or_else(|| json!([{ "provider": "alexandria:alexandria/land_report", "retrieved_at": time::to_db(&time::now()) }]));
            s.insert("sources".into(), sources);
        }
        out.insert(name, Value::Object(s));
    }
    (out, report.get("report_id").and_then(Value::as_str).map(str::to_owned))
}

/// The report object inside a Firecrawl envelope.
fn find_report(payload: &Value) -> Value {
    let Some(obj) = payload.as_object() else { return json!({}) };
    let mut candidates: Vec<Option<&Value>> = vec![Some(payload)];
    if let Some(data) = obj.get("data").filter(|d| d.is_object()) {
        candidates = vec![data.get("alexandria"), data.get("result"), data.get("json"), Some(data), Some(payload)];
    }
    for c in candidates.into_iter().flatten() {
        if let Some(m) = c.as_object()
            && (m.contains_key("sections") || ["weather", "imagery", "soil"].iter().any(|k| m.contains_key(*k)))
        {
            return c.clone();
        }
    }
    json!({})
}

// Open data (kit open_data.py + ingestion/weather.py).

async fn open_data(geometry: &Polygon, sections: &Value) -> Map<String, Value> {
    let mut out = Map::new();
    for name in names(sections) {
        let section = match name.as_str() {
            "weather" => {
                let opts = &sections["weather"];
                let history = opts["history_days"].as_i64().unwrap_or(30);
                let forecast = opts["forecast_days"].as_i64().unwrap_or(7);
                match geometry.centroid() {
                    Some([lon, lat]) => match open_meteo(lon, lat, history, forecast).await {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::warn!("Open-Meteo weather fallback failed: {e}");
                            unavailable(format!("Weather lookup failed: {e}"))
                        }
                    },
                    None => unavailable("The paddock has no usable geometry."),
                }
            }
            "imagery" => unavailable("Imagery comes through Alexandria. Add a Firecrawl API key to enable it."),
            _ => unavailable(NEEDS_ALEXANDRIA),
        };
        out.insert(name, section);
    }
    out
}

fn fraction(v: &Value) -> Value {
    v.as_f64().map(|p| json!(crate::calc::round(p / 100.0, 3))).unwrap_or(Value::Null)
}

async fn open_meteo(lon: f64, lat: f64, history_days: i64, forecast_days: i64) -> anyhow::Result<Value> {
    let resp = http()
        .get(OPEN_METEO_URL)
        .timeout(Duration::from_secs(10))
        .query(&[
            ("latitude", lat.to_string()),
            ("longitude", lon.to_string()),
            ("current", "temperature_2m,precipitation,wind_speed_10m,relative_humidity_2m,snow_depth".into()),
            (
                "daily",
                "temperature_2m_max,temperature_2m_min,temperature_2m_mean,precipitation_sum,precipitation_probability_max,et0_fao_evapotranspiration".into(),
            ),
            ("past_days", history_days.clamp(0, 92).to_string()),
            ("forecast_days", forecast_days.clamp(1, 16).to_string()),
            ("timezone", "UTC".into()),
        ])
        .send()
        .await?
        .error_for_status()?;
    let payload: Value = resp.json().await?;
    Ok(weather_section(&payload, &time::now().format("%Y-%m-%d").to_string()))
}

/// Contract-shaped weather section from an Open-Meteo payload.
pub fn weather_section(payload: &Value, today: &str) -> Value {
    let current = &payload["current"];
    let daily = &payload["daily"];
    let pick = |key: &str, i: usize| daily[key].get(i).cloned().unwrap_or(Value::Null);
    let mut history = Vec::new();
    let mut forecast = Vec::new();
    for (i, day) in daily["time"].as_array().into_iter().flatten().enumerate() {
        let d = day.as_str().unwrap_or_default();
        let mut row = json!({
            "date": d,
            "precip_mm": pick("precipitation_sum", i),
            "temp_max_c": pick("temperature_2m_max", i),
            "temp_min_c": pick("temperature_2m_min", i),
        });
        if d < today {
            row["et0_mm"] = pick("et0_fao_evapotranspiration", i);
            row["temp_mean_c"] = pick("temperature_2m_mean", i);
            history.push(row);
        } else {
            row["precip_probability"] = fraction(&pick("precipitation_probability_max", i));
            forecast.push(row);
        }
    }
    let precip_24h = history.last().map(|r| r["precip_mm"].clone()).unwrap_or_else(|| current["precipitation"].clone());
    json!({
        "status": "ok",
        "current": {
            "air_temp_c": current["temperature_2m"],
            "precip_mm_24h": precip_24h,
            "wind_kph": current["wind_speed_10m"],
            "relative_humidity": fraction(&current["relative_humidity_2m"]),
            // Open-Meteo gives metres.
            "snow_depth_cm": current["snow_depth"].as_f64().map(|m| json!(crate::calc::round(m * 100.0, 1))).unwrap_or(Value::Null),
        },
        "history": history,
        "forecast": forecast,
        "sources": [{ "provider": "open-meteo", "retrieved_at": time::to_db(&time::now()), "resolution": "~11 km", "license": "CC BY 4.0" }],
    })
}

/// Why imagery can't give forage for this report, if it can't: `"snow"` when the
/// current snow depth is over [`SNOW_DEPTH_CM`], `"dormant"` when the mean air
/// temperature of the last [`DORMANT_DAYS`] days is under [`DORMANT_MEAN_C`]. A day's
/// mean is `temp_mean_c`, else the midpoint of its high and low. Fewer than
/// [`DORMANT_DAYS`] days of history never count as dormant.
pub fn forage_withheld(report: &Value) -> Option<&'static str> {
    let w = ok_section(report, "weather")?;
    let snow = w.get("current").and_then(|c| c.get("snow_depth_cm")).and_then(Value::as_f64);
    if snow.is_some_and(|d| d > SNOW_DEPTH_CM) {
        return Some("snow");
    }
    let day_mean =
        |r: &Value| r.get("temp_mean_c").and_then(Value::as_f64).or_else(|| Some((r.get("temp_max_c")?.as_f64()? + r.get("temp_min_c")?.as_f64()?) / 2.0));
    let days: Vec<f64> = w.get("history").and_then(Value::as_array)?.iter().rev().take(DORMANT_DAYS).filter_map(day_mean).collect();
    (days.len() == DORMANT_DAYS && days.iter().sum::<f64>() / (DORMANT_DAYS as f64) < DORMANT_MEAN_C).then_some("dormant")
}

/// One line per section, for the land endpoint and the context.
pub fn summary(report: &Value) -> Value {
    let mut lines = Vec::new();
    if let Some(w) = ok_section(report, "weather") {
        let rain: f64 = w.get("forecast").and_then(Value::as_array).into_iter().flatten().take(3).filter_map(|d| d["precip_mm"].as_f64()).sum();
        let cur = w.get("current").cloned().unwrap_or_default();
        let temp = cur["air_temp_c"].as_f64().map(|t| format!("{t}")).unwrap_or_else(|| "unknown".into());
        let p24 = cur["precip_mm_24h"].as_f64().unwrap_or(0.0);
        lines.push(format!("Weather: {temp}C now, {p24} mm in the last 24h, {} mm forecast over the next 3 days.", crate::calc::round(rain, 1)));
    }
    if let Some(i) = ok_section(report, "imagery") {
        let mean = i.get("ndvi_stats").and_then(|s| s.get("mean")).map(|m| m.to_string()).unwrap_or_else(|| "unknown".into());
        let captured =
            i.get("latest").and_then(|l| l["captured_at"].as_str()).map(|s| s.chars().take(10).collect()).unwrap_or_else(|| "unknown date".to_string());
        lines.push(format!("Imagery {captured}: NDVI mean {mean}."));
    }
    json!(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alexandria_envelope_maps_requested_sections() {
        let payload = json!({ "success": true, "data": { "alexandria": {
            "report_id": "lr_x",
            "sections": {
                "weather": { "status": "ok", "current": { "air_temp_c": 21 } },
                "imagery": { "status": "unavailable", "reason": "cloudy" },
                "soil": { "status": "ok", "texture": "loam" }
            }
        }}});
        let (s, id) = map_alexandria(&decision_sections(), &payload);
        assert_eq!(id.as_deref(), Some("lr_x"));
        assert_eq!(s["weather"]["status"], "ok");
        assert!(s["weather"]["sources"].as_array().is_some_and(|a| !a.is_empty()));
        assert_eq!(s["imagery"]["reason"], "cloudy");
        assert_eq!(s["climate"]["status"], "unavailable");
        assert!(s.get("soil").is_none());
    }

    #[test]
    fn open_meteo_splits_history_and_forecast() {
        let payload = json!({
            "current": { "temperature_2m": 18.2, "precipitation": 0.1, "wind_speed_10m": 9, "relative_humidity_2m": 55 },
            "daily": {
                "time": ["2026-09-24", "2026-09-25", "2026-09-26", "2026-09-27"],
                "precipitation_sum": [1.0, 4.5, 0.0, 12.0],
                "temperature_2m_max": [20, 21, 22, 19], "temperature_2m_min": [8, 9, 10, 7],
                "precipitation_probability_max": [10, 80, 5, 90],
                "et0_fao_evapotranspiration": [3.1, 2.2, 3.3, 1.0]
            }
        });
        let w = weather_section(&payload, "2026-09-26");
        assert_eq!(w["history"].as_array().unwrap().len(), 2);
        assert_eq!(w["forecast"][1]["precip_probability"], 0.9);
        assert_eq!(w["current"]["precip_mm_24h"], 4.5);
        assert_eq!(w["current"]["relative_humidity"], 0.55);
    }

    #[test]
    fn open_meteo_snow_depth_and_daily_means() {
        let payload = json!({
            "current": { "temperature_2m": -3.0, "snow_depth": 0.064 },
            "daily": {
                "time": ["2026-01-09", "2026-01-10", "2026-01-11"],
                "temperature_2m_max": [1, 0, 2], "temperature_2m_min": [-7, -8, -5], "temperature_2m_mean": [-3.1, -4.0, -1.5],
                "precipitation_sum": [0, 0, 1]
            }
        });
        let w = weather_section(&payload, "2026-01-11");
        assert_eq!(w["current"]["snow_depth_cm"], 6.4);
        assert_eq!(w["history"][1]["temp_mean_c"], -4.0);
        assert!(w["forecast"][0].get("temp_mean_c").is_none());
        let report = json!({ "sections": { "weather": w } });
        assert_eq!(forage_withheld(&report), Some("snow"));
        // No snow reported: null, not zero.
        let bare = weather_section(&json!({ "current": { "temperature_2m": 12.0 }, "daily": {} }), "2026-01-11");
        assert!(bare["current"]["snow_depth_cm"].is_null());
    }
}
