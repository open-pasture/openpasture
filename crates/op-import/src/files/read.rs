//! Paddock files to drafts: GeoJSON, KML, KMZ and zipped shapefiles (any
//! depth, several layers, as John Deere Operations Center exports them).
//! Every polygon becomes a draft named from its attributes, else its layer;
//! FSA farm, tract and field numbers go into `props`.

use std::collections::HashMap;
use std::io::{Cursor, Read as _};

use op_geo::{LonLat, Polygon, clean_ring, point_in_ring, signed_area};
use serde::Serialize;
use serde_json::{Map, Value};

use super::{geojson, kml, shp};

/// A polygon from a file, ready to become a paddock.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Draft {
    pub name: String,
    /// The file layer it came from (shapefile, KML folder, file name).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub layer: Option<String>,
    pub geometry: Polygon,
    pub area_ha: f64,
    /// FSA numbers under `fsa_farm`, `fsa_tract`, `fsa_field`.
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub props: Map<String, Value>,
}

/// A feature as read from a file: its layer, attributes and polygons, each
/// polygon an outer ring then holes, in longitude and latitude.
#[derive(Debug, Clone, Default)]
pub struct RawFeature {
    pub layer: String,
    /// A name the format carries outside the attributes (a KML Placemark's).
    pub name: Option<String>,
    pub attrs: Vec<(String, String)>,
    pub polygons: Vec<Vec<Vec<LonLat>>>,
}

/// Drafts and the polygons that couldn't become one, with why.
#[derive(Debug, Default)]
pub struct Read {
    pub drafts: Vec<Draft>,
    pub errors: Vec<String>,
}

/// Most drafts one file may give: a farm, not a county's CLU layer.
pub const MAX_DRAFTS: usize = 2_000;
/// Most corners one ring may have; checking a ring costs its square.
pub const MAX_RING_VERTICES: usize = 20_000;
/// Most bytes read out of one zip, all entries together.
const MAX_UNZIPPED: u64 = 256 * 1024 * 1024;

const FORMATS: &str = "Import GeoJSON, KML, KMZ or a zipped shapefile.";

/// Read a paddock file by its content (the name only helps with layer names).
pub fn read_paddock_file(file_name: &str, bytes: &[u8]) -> Result<Read, String> {
    let stem = stem(file_name);
    let features = if bytes.starts_with(b"PK\x03\x04") {
        read_zip(&stem, bytes)?
    } else if bytes.starts_with(&[0x00, 0x00, 0x27, 0x0a]) {
        return Err("A shapefile comes as several files. Zip the .shp with its .dbf, .shx and .prj, then import the zip.".to_owned());
    } else {
        let text = std::str::from_utf8(bytes).map_err(|_| format!("This isn't a file openpasture can read. {FORMATS}"))?;
        let t = text.trim_start_matches('\u{feff}').trim_start();
        if t.starts_with('{') {
            geojson::polygons(t, &stem)?
        } else if t.starts_with('<') {
            kml::polygons(t, &stem)?
        } else {
            return Err(format!("This isn't a file openpasture can read. {FORMATS}"));
        }
    };
    let read = drafts(features)?;
    if read.drafts.is_empty() && read.errors.is_empty() {
        return Err("The file has no polygons.".to_owned());
    }
    Ok(read)
}

fn stem(path: &str) -> String {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let s = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base).trim();
    if s.is_empty() { "Import".to_owned() } else { s.to_owned() }
}

fn ext(path: &str) -> String {
    path.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default()
}

// ---------------------------------------------------------------- zips

/// Every shapefile, KML and GeoJSON inside a zip, at any depth. Mac zips'
/// `__MACOSX/` shadows and dot files are skipped.
fn read_zip(zip_stem: &str, bytes: &[u8]) -> Result<Vec<RawFeature>, String> {
    let mut archive = zip::ZipArchive::new(Cursor::new(bytes)).map_err(|e| format!("The zip can't be opened: {e}."))?;
    let mut files: HashMap<String, (String, Vec<u8>)> = HashMap::new(); // lowercase path → (path, bytes)
    let mut budget = MAX_UNZIPPED;
    for i in 0..archive.len() {
        let mut f = archive.by_index(i).map_err(|e| format!("The zip can't be read: {e}."))?;
        if f.is_dir() {
            continue;
        }
        let name = f.name().replace('\\', "/");
        let base = name.rsplit('/').next().unwrap_or(&name);
        if name.starts_with("__MACOSX/") || name.contains("/__MACOSX/") || base.starts_with("._") || base.is_empty() {
            continue;
        }
        if !matches!(ext(&name).as_str(), "shp" | "dbf" | "prj" | "kml" | "geojson" | "json") {
            continue;
        }
        let mut buf = Vec::new();
        let n = (&mut f).take(budget + 1).read_to_end(&mut buf).map_err(|e| format!("The zip can't be read: {e}."))? as u64;
        if n > budget {
            return Err("The zip unpacks to more than 256 MB.".to_owned());
        }
        budget -= n;
        files.insert(name.to_ascii_lowercase(), (name, buf));
    }

    let mut keys: Vec<&String> = files.keys().collect();
    keys.sort();
    // Layer names: the file's stem, with its folder when two layers share a stem.
    let layer_paths: Vec<&String> = keys.iter().copied().filter(|k| matches!(ext(k).as_str(), "shp" | "kml" | "geojson" | "json")).collect();
    let mut stems: HashMap<String, usize> = HashMap::new();
    for k in &layer_paths {
        *stems.entry(stem(k)).or_default() += 1;
    }
    let layer_name = |path: &str| -> String {
        let s = stem(path);
        if s.eq_ignore_ascii_case("doc") && layer_paths.len() == 1 {
            return zip_stem.to_owned(); // a KMZ's doc.kml is the KMZ
        }
        if stems.get(&s.to_ascii_lowercase()).copied().unwrap_or(0) > 1 {
            let dir: Vec<&str> = path.rsplit('/').skip(1).take(1).collect();
            if let Some(d) = dir.first().filter(|d| !d.is_empty()) {
                return format!("{d}/{s}");
            }
        }
        s
    };

    let mut out = Vec::new();
    let mut polygon_layers = 0;
    for key in &layer_paths {
        let (path, data) = &files[*key];
        let layer = layer_name(path);
        let features = match ext(key).as_str() {
            "shp" => {
                let base = &key[..key.len() - 4];
                let dbf = files.get(&format!("{base}.dbf")).map(|(_, b)| b.as_slice());
                let prj = files.get(&format!("{base}.prj")).map(|(_, b)| String::from_utf8_lossy(b).into_owned());
                match shp::polygons(data, dbf, prj.as_deref(), &layer)? {
                    Some(f) => f,
                    None => continue, // points or lines
                }
            }
            "kml" => {
                let text = std::str::from_utf8(data).map_err(|_| format!("{layer}: the KML isn't UTF-8 text."))?;
                kml::polygons(text, &layer)?
            }
            _ => {
                let text = std::str::from_utf8(data).map_err(|_| format!("{layer}: the GeoJSON isn't UTF-8 text."))?;
                geojson::polygons(text, &layer)?
            }
        };
        if features.iter().any(|f| !f.polygons.is_empty()) {
            polygon_layers += 1;
        }
        out.extend(features);
    }
    if layer_paths.is_empty() {
        return Err(format!("The zip holds no shapefile, KML or GeoJSON. {FORMATS}"));
    }
    if polygon_layers == 0 {
        return Err("The zip has no polygon layers.".to_owned());
    }
    Ok(out)
}

// ---------------------------------------------------------------- drafts

const NAME_KEYS: &[&str] =
    &["NAME", "FIELDNAME", "FIELD", "FLDNAME", "PADDOCK", "PADDOCKNAME", "PASTURE", "PASTURENAME", "BOUNDARYNAME", "BNDNAME", "LABEL", "TITLE"];
const FARM_KEYS: &[&str] = &["FARMNBR", "FARMNUMBER", "FARMNUM", "FARMNO", "FSAFARM", "FSAFARMNBR"];
const TRACT_KEYS: &[&str] = &["TRACTNBR", "TRACTNUMBER", "TRACTNUM", "TRACTNO", "FSATRACT", "FSATRACTNBR"];
const FIELD_KEYS: &[&str] = &["CLUNBR", "CLUNUMBER", "CLUNUM", "CLUNO", "CLU", "FIELDNBR", "FIELDNUMBER", "FIELDNUM", "FIELDNO", "FLDNBR", "FSAFIELD"];

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

/// The first non-empty attribute among `keys` (compared without case or punctuation).
fn attr<'a>(attrs: &'a [(String, String)], keys: &[&str]) -> Option<&'a str> {
    keys.iter().find_map(|k| attrs.iter().find(|(n, v)| norm(n) == *k && !v.trim().is_empty()).map(|(_, v)| v.trim()))
}

/// A number as people write it: `12`, not `12.0`.
pub fn fmt_num(v: f64) -> Option<String> {
    if !v.is_finite() {
        return None;
    }
    if v.fract() == 0.0 && v.abs() < 1e15 {
        return Some(format!("{}", v as i64));
    }
    Some(format!("{v}"))
}

fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Name, FSA props and a checked geometry for every polygon.
pub fn drafts(features: Vec<RawFeature>) -> Result<Read, String> {
    let total: usize = features.iter().map(|f| f.polygons.len()).sum();
    if total > MAX_DRAFTS {
        return Err(format!("The file has {total} polygons; openpasture imports at most {MAX_DRAFTS} at a time. Import one farm's fields at a time."));
    }
    let mut per_layer: HashMap<String, usize> = HashMap::new();
    for f in &features {
        *per_layer.entry(f.layer.clone()).or_default() += f.polygons.len();
    }
    let mut seen_in_layer: HashMap<String, usize> = HashMap::new();
    let mut out = Read::default();
    for f in features {
        let mut props = Map::new();
        for (key, set) in [("fsa_farm", FARM_KEYS), ("fsa_tract", TRACT_KEYS), ("fsa_field", FIELD_KEYS)] {
            if let Some(v) = attr(&f.attrs, set) {
                props.insert(key.to_owned(), Value::String(v.to_owned()));
            }
        }
        let named = attr(&f.attrs, NAME_KEYS)
            .map(str::to_owned)
            .or_else(|| f.name.clone().filter(|n| !n.trim().is_empty()).map(|n| n.trim().to_owned()))
            .or_else(|| props.get("fsa_field").and_then(Value::as_str).map(|c| format!("Field {c}")));
        for (i, rings) in f.polygons.into_iter().enumerate() {
            let n = seen_in_layer.entry(f.layer.clone()).or_default();
            *n += 1;
            let name = match &named {
                Some(base) if i == 0 => base.clone(),
                Some(base) => format!("{base} {}", i + 1),
                None if per_layer.get(&f.layer).copied().unwrap_or(0) <= 1 => f.layer.clone(),
                None => format!("{} {n}", f.layer),
            };
            let name: String = name.chars().take(200).collect();
            if let Some(r) = rings.iter().find(|r| r.len() > MAX_RING_VERTICES) {
                out.errors.push(format!("{name}: {} corners is more than openpasture takes ({MAX_RING_VERTICES}).", r.len()));
                continue;
            }
            let geometry = match (Polygon { kind: Default::default(), coordinates: rings }).validated() {
                Ok(g) => g,
                Err(e) => {
                    out.errors.push(format!("{name}: {e}"));
                    continue;
                }
            };
            let area_ha = round3(geometry.area_ha());
            if area_ha <= 0.0 {
                out.errors.push(format!("{name}: the shape has no area."));
                continue;
            }
            out.drafts.push(Draft { name, layer: Some(f.layer.clone()), geometry, area_ha, props: props.clone() });
        }
    }
    Ok(out)
}

/// Rings with no stated role (a shapefile's parts) grouped into polygons by
/// nesting, whichever way they wind: a ring inside an odd number of others is
/// a hole of the smallest ring around it.
pub fn assemble(rings: Vec<Vec<LonLat>>) -> Vec<Vec<Vec<LonLat>>> {
    let rings: Vec<Vec<LonLat>> = rings.into_iter().filter(|r| clean_ring(r).len() >= 3).collect();
    let clean: Vec<Vec<LonLat>> = rings.iter().map(|r| clean_ring(r)).collect();
    let area: Vec<f64> = clean.iter().map(|r| signed_area(r).abs()).collect();
    let inside = |i: usize, j: usize| i != j && area[j] > area[i] && point_in_ring(probe(&clean[i], &clean[j]), &clean[j]);
    let depth: Vec<usize> = (0..clean.len()).map(|i| (0..clean.len()).filter(|&j| inside(i, j)).count()).collect();
    let mut polygons: Vec<(usize, Vec<Vec<LonLat>>)> = Vec::new();
    for i in (0..clean.len()).filter(|&i| depth[i] % 2 == 0) {
        polygons.push((i, vec![rings[i].clone()]));
    }
    for i in (0..clean.len()).filter(|&i| depth[i] % 2 == 1) {
        let parent = (0..clean.len()).filter(|&j| depth[j] + 1 == depth[i] && inside(i, j)).min_by(|&a, &b| area[a].total_cmp(&area[b]));
        if let Some(p) = parent
            && let Some((_, poly)) = polygons.iter_mut().find(|(o, _)| *o == p)
        {
            poly.push(rings[i].clone());
        }
    }
    polygons.into_iter().map(|(_, p)| p).collect()
}

/// A vertex of `ring` that isn't on `other` (rings that share a corner still
/// nest by their other corners).
fn probe(ring: &[LonLat], other: &[LonLat]) -> LonLat {
    ring.iter().copied().find(|p| !other.iter().any(|q| (q[0] - p[0]).abs() < 1e-12 && (q[1] - p[1]).abs() < 1e-12)).unwrap_or(ring[0])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn square(x: f64, y: f64, s: f64, cw: bool) -> Vec<LonLat> {
        let mut r = vec![[x, y], [x + s, y], [x + s, y + s], [x, y + s], [x, y]];
        if cw {
            r.reverse();
        }
        r
    }

    #[test]
    fn rings_nest_whichever_way_they_wind() {
        for cw in [false, true] {
            let got = assemble(vec![square(0.0, 0.0, 0.01, cw), square(0.004, 0.004, 0.002, cw), square(0.02, 0.0, 0.01, !cw)]);
            assert_eq!(got.len(), 2);
            assert_eq!(got[0].len(), 2, "hole joins the big square");
            assert_eq!(got[1].len(), 1);
            // An island inside the hole is its own polygon.
            let got = assemble(vec![square(0.0, 0.0, 0.01, cw), square(0.003, 0.003, 0.004, cw), square(0.0045, 0.0045, 0.001, cw)]);
            assert_eq!(got.len(), 2);
            assert_eq!(got.iter().map(Vec::len).collect::<Vec<_>>(), vec![2, 1]);
        }
    }

    #[test]
    fn names_come_from_attributes_then_layers() {
        let poly = || vec![square(-93.62, 42.03, 0.002, false)];
        let f = |layer: &str, attrs: &[(&str, &str)], n: usize| RawFeature {
            layer: layer.to_owned(),
            name: None,
            attrs: attrs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            polygons: (0..n).map(|_| poly()).collect(),
        };
        let r = drafts(vec![
            f("Fields", &[("FIELD_NAME", "North 40"), ("FARMNBR", "1234"), ("TRACTNBR", "5678"), ("CLUNBR", "3")], 1),
            f("Fields", &[("CLU_NBR", "7")], 1),
            f("Fields", &[("NAME", "Creek")], 2),
            f("Boundary", &[], 1),
            f("Lots", &[], 1),
            f("Lots", &[], 1),
        ])
        .unwrap();
        let names: Vec<&str> = r.drafts.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["North 40", "Field 7", "Creek", "Creek 2", "Boundary", "Lots 1", "Lots 2"]);
        assert_eq!(r.drafts[0].props["fsa_farm"], "1234");
        assert_eq!(r.drafts[0].props["fsa_tract"], "5678");
        assert_eq!(r.drafts[0].props["fsa_field"], "3");
        assert!(r.drafts[4].props.is_empty());
    }

    #[test]
    fn a_bowtie_is_an_error_not_a_draft() {
        let bowtie = vec![[-93.62, 42.03], [-93.61, 42.04], [-93.61, 42.03], [-93.62, 42.04], [-93.62, 42.03]];
        let r = drafts(vec![RawFeature { layer: "x".into(), name: Some("Bow".into()), attrs: vec![], polygons: vec![vec![bowtie]] }]).unwrap();
        assert!(r.drafts.is_empty());
        assert_eq!(r.errors, vec!["Bow: The shape crosses itself.".to_owned()]);
    }
}
