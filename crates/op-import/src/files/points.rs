//! Position history files to labelled, timed points: CSV (columns chosen by
//! a mapping: tag, time, lat, lon, accuracy), GPX (one track per animal,
//! named by its tag) and GeoJSON Points (time and tag from properties).
//! Timestamps without an offset are read in a time zone the farmer picks.

use std::collections::HashMap;
use std::io::Cursor;

use chrono::{DateTime, LocalResult, NaiveDateTime, TimeZone, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};

use super::geojson;

/// Which column (CSV) or property (GeoJSON) holds each field. `tag` absent =
/// the whole file is one animal, labelled by the file name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Mapping {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub time: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lat: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lon: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub accuracy: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Csv,
    Gpx,
    Geojson,
}

impl Source {
    pub fn as_str(self) -> &'static str {
        match self {
            Source::Csv => "csv",
            Source::Gpx => "gpx",
            Source::Geojson => "geojson",
        }
    }
}

/// One point: index into `labels`, unix ms, where, accuracy in metres.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pt {
    pub label: u32,
    pub t: i64,
    pub lon: f64,
    pub lat: f64,
    pub accuracy_m: Option<f64>,
}

#[derive(Debug, Default)]
pub struct Parsed {
    pub source: Option<Source>,
    /// CSV headers, or the GeoJSON properties seen.
    pub columns: Vec<String>,
    pub mapping: Mapping,
    /// The first rows of a CSV, as text.
    pub rows: Vec<Vec<String>>,
    /// Rows (CSV), track points (GPX) or point features (GeoJSON) in the file.
    pub total: usize,
    pub points: Vec<Pt>,
    pub labels: Vec<String>,
    /// Some timestamps had no offset and were read in the chosen zone.
    pub needs_zone: bool,
    pub errors: Vec<String>,
}

const MAX_ERRORS: usize = 20;
const PREVIEW_ROWS: usize = 20;

struct Errors {
    list: Vec<String>,
    more: usize,
}

impl Errors {
    fn push(&mut self, e: String) {
        if self.list.len() < MAX_ERRORS {
            self.list.push(e);
        } else {
            self.more += 1;
        }
    }
    fn done(mut self) -> Vec<String> {
        if self.more > 0 {
            self.list.push(format!("{} more rows couldn't be read.", self.more));
        }
        self.list
    }
}

/// What kind of position file this is, by content.
pub fn sniff(file_name: &str, bytes: &[u8]) -> Result<Source, String> {
    let text = std::str::from_utf8(bytes).map_err(|_| "This isn't a text file. Import CSV, GPX or GeoJSON points.".to_owned())?;
    let t = text.trim_start_matches('\u{feff}').trim_start();
    if t.starts_with('<') {
        if t.contains("<gpx") {
            return Ok(Source::Gpx);
        }
        return Err("This XML isn't GPX. Import CSV, GPX or GeoJSON points.".to_owned());
    }
    if t.starts_with('{') {
        return Ok(Source::Geojson);
    }
    let lower = file_name.to_ascii_lowercase();
    if lower.ends_with(".csv")
        || lower.ends_with(".txt")
        || lower.ends_with(".tsv")
        || t.lines().next().is_some_and(|l| l.contains(',') || l.contains(';') || l.contains('\t'))
    {
        return Ok(Source::Csv);
    }
    Err("This isn't a file openpasture can read. Import CSV, GPX or GeoJSON points.".to_owned())
}

pub fn parse(file_name: &str, bytes: &[u8], mapping: Option<&Mapping>, zone: Tz) -> Result<Parsed, String> {
    let source = sniff(file_name, bytes)?;
    let text = std::str::from_utf8(bytes).map_err(|_| "This isn't a text file.".to_owned())?.trim_start_matches('\u{feff}');
    let mut p = match source {
        Source::Csv => csv(text, mapping, zone, &stem(file_name))?,
        Source::Gpx => gpx(text, &stem(file_name))?,
        Source::Geojson => geojson_points(text, mapping, zone, &stem(file_name))?,
    };
    p.source = Some(source);
    Ok(p)
}

fn stem(path: &str) -> String {
    let base = path.rsplit(['/', '\\']).next().unwrap_or(path);
    let s = base.rsplit_once('.').map(|(s, _)| s).unwrap_or(base).trim();
    if s.is_empty() { "Track".to_owned() } else { s.to_owned() }
}

// ---------------------------------------------------------------- mapping guesses

const TAG: &[&str] = &[
    "tag",
    "animaltag",
    "animal",
    "animalid",
    "eartag",
    "visualid",
    "vid",
    "cow",
    "collar",
    "collarid",
    "collarname",
    "device",
    "deviceid",
    "deviceserial",
    "serial",
    "label",
    "name",
    "id",
    "eid",
    "rfid",
];
const TIME: &[&str] =
    &["time", "timestamp", "datetime", "datetimeutc", "timeutc", "utc", "gpstime", "fixtime", "recordedat", "localtime", "createdat", "date", "at", "t"];
const LAT: &[&str] = &["lat", "latitude", "gpslat", "gpslatitude", "y"];
const LON: &[&str] = &["lon", "lng", "long", "longitude", "gpslon", "gpslong", "gpslongitude", "x"];
const ACCURACY: &[&str] = &["accuracy", "accuracym", "hacc", "horizontalaccuracy", "acc", "hpe", "precision", "errorm"];

fn norm(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

fn guess(columns: &[String], keys: &[&str], taken: &[String]) -> Option<String> {
    let free = |c: &&String| !taken.contains(c);
    for k in keys {
        if let Some(c) = columns.iter().filter(free).find(|c| norm(c) == *k) {
            return Some(c.clone());
        }
    }
    for k in keys.iter().filter(|k| k.len() >= 3) {
        if let Some(c) = columns.iter().filter(free).find(|c| norm(c).starts_with(k)) {
            return Some(c.clone());
        }
    }
    None
}

/// Guess each field's column from the headers.
pub fn guess_mapping(columns: &[String], with_position: bool) -> Mapping {
    let mut taken: Vec<String> = Vec::new();
    let mut pick = |keys: &[&str]| {
        let c = guess(columns, keys, &taken);
        if let Some(c) = &c {
            taken.push(c.clone());
        }
        c
    };
    let time = pick(TIME);
    let (lat, lon) = if with_position { (pick(LAT), pick(LON)) } else { (None, None) };
    let accuracy = pick(ACCURACY);
    let tag = pick(TAG);
    Mapping { tag, time, lat, lon, accuracy }
}

// ---------------------------------------------------------------- times

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum When {
    Utc(DateTime<Utc>),
    Local(NaiveDateTime),
}

const NAIVE: &[&str] = &[
    "%Y-%m-%dT%H:%M:%S%.f",
    "%Y-%m-%d %H:%M:%S%.f",
    "%Y-%m-%dT%H:%M",
    "%Y-%m-%d %H:%M",
    "%Y/%m/%d %H:%M:%S%.f",
    "%Y/%m/%d %H:%M",
    "%m/%d/%Y %H:%M:%S%.f",
    "%m/%d/%Y %H:%M",
    "%m/%d/%Y %I:%M:%S %p",
    "%m/%d/%Y %I:%M %p",
    "%d/%m/%Y %H:%M:%S%.f",
    "%d/%m/%Y %H:%M",
    "%m/%d/%y %H:%M:%S",
    "%m/%d/%y %H:%M",
    "%d.%m.%Y %H:%M:%S",
    "%d.%m.%Y %H:%M",
];
const OFFSET: &[&str] = &["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%z", "%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%d %H:%M%:z", "%Y-%m-%dT%H:%M%:z"];

/// A timestamp as devices write them: RFC 3339, ISO with or without offset,
/// US `M/D/YYYY h:mm`, day-first when the month can't be, or unix seconds or ms.
pub fn parse_time(s: &str) -> Option<When> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if s.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        let v: f64 = s.parse().ok()?;
        let ms = if v > 1e11 { v } else { v * 1000.0 };
        let t = DateTime::<Utc>::from_timestamp_millis(ms.round() as i64)?;
        return (946_684_800_000..4_102_444_800_000).contains(&t.timestamp_millis()).then_some(When::Utc(t));
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Some(When::Utc(t.with_timezone(&Utc)));
    }
    let (body, utc) = match s.strip_suffix(" UTC").or_else(|| s.strip_suffix(" GMT")).or_else(|| s.strip_suffix('Z')) {
        Some(b) => (b.trim_end(), true),
        None => (s, false),
    };
    if !utc {
        for f in OFFSET {
            if let Ok(t) = DateTime::parse_from_str(body, f) {
                return Some(When::Utc(t.with_timezone(&Utc)));
            }
        }
    }
    for f in NAIVE {
        if let Ok(n) = NaiveDateTime::parse_from_str(body, f) {
            return Some(if utc { When::Utc(n.and_utc()) } else { When::Local(n) });
        }
    }
    None
}

/// The instant a parsed time names, reading local times in `zone`.
pub fn resolve(w: When, zone: Tz) -> Result<DateTime<Utc>, String> {
    match w {
        When::Utc(t) => Ok(t),
        When::Local(n) => match zone.from_local_datetime(&n) {
            LocalResult::Single(t) => Ok(t.with_timezone(&Utc)),
            LocalResult::Ambiguous(a, _) => Ok(a.with_timezone(&Utc)),
            LocalResult::None => Err(format!("{} doesn't exist in {zone} (the clocks skipped it).", n.format("%Y-%m-%d %H:%M"))),
        },
    }
}

// ---------------------------------------------------------------- building points

struct Builder {
    labels: Vec<String>,
    index: HashMap<String, u32>,
    points: Vec<Pt>,
    errors: Errors,
    needs_zone: bool,
    zone: Tz,
}

impl Builder {
    fn new(zone: Tz) -> Self {
        Self { labels: Vec::new(), index: HashMap::new(), points: Vec::new(), errors: Errors { list: Vec::new(), more: 0 }, needs_zone: false, zone }
    }

    /// Add one point; `row` names it in errors ("Row 12").
    fn add(&mut self, row: &str, label: &str, time: &str, lon: Option<f64>, lat: Option<f64>, accuracy: Option<f64>) {
        let label = label.trim();
        if label.is_empty() {
            return self.errors.push(format!("{row}: no tag."));
        }
        let Some(w) = parse_time(time) else {
            return self.errors.push(format!("{row}: \"{}\" isn't a time.", time.trim()));
        };
        if matches!(w, When::Local(_)) {
            self.needs_zone = true;
        }
        let t = match resolve(w, self.zone) {
            Ok(t) => t,
            Err(e) => return self.errors.push(format!("{row}: {e}")),
        };
        let (Some(lon), Some(lat)) = (lon, lat) else {
            return self.errors.push(format!("{row}: no position."));
        };
        if !(lon.is_finite() && lat.is_finite() && lon.abs() <= 180.0 && lat.abs() <= 90.0) || (lon == 0.0 && lat == 0.0) {
            return self.errors.push(format!("{row}: no position."));
        }
        let i = match self.index.get(label) {
            Some(i) => *i,
            None => {
                let i = self.labels.len() as u32;
                self.labels.push(label.to_owned());
                self.index.insert(label.to_owned(), i);
                i
            }
        };
        let accuracy_m = accuracy.filter(|a| a.is_finite() && *a >= 0.0);
        self.points.push(Pt { label: i, t: t.timestamp_millis(), lon, lat, accuracy_m });
    }

    fn finish(self, mut p: Parsed) -> Parsed {
        p.labels = self.labels;
        p.points = self.points;
        p.needs_zone = self.needs_zone;
        p.errors = self.errors.done();
        p
    }
}

fn num(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    s.parse().ok().or_else(|| s.replace(',', ".").parse().ok())
}

fn csv(text: &str, mapping: Option<&Mapping>, zone: Tz, stem: &str) -> Result<Parsed, String> {
    let first = text.lines().next().unwrap_or_default();
    let delim = [b',', b';', b'\t'].into_iter().max_by_key(|d| first.bytes().filter(|b| b == d).count()).unwrap_or(b',');
    let mut rd = ::csv::ReaderBuilder::new().delimiter(delim).flexible(true).trim(::csv::Trim::All).from_reader(Cursor::new(text.as_bytes()));
    let columns: Vec<String> = rd.headers().map_err(|e| format!("The CSV can't be read: {e}."))?.iter().map(str::to_owned).collect();
    if columns.iter().all(String::is_empty) {
        return Err("The CSV has no header row.".to_owned());
    }
    let mapping = mapping.cloned().unwrap_or_else(|| guess_mapping(&columns, true));
    let col = |name: &Option<String>| name.as_ref().and_then(|n| columns.iter().position(|c| c == n));
    let (tag, time, lat, lon, acc) = (col(&mapping.tag), col(&mapping.time), col(&mapping.lat), col(&mapping.lon), col(&mapping.accuracy));
    let mut b = Builder::new(zone);
    let mut rows = Vec::new();
    let mut total = 0;
    for (i, rec) in rd.records().enumerate() {
        let rec = rec.map_err(|e| format!("The CSV can't be read: {e}."))?;
        if rec.iter().all(str::is_empty) {
            continue;
        }
        total += 1;
        if rows.len() < PREVIEW_ROWS {
            rows.push(rec.iter().map(str::to_owned).collect());
        }
        let (Some(time), Some(lat), Some(lon)) = (time, lat, lon) else { continue };
        let get = |c: usize| rec.get(c).unwrap_or_default();
        let label = tag.map(get).unwrap_or(stem);
        b.add(&format!("Row {}", i + 2), label, get(time), num(get(lon)), num(get(lat)), acc.and_then(|c| num(get(c))));
    }
    let missing: Vec<&str> = [("time", time), ("lat", lat), ("lon", lon)].iter().filter(|(_, c)| c.is_none()).map(|(n, _)| *n).collect();
    let mut p = b.finish(Parsed { columns, mapping, rows, total, ..Default::default() });
    if !missing.is_empty() {
        p.errors.insert(0, format!("Choose the {} column.", missing.join(", ")));
    }
    Ok(p)
}

fn gpx(text: &str, stem: &str) -> Result<Parsed, String> {
    let doc = ::gpx::read(Cursor::new(text.as_bytes())).map_err(|e| format!("The GPX can't be read: {e}."))?;
    let unnamed = doc.tracks.iter().filter(|t| t.name.as_deref().is_none_or(|n| n.trim().is_empty())).count();
    let mut b = Builder::new(chrono_tz::UTC);
    let mut total = 0;
    let mut n_unnamed = 0;
    for track in &doc.tracks {
        let label = match track.name.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            Some(n) => n.to_owned(),
            None => {
                n_unnamed += 1;
                if unnamed > 1 { format!("{stem} {n_unnamed}") } else { stem.to_owned() }
            }
        };
        for seg in &track.segments {
            for (i, w) in seg.points.iter().enumerate() {
                total += 1;
                let p = w.point();
                let time = w.time.as_ref().and_then(|t| t.format().ok()).unwrap_or_default();
                b.add(&format!("{label} point {}", i + 1), &label, &time, Some(p.x()), Some(p.y()), None);
            }
        }
    }
    Ok(b.finish(Parsed { total, ..Default::default() }))
}

fn prop_text(v: Option<&serde_json::Value>) -> Option<String> {
    match v? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

fn geojson_points(text: &str, mapping: Option<&Mapping>, zone: Tz, stem: &str) -> Result<Parsed, String> {
    let feats = geojson::points(text)?;
    let mut columns: Vec<String> = Vec::new();
    for f in &feats {
        for k in f.props.keys() {
            if !columns.contains(k) {
                columns.push(k.clone());
            }
        }
    }
    let mapping = mapping.cloned().unwrap_or_else(|| guess_mapping(&columns, false));
    let mut b = Builder::new(zone);
    for (i, f) in feats.iter().enumerate() {
        let get = |k: &Option<String>| k.as_ref().and_then(|k| prop_text(f.props.get(k)));
        let label = if mapping.tag.is_some() { get(&mapping.tag).unwrap_or_default() } else { stem.to_owned() };
        let acc = get(&mapping.accuracy).and_then(|s| num(&s));
        b.add(&format!("Point {}", i + 1), &label, &get(&mapping.time).unwrap_or_default(), Some(f.at[0]), Some(f.at[1]), acc);
    }
    let mut p = b.finish(Parsed { columns, mapping: mapping.clone(), total: feats.len(), ..Default::default() });
    if feats.is_empty() {
        return Err("The GeoJSON has no points.".to_owned());
    }
    if mapping.time.is_none() {
        p.errors.insert(0, "Choose the time property.".to_owned());
    }
    Ok(p)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn times_with_and_without_offsets() {
        let chicago: Tz = "America/Chicago".parse().unwrap();
        let at = |s: &str| resolve(parse_time(s).unwrap_or_else(|| panic!("{s}")), chicago).unwrap();
        assert_eq!(at("2025-06-14T12:05:00Z"), utc("2025-06-14T12:05:00Z"));
        assert_eq!(at("2025-06-14T07:05:00-05:00"), utc("2025-06-14T12:05:00Z"));
        assert_eq!(at("2025-06-14 07:05:00-0500"), utc("2025-06-14T12:05:00Z"));
        assert_eq!(at("2025-06-14 12:05:00 UTC"), utc("2025-06-14T12:05:00Z"));
        assert_eq!(at("1749902700"), utc("2025-06-14T12:05:00Z"));
        assert_eq!(at("1749902700000"), utc("2025-06-14T12:05:00Z"));
        // Local times (CDT = UTC-5 in June).
        assert_eq!(at("2025-06-14 07:05:00"), utc("2025-06-14T12:05:00Z"));
        assert_eq!(at("2025-06-14 07:05:00.250"), utc("2025-06-14T12:05:00.250Z"));
        assert_eq!(at("2025-06-14T07:05"), utc("2025-06-14T12:05:00Z"));
        assert_eq!(at("6/14/2025 7:05"), utc("2025-06-14T12:05:00Z"));
        assert_eq!(at("6/14/2025 7:05:00 AM"), utc("2025-06-14T12:05:00Z"));
        assert_eq!(at("14/06/2025 07:05"), utc("2025-06-14T12:05:00Z"));
        assert!(matches!(parse_time("2025-06-14 07:05:00"), Some(When::Local(_))));
        assert!(parse_time("yesterday").is_none());
        // 02:30 on the spring-forward night doesn't exist in Chicago.
        assert!(resolve(parse_time("2025-03-09 02:30").unwrap(), chicago).is_err());
    }

    #[test]
    fn csv_headers_are_guessed() {
        let cols: Vec<String> = ["Animal ID", "Date/Time", "Latitude (deg)", "Longitude (deg)", "HAcc"].iter().map(|s| s.to_string()).collect();
        let m = guess_mapping(&cols, true);
        assert_eq!(m.tag.as_deref(), Some("Animal ID"));
        assert_eq!(m.time.as_deref(), Some("Date/Time"));
        assert_eq!(m.lat.as_deref(), Some("Latitude (deg)"));
        assert_eq!(m.lon.as_deref(), Some("Longitude (deg)"));
        assert_eq!(m.accuracy.as_deref(), Some("HAcc"));
    }

    #[test]
    fn csv_rows_become_points_and_bad_rows_errors() {
        let text =
            "tag;time;lat;lon\n214;2025-06-14 07:05;42.031;-93.622\n214;2025-06-14 07:10;42,0312;-93,6221\n031;nope;42.03;-93.62\n031;2025-06-14 07:10;0;0\n";
        let p = parse("x.csv", text.as_bytes(), None, "America/Chicago".parse().unwrap()).unwrap();
        assert_eq!(p.total, 4);
        assert_eq!(p.points.len(), 2);
        assert!(p.needs_zone);
        assert_eq!(p.labels, vec!["214"]);
        assert_eq!(p.errors, vec!["Row 4: \"nope\" isn't a time.".to_owned(), "Row 5: no position.".to_owned()]);
        assert!((p.points[1].lat - 42.0312).abs() < 1e-9);
    }
}
