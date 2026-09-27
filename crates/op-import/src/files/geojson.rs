//! GeoJSON: polygons for paddock files, points with a time and a tag for
//! position history. Coordinates are WGS 84 longitude and latitude (RFC 7946);
//! an old-style `crs` member naming a UTM or Iowa state plane EPSG code is
//! projected back.

use op_geo::LonLat;
use serde_json::{Map, Value};

use super::crs::{self, Crs};
use super::read::{RawFeature, fmt_num};

const EXPORT_HINT: &str = "Export it in WGS 84 (EPSG:4326).";

fn parse(text: &str) -> Result<Value, String> {
    serde_json::from_str(text).map_err(|e| format!("The GeoJSON can't be read: {e}."))
}

fn crs_of(root: &Value) -> Result<Crs, String> {
    let Some(name) = root.pointer("/crs/properties/name").and_then(Value::as_str) else { return Ok(Crs::Geographic) };
    match crs::epsg_in_name(name) {
        Some(code) => crs::from_epsg(code).ok_or_else(|| format!("The GeoJSON is in EPSG:{code}, which openpasture can't read. {EXPORT_HINT}")),
        None => Err(format!("The GeoJSON is in {name}, which openpasture can't read. {EXPORT_HINT}")),
    }
}

/// Each feature (or bare geometry) with its properties, in file order.
fn features(root: &Value) -> Result<Vec<(Value, Map<String, Value>)>, String> {
    let kind = root.get("type").and_then(Value::as_str).unwrap_or_default();
    Ok(match kind {
        "FeatureCollection" => root
            .get("features")
            .and_then(Value::as_array)
            .ok_or_else(|| "The GeoJSON FeatureCollection has no features.".to_owned())?
            .iter()
            .map(|f| (f.get("geometry").cloned().unwrap_or(Value::Null), f.get("properties").and_then(Value::as_object).cloned().unwrap_or_default()))
            .collect(),
        "Feature" => {
            vec![(root.get("geometry").cloned().unwrap_or(Value::Null), root.get("properties").and_then(Value::as_object).cloned().unwrap_or_default())]
        }
        "Polygon" | "MultiPolygon" | "GeometryCollection" | "Point" | "MultiPoint" | "LineString" | "MultiLineString" => vec![(root.clone(), Map::new())],
        _ => return Err("This JSON isn't GeoJSON.".to_owned()),
    })
}

fn position(v: &Value, crs: &Crs) -> Option<LonLat> {
    let a = v.as_array()?;
    let (x, y) = (a.first()?.as_f64()?, a.get(1)?.as_f64()?);
    Some(crs.to_lonlat(x, y))
}

fn ring(v: &Value, crs: &Crs) -> Option<Vec<LonLat>> {
    v.as_array()?.iter().map(|p| position(p, crs)).collect()
}

fn polygon(v: &Value, crs: &Crs) -> Option<Vec<Vec<LonLat>>> {
    v.as_array()?.iter().map(|r| ring(r, crs)).collect()
}

/// Polygons and multipolygons of one geometry (collections included).
fn geometry_polygons(g: &Value, crs: &Crs, out: &mut Vec<Vec<Vec<LonLat>>>) -> Result<(), String> {
    let coords = g.get("coordinates");
    match g.get("type").and_then(Value::as_str).unwrap_or_default() {
        "Polygon" => out.push(coords.and_then(|c| polygon(c, crs)).ok_or_else(|| "A GeoJSON polygon has malformed coordinates.".to_owned())?),
        "MultiPolygon" => {
            for p in coords.and_then(Value::as_array).ok_or_else(|| "A GeoJSON multipolygon has malformed coordinates.".to_owned())? {
                out.push(polygon(p, crs).ok_or_else(|| "A GeoJSON multipolygon has malformed coordinates.".to_owned())?);
            }
        }
        "GeometryCollection" => {
            for part in g.get("geometries").and_then(Value::as_array).into_iter().flatten() {
                geometry_polygons(part, crs, out)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn check_lonlat(points: impl Iterator<Item = LonLat>) -> Result<(), String> {
    for p in points {
        if !(p[0].is_finite() && p[1].is_finite() && p[0].abs() <= 180.0 && p[1].abs() <= 90.0) {
            return Err(format!("The GeoJSON's coordinates aren't longitude and latitude, and it doesn't say what they are. {EXPORT_HINT}"));
        }
    }
    Ok(())
}

/// Property values as text, for names and FSA numbers.
pub fn attrs(props: &Map<String, Value>) -> Vec<(String, String)> {
    props
        .iter()
        .filter_map(|(k, v)| {
            let s = match v {
                Value::String(s) => s.trim().to_owned(),
                Value::Number(n) => fmt_num(n.as_f64()?)?,
                _ => return None,
            };
            (!s.is_empty()).then(|| (k.clone(), s))
        })
        .collect()
}

pub fn polygons(text: &str, layer: &str) -> Result<Vec<RawFeature>, String> {
    let root = parse(text)?;
    let crs = crs_of(&root)?;
    let mut out = Vec::new();
    for (g, props) in features(&root)? {
        let mut polys = Vec::new();
        geometry_polygons(&g, &crs, &mut polys)?;
        if polys.is_empty() {
            continue;
        }
        if crs == Crs::Geographic {
            check_lonlat(polys.iter().flatten().flatten().copied())?;
        }
        out.push(RawFeature { layer: layer.to_owned(), name: None, attrs: attrs(&props), polygons: polys });
    }
    Ok(out)
}

/// A point feature: where, and its properties (time and tag are chosen from
/// them by the mapping).
pub struct PointFeature {
    pub at: LonLat,
    pub props: Map<String, Value>,
}

/// Every Point (and each position of a MultiPoint) with its feature's properties.
pub fn points(text: &str) -> Result<Vec<PointFeature>, String> {
    let root = parse(text)?;
    let crs = crs_of(&root)?;
    let mut out = Vec::new();
    for (g, props) in features(&root)? {
        let coords = g.get("coordinates");
        let pts: Vec<LonLat> = match g.get("type").and_then(Value::as_str).unwrap_or_default() {
            "Point" => coords.and_then(|c| position(c, &crs)).into_iter().collect(),
            "MultiPoint" => coords.and_then(Value::as_array).into_iter().flatten().filter_map(|c| position(c, &crs)).collect(),
            _ => continue,
        };
        if crs == Crs::Geographic {
            check_lonlat(pts.iter().copied())?;
        }
        for at in pts {
            out.push(PointFeature { at, props: props.clone() });
        }
    }
    Ok(out)
}
