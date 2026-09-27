//! KML Placemarks with polygons (also inside MultiGeometry). The name comes
//! from the Placemark, attributes from ExtendedData (`Data`/`value` and
//! `SchemaData`/`SimpleData`), the layer from the innermost named Folder.

use op_geo::LonLat;
use quick_xml::Reader;
use quick_xml::events::Event;

use super::read::RawFeature;

#[derive(Default)]
struct Placemark {
    name: Option<String>,
    attrs: Vec<(String, String)>,
    polygons: Vec<Vec<Vec<LonLat>>>,
    /// The polygon being read: outer ring first, then holes.
    open: Option<(Option<Vec<LonLat>>, Vec<Vec<LonLat>>)>,
}

fn coordinates(text: &str) -> Result<Vec<LonLat>, String> {
    text.split_whitespace()
        .map(|t| {
            let mut it = t.split(',');
            let lon = it.next().and_then(|v| v.trim().parse::<f64>().ok());
            let lat = it.next().and_then(|v| v.trim().parse::<f64>().ok());
            match (lon, lat) {
                (Some(lon), Some(lat)) if lon.abs() <= 180.0 && lat.abs() <= 90.0 => Ok([lon, lat]),
                _ => Err(format!("The KML has a coordinate openpasture can't read: \"{t}\".")),
            }
        })
        .collect()
}

fn entity(name: &str) -> Option<char> {
    Some(match name {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        _ => return None,
    })
}

pub fn polygons(text: &str, layer: &str) -> Result<Vec<RawFeature>, String> {
    let mut reader = Reader::from_str(text);
    let bad = |e: quick_xml::Error| format!("The KML can't be read: {e}.");
    let mut stack: Vec<String> = Vec::new();
    // Folder and Document names, innermost last (None until its <name> is read).
    let mut folders: Vec<Option<String>> = Vec::new();
    let mut place: Option<Placemark> = None;
    let mut data_name: Option<String> = None;
    let mut text_buf = String::new();
    let mut out = Vec::new();
    let mut saw_kml = false;

    loop {
        match reader.read_event().map_err(bad)? {
            Event::Start(e) => {
                let tag = e.local_name().as_ref().to_owned();
                text_buf.clear();
                match tag.as_str() {
                    "kml" => saw_kml = true,
                    "Folder" | "Document" => folders.push(None),
                    "Placemark" => place = Some(Placemark::default()),
                    "Polygon" => {
                        if let Some(p) = place.as_mut() {
                            p.open = Some((None, Vec::new()));
                        }
                    }
                    "Data" | "SimpleData" => {
                        data_name = e
                            .attributes()
                            .flatten()
                            .find(|a| a.key.local_name().as_ref() == "name")
                            .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()));
                    }
                    _ => {}
                }
                stack.push(tag);
            }
            Event::Empty(e) => {
                if e.local_name().as_ref() == "kml" {
                    saw_kml = true;
                }
            }
            Event::Text(t) => text_buf.push_str(&t.xml10_content()),
            Event::CData(t) => text_buf.push_str(&t.into_inner()),
            Event::GeneralRef(r) => {
                if let Ok(Some(c)) = r.resolve_char_ref() {
                    text_buf.push(c);
                } else if let Some(c) = entity(&r.xml10_content()) {
                    text_buf.push(c);
                }
            }
            Event::End(_) => {
                let Some(tag) = stack.pop() else { continue };
                let parent = stack.last().map(String::as_str);
                let value = text_buf.trim().to_owned();
                match tag.as_str() {
                    "name" => match parent {
                        Some("Placemark") => {
                            if let Some(p) = place.as_mut() {
                                p.name = Some(value.clone()).filter(|v| !v.is_empty());
                            }
                        }
                        Some("Folder" | "Document") => {
                            if let Some(slot) = folders.last_mut() {
                                *slot = Some(value.clone()).filter(|v| !v.is_empty());
                            }
                        }
                        _ => {}
                    },
                    "value" if parent == Some("Data") => {
                        if let (Some(p), Some(k)) = (place.as_mut(), data_name.clone())
                            && !value.is_empty()
                        {
                            p.attrs.push((k, value.clone()));
                        }
                    }
                    "SimpleData" => {
                        if let (Some(p), Some(k)) = (place.as_mut(), data_name.take())
                            && !value.is_empty()
                        {
                            p.attrs.push((k, value.clone()));
                        }
                    }
                    "coordinates" => {
                        let inner = stack.iter().any(|t| t == "innerBoundaryIs");
                        let outer = stack.iter().any(|t| t == "outerBoundaryIs");
                        if let Some((o, holes)) = place.as_mut().and_then(|p| p.open.as_mut())
                            && (inner || outer)
                        {
                            let ring = coordinates(&value)?;
                            if inner {
                                holes.push(ring);
                            } else {
                                *o = Some(ring);
                            }
                        }
                    }
                    "Polygon" => {
                        if let Some(p) = place.as_mut()
                            && let Some((Some(outer), holes)) = p.open.take()
                        {
                            let mut rings = vec![outer];
                            rings.extend(holes);
                            p.polygons.push(rings);
                        }
                    }
                    "Placemark" => {
                        if let Some(p) = place.take()
                            && !p.polygons.is_empty()
                        {
                            let layer = folders.iter().rev().flatten().next().cloned().unwrap_or_else(|| layer.to_owned());
                            out.push(RawFeature { layer, name: p.name, attrs: p.attrs, polygons: p.polygons });
                        }
                    }
                    "Folder" | "Document" => {
                        folders.pop();
                    }
                    _ => {}
                }
                text_buf.clear();
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if !saw_kml {
        return Err("This XML isn't KML.".to_owned());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placemarks_folders_holes_and_extended_data() {
        let kml = r##"<?xml version="1.0" encoding="UTF-8"?>
<kml xmlns="http://www.opengis.net/kml/2.2"><Document><name>Farm</name>
<Folder><name>Home &amp; East</name>
  <Placemark><name><![CDATA[North 40]]></name>
    <ExtendedData><SchemaData schemaUrl="#s"><SimpleData name="FARMNBR">1234</SimpleData></SchemaData>
      <Data name="TRACT_NBR"><value>5678</value></Data></ExtendedData>
    <Polygon><outerBoundaryIs><LinearRing><coordinates>
      -93.625,42.03,0 -93.62,42.03,0 -93.62,42.0336,0 -93.625,42.0336,0 -93.625,42.03,0
    </coordinates></LinearRing></outerBoundaryIs>
    <innerBoundaryIs><LinearRing><coordinates>-93.6235,42.0315 -93.6215,42.0315 -93.6215,42.0325 -93.6235,42.0315</coordinates></LinearRing></innerBoundaryIs>
    </Polygon></Placemark>
  <Placemark><name>Gate</name><Point><coordinates>-93.62,42.03</coordinates></Point></Placemark>
</Folder>
<Placemark><MultiGeometry>
  <Polygon><outerBoundaryIs><LinearRing><coordinates>-93.62,42.03 -93.615,42.03 -93.615,42.0336 -93.62,42.03</coordinates></LinearRing></outerBoundaryIs></Polygon>
  <Polygon><outerBoundaryIs><LinearRing><coordinates>-93.61,42.03 -93.605,42.03 -93.605,42.0336 -93.61,42.03</coordinates></LinearRing></outerBoundaryIs></Polygon>
</MultiGeometry></Placemark>
</Document></kml>"##;
        let got = polygons(kml, "file").unwrap();
        assert_eq!(got.len(), 2, "the point placemark is left out");
        assert_eq!(got[0].name.as_deref(), Some("North 40"));
        assert_eq!(got[0].layer, "Home & East");
        assert_eq!(got[0].polygons[0].len(), 2);
        assert_eq!(got[0].attrs, vec![("FARMNBR".to_owned(), "1234".to_owned()), ("TRACT_NBR".to_owned(), "5678".to_owned())]);
        assert_eq!(got[1].layer, "Farm");
        assert_eq!(got[1].polygons.len(), 2);
        assert!(polygons("<gpx></gpx>", "x").is_err());
    }
}
