//! Shapefile polygon layers: shapes from the `.shp`, attributes from the
//! `.dbf`, the projection from the `.prj`.

use std::io::Cursor;

use op_geo::LonLat;
use shapefile::dbase::FieldValue;
use shapefile::{Shape, ShapeReader};

use super::crs::{self, Crs};
use super::read::{RawFeature, assemble, fmt_num};

/// The layer's polygons, or `None` when it holds points or lines.
pub fn polygons(shp: &[u8], dbf: Option<&[u8]>, prj: Option<&str>, layer: &str) -> Result<Option<Vec<RawFeature>>, String> {
    let bad = |e: shapefile::Error| format!("{layer}.shp can't be read: {e}.");
    let reader = ShapeReader::new(Cursor::new(shp)).map_err(bad)?;
    let header = *reader.header();
    use shapefile::ShapeType as T;
    if !matches!(header.shape_type, T::Polygon | T::PolygonM | T::PolygonZ) {
        return Ok(None);
    }
    let crs = match prj.map(str::trim).filter(|p| !p.is_empty()) {
        Some(p) => crs::from_wkt(p).map_err(|e| format!("{layer}: {e}"))?,
        None => {
            let b = header.bbox;
            let geographic = [b.min.x, b.max.x].iter().all(|x| x.abs() <= 180.0) && [b.min.y, b.max.y].iter().all(|y| y.abs() <= 90.0);
            if !geographic {
                return Err(format!("{layer}.shp has no .prj file, so openpasture can't tell where its coordinates are. Include the .prj in the zip."));
            }
            Crs::Geographic
        }
    };

    let mut out = Vec::new();
    let mut push = |shape: Shape, attrs: Vec<(String, String)>| {
        let rings: Vec<Vec<LonLat>> = match shape {
            Shape::Polygon(p) => p.rings().iter().map(|r| r.points().iter().map(|q| crs.to_lonlat(q.x, q.y)).collect()).collect(),
            Shape::PolygonM(p) => p.rings().iter().map(|r| r.points().iter().map(|q| crs.to_lonlat(q.x, q.y)).collect()).collect(),
            Shape::PolygonZ(p) => p.rings().iter().map(|r| r.points().iter().map(|q| crs.to_lonlat(q.x, q.y)).collect()).collect(),
            _ => return,
        };
        let polygons = assemble(rings);
        if !polygons.is_empty() {
            out.push(RawFeature { layer: layer.to_owned(), name: None, attrs, polygons });
        }
    };

    match dbf {
        Some(dbf) => {
            let table = shapefile::dbase::Reader::new(Cursor::new(dbf)).map_err(|e| format!("{layer}.dbf can't be read: {e}."))?;
            let mut reader = shapefile::Reader::new(reader, table);
            for item in reader.iter_shapes_and_records() {
                let (shape, record) = item.map_err(bad)?;
                let mut attrs: Vec<(String, String)> = record.into_iter().filter_map(|(k, v)| Some((k, value_text(v)?))).collect();
                attrs.sort();
                push(shape, attrs);
            }
        }
        None => {
            let mut reader = reader;
            for shape in reader.iter_shapes() {
                push(shape.map_err(bad)?, Vec::new());
            }
        }
    }
    Ok(Some(out))
}

fn value_text(v: FieldValue) -> Option<String> {
    let s = match v {
        FieldValue::Character(s) => s?,
        FieldValue::Memo(s) => s,
        FieldValue::Numeric(n) => fmt_num(n?)?,
        FieldValue::Float(n) => fmt_num(n? as f64)?,
        FieldValue::Double(n) | FieldValue::Currency(n) => fmt_num(n)?,
        FieldValue::Integer(n) => n.to_string(),
        _ => return None,
    };
    let s = s.trim().to_owned();
    (!s.is_empty()).then_some(s)
}
