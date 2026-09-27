//! Coordinate systems of imported files: the WKT in a shapefile's `.prj`
//! (ESRI and OGC WKT 1) and the EPSG codes old GeoJSON files name. Geographic
//! WGS 84 and NAD83 pass through; Transverse Mercator and Lambert Conformal
//! Conic on those datums are projected back to longitude and latitude
//! ([`super::proj`]); anything else is refused with its name, so the farmer
//! knows what to export instead.

use super::proj::{Ellipsoid, Method, Projected};
use op_geo::LonLat;

#[derive(Debug, Clone, PartialEq)]
pub enum Crs {
    Geographic,
    Projected(Box<Projected>),
}

impl Crs {
    /// `[lon, lat]` for a coordinate pair in this system.
    pub fn to_lonlat(&self, x: f64, y: f64) -> LonLat {
        match self {
            Crs::Geographic => [x, y],
            Crs::Projected(p) => p.inverse(x, y),
        }
    }
}

const METRE: f64 = 1.0;
const US_FOOT: f64 = 1200.0 / 3937.0;
const FOOT: f64 = 0.3048;
const EXPORT_HINT: &str = "Export it in WGS 84 (EPSG:4326), UTM or a state plane zone.";

// ---------------------------------------------------------------- WKT tree

#[derive(Debug, Clone, PartialEq)]
enum Node {
    Item { key: String, args: Vec<Node> },
    Text(String),
    Number(f64),
    Word(String),
}

impl Node {
    fn key(&self) -> Option<&str> {
        match self {
            Node::Item { key, .. } => Some(key),
            _ => None,
        }
    }
    fn args(&self) -> &[Node] {
        match self {
            Node::Item { args, .. } => args,
            _ => &[],
        }
    }
    /// Children with this keyword (case-insensitive).
    fn children<'a>(&'a self, key: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.args().iter().filter(move |n| n.key().is_some_and(|k| k.eq_ignore_ascii_case(key)))
    }
    fn child(&self, key: &str) -> Option<&Node> {
        self.args().iter().find(|n| n.key().is_some_and(|k| k.eq_ignore_ascii_case(key)))
    }
    /// The first quoted argument: an item's name.
    fn name(&self) -> String {
        self.args().iter().find_map(|a| if let Node::Text(s) = a { Some(s.clone()) } else { None }).unwrap_or_default()
    }
    /// The `i`th number among the arguments.
    fn number(&self, i: usize) -> Option<f64> {
        self.args().iter().filter_map(|a| if let Node::Number(n) = a { Some(*n) } else { None }).nth(i)
    }
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while self.i < self.s.len() && self.s[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }

    fn node(&mut self, depth: usize) -> Result<Node, ()> {
        if depth > 32 {
            return Err(());
        }
        self.ws();
        let c = *self.s.get(self.i).ok_or(())?;
        if c == b'"' {
            self.i += 1;
            let mut out = Vec::new();
            loop {
                let c = *self.s.get(self.i).ok_or(())?;
                self.i += 1;
                if c == b'"' {
                    // "" is an escaped quote.
                    if self.s.get(self.i) == Some(&b'"') {
                        out.push(b'"');
                        self.i += 1;
                        continue;
                    }
                    break;
                }
                out.push(c);
            }
            return Ok(Node::Text(String::from_utf8_lossy(&out).into_owned()));
        }
        if c == b'-' || c == b'+' || c == b'.' || c.is_ascii_digit() {
            let start = self.i;
            while self.i < self.s.len() && matches!(self.s[self.i], b'0'..=b'9' | b'.' | b'-' | b'+' | b'e' | b'E') {
                self.i += 1;
            }
            let t = std::str::from_utf8(&self.s[start..self.i]).map_err(|_| ())?;
            return t.parse().map(Node::Number).map_err(|_| ());
        }
        if c.is_ascii_alphabetic() || c == b'_' {
            let start = self.i;
            while self.i < self.s.len() && (self.s[self.i].is_ascii_alphanumeric() || self.s[self.i] == b'_') {
                self.i += 1;
            }
            let key = std::str::from_utf8(&self.s[start..self.i]).map_err(|_| ())?.to_owned();
            self.ws();
            let Some(&open) = self.s.get(self.i).filter(|c| **c == b'[' || **c == b'(') else {
                return Ok(Node::Word(key));
            };
            let close = if open == b'[' { b']' } else { b')' };
            self.i += 1;
            let mut args = Vec::new();
            loop {
                self.ws();
                if self.s.get(self.i) == Some(&close) {
                    self.i += 1;
                    break;
                }
                args.push(self.node(depth + 1)?);
                self.ws();
                match self.s.get(self.i) {
                    Some(b',') => self.i += 1,
                    Some(c) if *c == close => {
                        self.i += 1;
                        break;
                    }
                    _ => return Err(()),
                }
            }
            return Ok(Node::Item { key, args });
        }
        Err(())
    }
}

fn parse(wkt: &str) -> Result<Node, String> {
    let mut p = Parser { s: wkt.trim_start_matches('\u{feff}').as_bytes(), i: 0 };
    let node = p.node(0).map_err(|_| "The .prj file isn't a coordinate system openpasture can read.".to_owned())?;
    if node.key().is_none() {
        return Err("The .prj file isn't a coordinate system openpasture can read.".to_owned());
    }
    Ok(node)
}

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_uppercase()).collect()
}

// ---------------------------------------------------------------- WKT → Crs

/// The coordinate system a `.prj` describes.
pub fn from_wkt(wkt: &str) -> Result<Crs, String> {
    let root = parse(wkt)?;
    let key = root.key().unwrap_or_default().to_ascii_uppercase();
    match key.as_str() {
        "COMPD_CS" => {
            let horizontal = root
                .args()
                .iter()
                .find(|n| n.key().is_some_and(|k| k.eq_ignore_ascii_case("PROJCS") || k.eq_ignore_ascii_case("GEOGCS")))
                .ok_or_else(|| "The .prj file has no horizontal coordinate system.".to_owned())?;
            from_node(horizontal)
        }
        "PROJCS" | "GEOGCS" => from_node(&root),
        "PROJCRS" | "GEOGCRS" | "GEODCRS" | "BOUNDCRS" | "COMPOUNDCRS" => {
            Err(format!("The .prj file is WKT2, which openpasture doesn't read. Save the shapefile with an ESRI .prj, or {}", lower_first(EXPORT_HINT)))
        }
        _ => Err("The .prj file isn't a coordinate system openpasture can read.".to_owned()),
    }
}

fn lower_first(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default()
}

fn from_node(node: &Node) -> Result<Crs, String> {
    if node.key().is_some_and(|k| k.eq_ignore_ascii_case("GEOGCS")) {
        geographic(node)?;
        return Ok(Crs::Geographic);
    }
    let geog = node.child("GEOGCS").ok_or_else(|| "The .prj file has no datum.".to_owned())?;
    let ellipsoid = geographic(geog)?;
    let projection = node.child("PROJECTION").map(Node::name).unwrap_or_default();
    let params: Vec<(String, f64)> = node.children("PARAMETER").filter_map(|p| Some((norm(&p.name()), p.number(0)?))).collect();
    let param = |names: &[&str]| params.iter().find(|(k, _)| names.contains(&k.as_str())).map(|(_, v)| *v);
    let need = |names: &[&str], label: &str| param(names).ok_or_else(|| format!("The .prj file has no {label}."));

    let unit = node.child("UNIT").ok_or_else(|| "The .prj file has no linear unit.".to_owned())?;
    let unit_m = linear_unit(unit)?;
    let fe = param(&["FALSEEASTING"]).unwrap_or(0.0);
    let fnn = param(&["FALSENORTHING"]).unwrap_or(0.0);
    let lon0_names = ["CENTRALMERIDIAN", "LONGITUDEOFORIGIN", "LONGITUDEOFCENTER", "LONGITUDEOFNATURALORIGIN"];
    let lat0_names = ["LATITUDEOFORIGIN", "LATITUDEOFCENTER", "LATITUDEOFNATURALORIGIN"];
    let k0_names = ["SCALEFACTOR", "SCALEFACTORATNATURALORIGIN"];

    let method = match norm(&projection).as_str() {
        "TRANSVERSEMERCATOR" | "GAUSSKRUGER" => Method::TransverseMercator {
            lat0: param(&lat0_names).unwrap_or(0.0),
            lon0: need(&lon0_names, "central meridian")?,
            k0: param(&k0_names).unwrap_or(1.0),
        },
        "LAMBERTCONFORMALCONIC" | "LAMBERTCONFORMALCONIC2SP" | "LAMBERTCONFORMALCONIC1SP" => {
            let sp1 = param(&["STANDARDPARALLEL1"]);
            let sp2 = param(&["STANDARDPARALLEL2"]);
            let lat0 = param(&lat0_names);
            let lon0 = need(&lon0_names, "central meridian")?;
            match (sp1, sp2) {
                (Some(sp1), Some(sp2)) => Method::LambertConic {
                    lat0: lat0.ok_or_else(|| "The .prj file has no latitude of origin.".to_owned())?,
                    lon0,
                    sp1,
                    sp2: Some(sp2),
                    k0: 1.0,
                },
                // One standard parallel: it is the latitude of origin, with a scale factor.
                (sp1, None) => {
                    let lat0 = lat0.or(sp1).ok_or_else(|| "The .prj file has no latitude of origin.".to_owned())?;
                    Method::LambertConic { lat0, lon0, sp1: lat0, sp2: None, k0: param(&k0_names).unwrap_or(1.0) }
                }
                (None, Some(_)) => return Err("The .prj file has no first standard parallel.".to_owned()),
            }
        }
        "" => return Err("The .prj file names no projection.".to_owned()),
        _ => return Err(format!("The file uses the {projection} projection, which openpasture can't read. {EXPORT_HINT}")),
    };
    Ok(Crs::Projected(Box::new(Projected::new(ellipsoid, method, fe, fnn, unit_m))))
}

/// Checks a GEOGCS (datum, prime meridian, angular unit) and returns its ellipsoid.
fn geographic(geog: &Node) -> Result<Ellipsoid, String> {
    let datum = geog.child("DATUM").ok_or_else(|| "The .prj file has no datum.".to_owned())?;
    let name = datum.name();
    let n = norm(&name);
    let nad83 = n.contains("NAD83") || (n.contains("1983") && (n.contains("NAD") || n.contains("NORTHAMERICAN")));
    let wgs84 = n.contains("WGS84") || n.contains("WGS1984");
    if !nad83 && !wgs84 {
        let shown = if name.is_empty() { "an unnamed".to_owned() } else { format!("the {name}") };
        return Err(format!("The file uses {shown} datum. openpasture reads WGS 84 and NAD83 only. {EXPORT_HINT}"));
    }
    if let Some(pm) = geog.child("PRIMEM")
        && pm.number(0).is_some_and(|v| v.abs() > 1e-9)
    {
        return Err(format!("The file measures longitude from the {} meridian, not Greenwich. {EXPORT_HINT}", pm.name()));
    }
    if let Some(u) = geog.child("UNIT")
        && let Some(f) = u.number(0)
        && (f - std::f64::consts::PI / 180.0).abs() > 1e-9
    {
        return Err(format!("The file measures angles in {}, not degrees. {EXPORT_HINT}", u.name()));
    }
    let spheroid = datum.child("SPHEROID").or_else(|| datum.child("ELLIPSOID"));
    Ok(match spheroid.and_then(|s| Some(Ellipsoid { a: s.number(0)?, inv_f: s.number(1)? })) {
        Some(e) if e.a > 6_000_000.0 && e.a < 7_000_000.0 && (e.inv_f == 0.0 || e.inv_f > 250.0) => e,
        _ if wgs84 => Ellipsoid::WGS84,
        _ => Ellipsoid::GRS80,
    })
}

fn linear_unit(unit: &Node) -> Result<f64, String> {
    let f = unit.number(0).ok_or_else(|| "The .prj file's linear unit has no size.".to_owned())?;
    for known in [METRE, US_FOOT, FOOT] {
        if (f - known).abs() < 1e-9 {
            return Ok(known);
        }
    }
    Err(format!("The file measures in {}, which openpasture doesn't read. Use metres or feet.", unit.name()))
}

// ---------------------------------------------------------------- EPSG codes

/// Coordinate systems an old GeoJSON `crs` member may name: geographic
/// WGS 84/NAD83, UTM zones on either datum, and the Iowa state plane zones.
pub fn from_epsg(code: u32) -> Option<Crs> {
    let utm = |zone: u32, south: bool, e: Ellipsoid| {
        let lon0 = -183.0 + 6.0 * zone as f64;
        Crs::Projected(Box::new(Projected::new(
            e,
            Method::TransverseMercator { lat0: 0.0, lon0, k0: 0.9996 },
            500_000.0,
            if south { 10_000_000.0 } else { 0.0 },
            METRE,
        )))
    };
    let iowa = |north: bool, unit: f64| {
        let (lat0, sp1, sp2, fe, fnn) = if north {
            (41.5, 43.0 + 16.0 / 60.0, 42.0 + 4.0 / 60.0, 1_500_000.0, 1_000_000.0)
        } else {
            (40.0, 41.0 + 47.0 / 60.0, 40.0 + 37.0 / 60.0, 500_000.0, 0.0)
        };
        Crs::Projected(Box::new(Projected::new(
            Ellipsoid::GRS80,
            Method::LambertConic { lat0, lon0: -93.5, sp1, sp2: Some(sp2), k0: 1.0 },
            fe / unit,
            fnn / unit,
            unit,
        )))
    };
    Some(match code {
        4326 | 4269 | 4152 | 6318 | 4979 | 4167 => Crs::Geographic,
        32601..=32660 => utm(code - 32600, false, Ellipsoid::WGS84),
        32701..=32760 => utm(code - 32700, true, Ellipsoid::WGS84),
        26901..=26923 => utm(code - 26900, false, Ellipsoid::GRS80),
        26975 => iowa(true, METRE),
        26976 => iowa(false, METRE),
        3417 => iowa(true, US_FOOT),
        3418 => iowa(false, US_FOOT),
        _ => return None,
    })
}

/// The code in a GeoJSON `crs` name: `EPSG:26915`, `urn:ogc:def:crs:EPSG::26915`,
/// `urn:ogc:def:crs:OGC:1.3:CRS84` (4326).
pub fn epsg_in_name(name: &str) -> Option<u32> {
    let up = name.to_ascii_uppercase();
    if up.ends_with("CRS84") {
        return Some(4326);
    }
    let at = up.rfind("EPSG")?;
    up[at + 4..].trim_start_matches(|c: char| !c.is_ascii_digit()).chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ESRI_3417: &str = r#"PROJCS["NAD_1983_StatePlane_Iowa_North_FIPS_1401_Feet",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Lambert_Conformal_Conic"],PARAMETER["False_Easting",4921250.0],PARAMETER["False_Northing",3280833.3333],PARAMETER["Central_Meridian",-93.5],PARAMETER["Standard_Parallel_1",43.2666666666667],PARAMETER["Standard_Parallel_2",42.0666666666667],PARAMETER["Latitude_Of_Origin",41.5],UNIT["US survey foot",0.304800609601219]]"#;
    const GDAL_3417: &str = r#"PROJCS["NAD83 / Iowa North (ftUS)",GEOGCS["NAD83",DATUM["North_American_Datum_1983",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6269"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","4269"]],PROJECTION["Lambert_Conformal_Conic_2SP"],PARAMETER["latitude_of_origin",41.5],PARAMETER["central_meridian",-93.5],PARAMETER["standard_parallel_1",43.2666666666667],PARAMETER["standard_parallel_2",42.0666666666667],PARAMETER["false_easting",4921250],PARAMETER["false_northing",3280833.3333],UNIT["US survey foot",0.304800609601219,AUTHORITY["EPSG","9003"]],AXIS["Easting",EAST],AXIS["Northing",NORTH],AUTHORITY["EPSG","3417"]]"#;
    const ESRI_26915: &str = r#"PROJCS["NAD_1983_UTM_Zone_15N",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-93.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;
    const GDAL_26915: &str = r#"PROJCS["NAD83 / UTM zone 15N",GEOGCS["NAD83",DATUM["North_American_Datum_1983",SPHEROID["GRS 1980",6378137,298.257222101,AUTHORITY["EPSG","7019"]],AUTHORITY["EPSG","6269"]],PRIMEM["Greenwich",0,AUTHORITY["EPSG","8901"]],UNIT["degree",0.0174532925199433,AUTHORITY["EPSG","9122"]],AUTHORITY["EPSG","4269"]],PROJECTION["Transverse_Mercator"],PARAMETER["latitude_of_origin",0],PARAMETER["central_meridian",-93],PARAMETER["scale_factor",0.9996],PARAMETER["false_easting",500000],PARAMETER["false_northing",0],UNIT["metre",1,AUTHORITY["EPSG","9001"]],AXIS["Easting",EAST],AXIS["Northing",NORTH],AUTHORITY["EPSG","26915"]]"#;
    const WGS84: &str =
        r#"GEOGCS["GCS_WGS_1984",DATUM["D_WGS_1984",SPHEROID["WGS_1984",6378137.0,298.257223563]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]]"#;
    const ALBERS: &str = r#"PROJCS["NAD_1983_Contiguous_USA_Albers",GEOGCS["GCS_North_American_1983",DATUM["D_North_American_1983",SPHEROID["GRS_1980",6378137.0,298.257222101]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Albers"],PARAMETER["False_Easting",0.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-96.0],PARAMETER["Standard_Parallel_1",29.5],PARAMETER["Standard_Parallel_2",45.5],PARAMETER["Latitude_Of_Origin",23.0],UNIT["Meter",1.0]]"#;
    const NAD27_UTM: &str = r#"PROJCS["NAD_1927_UTM_Zone_15N",GEOGCS["GCS_North_American_1927",DATUM["D_North_American_1927",SPHEROID["Clarke_1866",6378206.4,294.978698213898]],PRIMEM["Greenwich",0.0],UNIT["Degree",0.0174532925199433]],PROJECTION["Transverse_Mercator"],PARAMETER["False_Easting",500000.0],PARAMETER["False_Northing",0.0],PARAMETER["Central_Meridian",-93.0],PARAMETER["Scale_Factor",0.9996],PARAMETER["Latitude_Of_Origin",0.0],UNIT["Meter",1.0]]"#;

    fn near(a: LonLat, b: LonLat) -> bool {
        (a[0] - b[0]).abs() < 1e-7 && (a[1] - b[1]).abs() < 1e-7
    }

    #[test]
    fn esri_and_gdal_wkt_read_the_same() {
        for (a, b, xy, ll) in [
            (ESRI_3417, GDAL_3417, [4_888_646.767_925_305, 3_474_001.287_300_229_5], [-93.62, 42.03]),
            (ESRI_26915, GDAL_26915, [448_677.093_953_558_9, 4_653_293.017_557_551], [-93.62, 42.03]),
        ] {
            let (a, b) = (from_wkt(a).unwrap(), from_wkt(b).unwrap());
            assert!(near(a.to_lonlat(xy[0], xy[1]), ll), "{:?}", a.to_lonlat(xy[0], xy[1]));
            assert!(near(b.to_lonlat(xy[0], xy[1]), ll), "{:?}", b.to_lonlat(xy[0], xy[1]));
        }
        assert_eq!(from_wkt(WGS84).unwrap(), Crs::Geographic);
    }

    #[test]
    fn unknown_projections_and_datums_are_named() {
        let e = from_wkt(ALBERS).unwrap_err();
        assert!(e.contains("Albers projection"), "{e}");
        let e = from_wkt(NAD27_UTM).unwrap_err();
        assert!(e.contains("D_North_American_1927 datum"), "{e}");
        let e = from_wkt(&ESRI_26915.replace(r#"UNIT["Meter",1.0]"#, r#"UNIT["Link",0.201168]"#)).unwrap_err();
        assert!(e.contains("Link"), "{e}");
        assert!(from_wkt("not wkt at all").is_err());
        assert!(from_wkt(r#"PROJCRS["x",BASEGEOGCRS["y"]]"#).unwrap_err().contains("WKT2"));
    }

    #[test]
    fn epsg_codes_in_geojson_crs_names() {
        assert_eq!(epsg_in_name("urn:ogc:def:crs:EPSG::26915"), Some(26915));
        assert_eq!(epsg_in_name("EPSG:3417"), Some(3417));
        assert_eq!(epsg_in_name("urn:ogc:def:crs:OGC:1.3:CRS84"), Some(4326));
        let c = from_epsg(3417).unwrap();
        assert!(near(c.to_lonlat(4_888_646.767_925_305, 3_474_001.287_300_229_5), [-93.62, 42.03]));
        let c = from_epsg(26915).unwrap();
        assert!(near(c.to_lonlat(448_677.093_953_558_9, 4_653_293.017_557_551), [-93.62, 42.03]));
        assert!(from_epsg(5070).is_none());
    }
}
