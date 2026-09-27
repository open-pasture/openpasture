#!/usr/bin/env python3
"""Writes the K-files import fixtures in this folder, and expected.json.

The projected shapefiles are made with PROJ (pyproj), independent of the Rust
inverse projections they test; areas are geodesic areas from PROJ's Geod on
the source lon/lat rings. Run from a venv with `pip install pyproj pyshp`:

    python make_fixtures.py

Geometry, attributes and times are deterministic (fixed seeds); pyshp stamps
each .dbf header with the day it was written.
"""

import io
import json
import math
import os
import random
import re
import zipfile
from datetime import datetime, timedelta, timezone

import shapefile  # pyshp
from pyproj import CRS, Geod, Transformer
from pyproj.enums import WktVersion

HERE = os.path.dirname(os.path.abspath(__file__))
GEOD = Geod(ellps="WGS84")
FIXED_ZIP_TIME = (2025, 6, 1, 0, 0, 0)


def ring_area_m2(ring):
    lons, lats = zip(*ring)
    area, _ = GEOD.polygon_area_perimeter(lons, lats)
    return abs(area)


def polygon_area_ha(rings):
    return (ring_area_m2(rings[0]) - sum(ring_area_m2(r) for r in rings[1:])) / 10000.0


def closed(ring):
    return ring if ring[0] == ring[-1] else ring + [ring[0]]


def ccw(ring):
    a = sum(x1 * y2 - x2 * y1 for (x1, y1), (x2, y2) in zip(ring, ring[1:] + ring[:1]))
    return ring if a > 0 else list(reversed(ring))


def cw(ring):
    return list(reversed(ccw(ring)))


def wobble(ring, seed, metres=6.0):
    """Densify each edge into 8 steps with a small deterministic wobble, so
    rings look surveyed rather than drawn."""
    rnd = random.Random(seed)
    out = []
    for (x1, y1), (x2, y2) in zip(ring, ring[1:] + ring[:1]):
        for k in range(8):
            t = k / 8
            x, y = x1 + (x2 - x1) * t, y1 + (y2 - y1) * t
            if k:
                x += rnd.uniform(-1, 1) * metres / (111320 * math.cos(math.radians(y)))
                y += rnd.uniform(-1, 1) * metres / 110540
            out.append((round(x, 7), round(y, 7)))
    return out


# ---------------------------------------------------------------- fields (WGS 84 lon/lat)

NORTH_40 = [
    wobble([(-93.6250, 42.0300), (-93.6200, 42.0300), (-93.6198, 42.0336), (-93.6224, 42.0345), (-93.6252, 42.0336)], 1),
    [(-93.6235, 42.0315), (-93.6222, 42.0313), (-93.6215, 42.0322), (-93.6228, 42.0327)],  # pond
]
CREEK = [wobble([(-93.6190, 42.0300), (-93.6150, 42.0300), (-93.6150, 42.0310), (-93.6170, 42.0310), (-93.6170, 42.0340), (-93.6190, 42.0340)], 2)]
HILLTOP = [
    [wobble([(-93.6140, 42.0300), (-93.6110, 42.0300), (-93.6110, 42.0320), (-93.6140, 42.0320)], 3)],
    [wobble([(-93.6140, 42.0330), (-93.6120, 42.0330), (-93.6120, 42.0345), (-93.6140, 42.0345)], 4)],
]
# Far from Iowa North's central meridian and 3.3° from UTM 15's: tests the series away from the middle.
WEST = [wobble([(-96.3000, 42.5000), (-96.2950, 42.5000), (-96.2950, 42.5036), (-96.3000, 42.5036)], 5)]

FIELDS = [
    # (attributes, polygons: each [outer, holes...])
    ({"FIELD_NAME": "North 40", "FARMNBR": 1234, "TRACTNBR": 5678, "CLUNBR": 3}, [NORTH_40]),
    ({"FIELD_NAME": "Creek bottom", "FARMNBR": 1234, "TRACTNBR": 5678, "CLUNBR": 4}, [CREEK]),
    ({"FIELD_NAME": "Hilltop", "FARMNBR": 1234, "TRACTNBR": 9012, "CLUNBR": 1}, HILLTOP),
    ({"FIELD_NAME": "West", "FARMNBR": 88, "TRACTNBR": 77, "CLUNBR": 12}, [WEST]),
]


def expected_drafts(fields, layer):
    out = []
    for attrs, polys in fields:
        for i, rings in enumerate(polys):
            name = attrs.get("FIELD_NAME") if attrs.get("FIELD_NAME") else None
            name = name if i == 0 else f"{name} {i + 1}"
            out.append({
                "name": name,
                "layer": layer,
                "area_ha": round(polygon_area_ha([closed(r) for r in rings]), 4),
                "rings": len(rings),
                "outer": [list(p) for p in rings[0]],
                "props": {k: str(v) for k, v in (("fsa_farm", attrs.get("FARMNBR")), ("fsa_tract", attrs.get("TRACTNBR")), ("fsa_field", attrs.get("CLUNBR"))) if v is not None},
            })
    return out


# ---------------------------------------------------------------- GeoJSON, KML, KMZ

def write_geojson():
    feats = []
    for attrs, polys in FIELDS:
        coords = [[[list(p) for p in closed(ccw(r) if j == 0 else cw(r))] for j, r in enumerate(rings)] for rings in polys]
        geom = {"type": "Polygon", "coordinates": coords[0]} if len(coords) == 1 else {"type": "MultiPolygon", "coordinates": coords}
        feats.append({"type": "Feature", "properties": attrs, "geometry": geom})
    feats.append({"type": "Feature", "properties": {"name": "East gate"}, "geometry": {"type": "Point", "coordinates": [-93.62, 42.0318]}})
    with open(os.path.join(HERE, "fields.geojson"), "w") as f:
        json.dump({"type": "FeatureCollection", "features": feats}, f, indent=1)


def kml_ring(r):
    return " ".join(f"{x},{y},0" for x, y in closed(r))


def kml_text():
    parts = ['<?xml version="1.0" encoding="UTF-8"?>', '<kml xmlns="http://www.opengis.net/kml/2.2">', "<Document><name>Smith farm</name>", "<Folder><name>Fields</name>"]
    for attrs, polys in FIELDS:
        data = "".join(f'<SimpleData name="{k}">{v}</SimpleData>' for k, v in attrs.items() if k != "FIELD_NAME")
        geoms = []
        for rings in polys:
            inner = "".join(f"<innerBoundaryIs><LinearRing><coordinates>{kml_ring(h)}</coordinates></LinearRing></innerBoundaryIs>" for h in rings[1:])
            geoms.append(f"<Polygon><outerBoundaryIs><LinearRing><coordinates>{kml_ring(rings[0])}</coordinates></LinearRing></outerBoundaryIs>{inner}</Polygon>")
        geom = geoms[0] if len(geoms) == 1 else "<MultiGeometry>" + "".join(geoms) + "</MultiGeometry>"
        parts.append(f"<Placemark><name>{attrs['FIELD_NAME']}</name><ExtendedData><SchemaData schemaUrl=\"#fields\">{data}</SchemaData></ExtendedData>{geom}</Placemark>")
    parts.append("<Placemark><name>East gate</name><Point><coordinates>-93.62,42.0318,0</coordinates></Point></Placemark>")
    parts += ["</Folder>", "</Document>", "</kml>"]
    return "\n".join(parts) + "\n"


def write_zip(path, entries):
    with zipfile.ZipFile(path, "w", zipfile.ZIP_DEFLATED) as z:
        for name, data in entries:
            info = zipfile.ZipInfo(name, FIXED_ZIP_TIME)
            info.compress_type = zipfile.ZIP_DEFLATED
            z.writestr(info, data)


# ---------------------------------------------------------------- shapefiles

def shp_bytes(shape_type, records, fields, to_xy=None):
    """(shp, shx, dbf) bytes for records [(attrs, geometry)]."""
    shp, shx, dbf = io.BytesIO(), io.BytesIO(), io.BytesIO()
    w = shapefile.Writer(shp=shp, shx=shx, dbf=dbf, shapeType=shape_type)
    for name, kind, size, dec in fields:
        w.field(name, kind, size=size, decimal=dec)
    t = to_xy or (lambda x, y: (x, y))
    for attrs, geom in records:
        if shape_type == shapefile.POLYGON:
            # ESRI order: outer rings clockwise, holes counter-clockwise.
            rings = []
            for poly in geom:
                for j, r in enumerate(poly):
                    rr = cw(r) if j == 0 else ccw(r)
                    rings.append([t(x, y) for x, y in closed(rr)])
            w.poly(rings)
        elif shape_type == shapefile.POINT:
            w.point(*t(*geom))
        else:
            w.line([[t(x, y) for x, y in geom]])
        w.record(*[attrs.get(f[0]) for f in fields])
    w.close()
    return shp.getvalue(), shx.getvalue(), dbf.getvalue()


FIELD_DEFS = [("FIELD_NAME", "C", 40, 0), ("FARMNBR", "N", 10, 0), ("TRACTNBR", "N", 10, 0), ("CLUNBR", "N", 6, 0)]


def projected(code):
    if code == 4326:
        return None, CRS.from_epsg(4326).to_wkt(WktVersion.WKT1_ESRI)
    tr = Transformer.from_crs(4269, code, always_xy=True)
    return (lambda x, y: tr.transform(x, y)), CRS.from_epsg(code).to_wkt(WktVersion.WKT1_ESRI)


def write_shp_zip(name, code, prj=True):
    to_xy, wkt = projected(code)
    shp, shx, dbf = shp_bytes(shapefile.POLYGON, FIELDS, FIELD_DEFS, to_xy)
    entries = [("fields.shp", shp), ("fields.shx", shx), ("fields.dbf", dbf)]
    if prj:
        entries.append(("fields.prj", wkt))
    write_zip(os.path.join(HERE, name), entries)


def write_jd_export():
    """Operations Center style: nested folders, a boundary per field, a
    waterway layer, points and lines that aren't paddocks, Mac junk."""
    wkt = CRS.from_epsg(4326).to_wkt(WktVersion.WKT1_ESRI)
    jd_fields = [("CLIENT_NAM", "C", 30, 0), ("FARM_NAME", "C", 30, 0), ("FIELD_NAME", "C", 30, 0)]
    base = "JD_Export/Smith Farms/Home"
    entries = []
    for field, polys in (("North 40", [NORTH_40]), ("Creek bottom", [CREEK])):
        shp, shx, dbf = shp_bytes(shapefile.POLYGON, [({"CLIENT_NAM": "Smith Farms", "FARM_NAME": "Home", "FIELD_NAME": field}, polys)], jd_fields)
        entries += [(f"{base}/{field}/Boundary.shp", shp), (f"{base}/{field}/Boundary.shx", shx), (f"{base}/{field}/Boundary.dbf", dbf), (f"{base}/{field}/Boundary.prj", wkt)]
    waterway = [[[(-93.6188, 42.0315), (-93.6178, 42.0315), (-93.6178, 42.0335), (-93.6188, 42.0335)]]]
    shp, shx, dbf = shp_bytes(shapefile.POLYGON, [({"TYPE": "Grassed waterway"}, waterway)], [("TYPE", "C", 30, 0)])
    entries += [(f"{base}/Creek bottom/Waterways.shp", shp), (f"{base}/Creek bottom/Waterways.shx", shx), (f"{base}/Creek bottom/Waterways.dbf", dbf), (f"{base}/Creek bottom/Waterways.prj", wkt)]
    shp, shx, dbf = shp_bytes(shapefile.POINT, [({"NAME": "Rock"}, (-93.6230, 42.0305))], [("NAME", "C", 20, 0)])
    entries += [(f"{base}/Flags.shp", shp), (f"{base}/Flags.shx", shx), (f"{base}/Flags.dbf", dbf), (f"{base}/Flags.prj", wkt)]
    shp, shx, dbf = shp_bytes(shapefile.POLYLINE, [({"NAME": "AB 1"}, [(-93.6250, 42.0301), (-93.6200, 42.0301)])], [("NAME", "C", 20, 0)])
    entries += [(f"{base}/Guidance.shp", shp), (f"{base}/Guidance.shx", shx), (f"{base}/Guidance.dbf", dbf)]
    entries += [("__MACOSX/JD_Export/Smith Farms/Home/North 40/._Boundary.shp", b"\x00\x05\x16\x07junk")]
    write_zip(os.path.join(HERE, "jd_export.zip"), entries)
    return [
        {"name": "Creek bottom", "layer": "Creek bottom/Boundary", "area_ha": round(polygon_area_ha([closed(r) for r in CREEK]), 4)},
        {"name": "North 40", "layer": "North 40/Boundary", "area_ha": round(polygon_area_ha([closed(r) for r in NORTH_40]), 4)},
        {"name": "Waterways", "layer": "Waterways", "area_ha": round(polygon_area_ha([closed(r) for r in waterway[0]]), 4)},
    ]


# ---------------------------------------------------------------- position history

P1 = (-93.6225, 42.0318)
P2 = (-93.6175, 42.0318)
OUTSIDE = (-93.6300, 42.0200)
DAY = datetime(2025, 6, 14, tzinfo=timezone.utc)
CHICAGO = timezone(timedelta(hours=-5))  # CDT in June


def walk(tag, seed, schedule):
    """Points every 15 min: schedule = [(start_offset_min, end_offset_min, place)]."""
    rnd = random.Random(seed)
    pts = []
    for start, end, place in schedule:
        for m in range(start, end, 15):
            x = place[0] + rnd.uniform(-1, 1) * 0.0012
            y = place[1] + rnd.uniform(-1, 1) * 0.0010
            pts.append((tag, DAY + timedelta(minutes=m), round(y, 6), round(x, 6), round(rnd.uniform(1.5, 4.5), 1)))
    return pts


def write_positions():
    # 214: day 1 in P1 until noon, then P2; day 2 in P2. 031: day 1 in P2, an hour outside, then P1.
    a214 = walk("214", 11, [(0, 720, P1), (720, 2880, P2)])
    a031 = walk("031", 12, [(0, 600, P2), (600, 660, OUTSIDE), (660, 1440, P1)])
    stray = walk("999", 13, [(0, 60, P1)])  # no such animal
    rows = sorted(a214 + a031 + stray, key=lambda r: (r[1], r[0]))
    with open(os.path.join(HERE, "positions_offset.csv"), "w") as f:
        f.write("tag,timestamp,lat,lon,accuracy\n")
        for i, (tag, t, lat, lon, acc) in enumerate(rows):
            ts = t.strftime("%Y-%m-%dT%H:%M:%SZ") if i % 2 == 0 else t.astimezone(CHICAGO).isoformat()
            f.write(f"{tag},{ts},{lat},{lon},{acc}\n")
    # The same 031 day written by a device that logs local time without an offset, US style, semicolons.
    with open(os.path.join(HERE, "positions_local.csv"), "w") as f:
        f.write("Animal ID;Date/Time;Latitude;Longitude\n")
        for tag, t, lat, lon, _ in a031:
            lt = t.astimezone(CHICAGO)
            f.write(f"{tag};{lt.month}/{lt.day}/{lt.year} {lt.hour}:{lt.minute:02d};{lat};{lon}\n")
    # GPX: one track per animal, named by its tag.
    a118 = walk("118", 14, [(0, 240, P2)])
    g = ['<?xml version="1.0" encoding="UTF-8"?>', '<gpx version="1.1" creator="fixture" xmlns="http://www.topografix.com/GPX/1/1">']
    for tag, pts in (("214", a214[:96]), ("118", a118)):
        g.append(f"<trk><name>{tag}</name><trkseg>")
        g += [f'<trkpt lat="{lat}" lon="{lon}"><time>{t.strftime("%Y-%m-%dT%H:%M:%SZ")}</time></trkpt>' for _, t, lat, lon, _ in pts]
        g.append("</trkseg></trk>")
    g.append("</gpx>")
    with open(os.path.join(HERE, "tracks.gpx"), "w") as f:
        f.write("\n".join(g) + "\n")
    # GeoJSON points with time and tag properties.
    feats = [{"type": "Feature", "properties": {"animal": tag, "time": t.strftime("%Y-%m-%dT%H:%M:%SZ"), "accuracy": acc}, "geometry": {"type": "Point", "coordinates": [lon, lat]}}
             for tag, t, lat, lon, acc in a118]
    with open(os.path.join(HERE, "points.geojson"), "w") as f:
        json.dump({"type": "FeatureCollection", "features": feats}, f, indent=1)
    first = lambda pts: pts[0][1].strftime("%Y-%m-%dT%H:%M:%SZ")
    last = lambda pts: pts[-1][1].strftime("%Y-%m-%dT%H:%M:%SZ")
    return {
        "positions_offset.csv": {"rows": len(rows), "labels": {"214": [len(a214), first(a214), last(a214)], "031": [len(a031), first(a031), last(a031)], "999": [len(stray), first(stray), last(stray)]}},
        "positions_local.csv": {"rows": len(a031), "labels": {"031": [len(a031), first(a031), last(a031)]}},
        "tracks.gpx": {"rows": 96 + len(a118), "labels": {"214": [96, first(a214[:96]), last(a214[:96])], "118": [len(a118), first(a118), last(a118)]}},
        "points.geojson": {"rows": len(a118), "labels": {"118": [len(a118), first(a118), last(a118)]}},
    }


def main():
    write_geojson()
    kml = kml_text()
    with open(os.path.join(HERE, "fields.kml"), "w") as f:
        f.write(kml)
    write_zip(os.path.join(HERE, "fields.kmz"), [("doc.kml", kml)])
    shp_zips = {"wgs84_shp.zip": 4326, "utm15n_shp.zip": 26915, "iowa_north_ftus_shp.zip": 3417, "iowa_north_m_shp.zip": 26975, "iowa_south_ftus_shp.zip": 3418}
    for name, code in shp_zips.items():
        write_shp_zip(name, code)
    write_shp_zip("albers_shp.zip", 5070)
    write_shp_zip("nad27_utm_shp.zip", 26715)
    write_shp_zip("no_prj_utm_shp.zip", 26915, prj=False)
    expected = {
        "paddocks": {
            "fields.geojson": expected_drafts(FIELDS, "fields"),
            "fields.kml": expected_drafts(FIELDS, "Fields"),
            "fields.kmz": expected_drafts(FIELDS, "Fields"),
            **{name: expected_drafts(FIELDS, "fields") for name in shp_zips},
            "jd_export.zip": write_jd_export(),
        },
        "positions": write_positions(),
    }
    text = json.dumps(expected, indent=1)
    # Coordinate pairs on one line each.
    text = re.sub(r"\[\s+(-?[\d.]+),\s+(-?[\d.]+)\s+\]", r"[\1, \2]", text)
    with open(os.path.join(HERE, "expected.json"), "w") as f:
        f.write(text + "\n")


if __name__ == "__main__":
    main()
