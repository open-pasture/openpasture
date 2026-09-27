//! Paddock files (GeoJSON, KML, KMZ, shapefiles in geographic, UTM and Iowa
//! state plane coordinates, nested John Deere zips) and position history
//! (CSV with and without offsets, GPX, GeoJSON points) through the REST API.
//! Fixtures and `expected.json` are written by
//! `tests/fixtures/files/make_fixtures.py` with PROJ, independent of the
//! projection code under test.

use std::path::PathBuf;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Ctx, Event, Identity, Via};
use serde_json::{Value, json};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
}

fn fixture(name: &str) -> Vec<u8> {
    let p = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/files").join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn expected() -> Value {
    serde_json::from_slice(&fixture("expected.json")).unwrap()
}

const BOUNDARY: &str = "----k-files-test-boundary";

fn multipart(file_name: &str, bytes: &[u8], fields: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (k, v) in fields {
        body.extend_from_slice(format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"{k}\"\r\n\r\n{v}\r\n").as_bytes());
    }
    body.extend_from_slice(
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: application/octet-stream\r\n\r\n")
            .as_bytes(),
    );
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let routes = op_core::router().merge(op_import::router());
        let router = op_core::with_identity(routes, Identity::owner(Via::Local)).with_state(ctx.clone());
        Self { _dir: dir, ctx, router }
    }

    async fn send(&self, req: Request<Body>) -> (StatusCode, Value) {
        let res = self.router.clone().oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        let v = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        (status, v)
    }

    async fn call(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let mut req = Request::builder().method(method).uri(path);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        self.send(req.body(body).unwrap()).await
    }

    async fn upload(&self, path: &str, file_name: &str, bytes: &[u8], fields: &[(&str, &str)]) -> (StatusCode, Value) {
        let req = Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", format!("multipart/form-data; boundary={BOUNDARY}"))
            .body(Body::from(multipart(file_name, bytes, fields)))
            .unwrap();
        self.send(req).await
    }

    async fn preview_paddocks(&self, name: &str) -> (StatusCode, Value) {
        self.upload("/api/import/paddocks/preview", name, &fixture(name), &[]).await
    }

    /// The §6 farm: P1 and P2 side by side near Ames, herd Cows, animals 214, 031 and 118.
    async fn farm(&self) -> Farm {
        let (s, _) = self.call("POST", "/api/farm", Some(json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]}))).await;
        assert_eq!(s, StatusCode::CREATED);
        let pad = |name: &str, w: f64, e: f64| json!({"name": name, "geometry": {"type": "Polygon", "coordinates": [[[w, 42.03], [e, 42.03], [e, 42.0336], [w, 42.0336], [w, 42.03]]]}});
        let (_, p1) = self.call("POST", "/api/paddocks", Some(pad("P1", -93.625, -93.62))).await;
        let (_, p2) = self.call("POST", "/api/paddocks", Some(pad("P2", -93.62, -93.615))).await;
        let (s, herd) = self.call("POST", "/api/herds", Some(json!({"name": "Cows", "species": "cattle", "count": 3, "paddock_id": p1["id"]}))).await;
        assert_eq!(s, StatusCode::CREATED, "{herd}");
        let herd = herd["id"].as_str().unwrap().to_owned();
        let mut animals = std::collections::HashMap::new();
        for tag in ["214", "031", "118"] {
            let (s, a) = self.call("POST", "/api/animals", Some(json!({"tag": tag, "herd_id": herd}))).await;
            assert_eq!(s, StatusCode::CREATED, "{a}");
            animals.insert(tag.to_owned(), a["id"].as_str().unwrap().to_owned());
        }
        Farm { p1: p1["id"].as_str().unwrap().to_owned(), p2: p2["id"].as_str().unwrap().to_owned(), herd, animals }
    }
}

struct Farm {
    p1: String,
    p2: String,
    herd: String,
    animals: std::collections::HashMap<String, String>,
}

/// Metres between two lon/lat points (equirectangular; fine at field scale).
fn metres(a: [f64; 2], b: [f64; 2]) -> f64 {
    let k = (a[1].to_radians()).cos();
    ((a[0] - b[0]) * k * 111_320.0).hypot((a[1] - b[1]) * 110_540.0)
}

fn pt(v: &Value) -> [f64; 2] {
    [v[0].as_f64().unwrap(), v[1].as_f64().unwrap()]
}

#[tokio::test]
async fn paddock_files_import_with_correct_areas_in_every_format_and_projection() {
    let app = App::new().await;
    let exp = expected();
    for (file, want) in exp["paddocks"].as_object().unwrap() {
        if file == "jd_export.zip" {
            continue;
        }
        let (s, got) = app.preview_paddocks(file).await;
        assert_eq!(s, StatusCode::OK, "{file}: {got}");
        assert!(got["import_id"].as_str().unwrap().starts_with("imp_"));
        assert_eq!(got["errors"], json!([]), "{file}");
        let drafts = got["drafts"].as_array().unwrap();
        let want = want.as_array().unwrap();
        assert_eq!(drafts.len(), want.len(), "{file}: {got}");
        for (d, w) in drafts.iter().zip(want) {
            assert_eq!(d["name"], w["name"], "{file}");
            assert_eq!(d["layer"], w["layer"], "{file}");
            assert_eq!(d["props"], w["props"], "{file}: {}", d["name"]);
            let rings = d["geometry"]["coordinates"].as_array().unwrap();
            assert_eq!(rings.len() as u64, w["rings"].as_u64().unwrap(), "{file}: {} rings", d["name"]);
            let (area, want_area) = (d["area_ha"].as_f64().unwrap(), w["area_ha"].as_f64().unwrap());
            assert!((area - want_area).abs() <= want_area * 0.01, "{file}: {} area {area} ha, want {want_area} ha (± 1 %)", d["name"]);
            // Every corner lands where PROJ put it, not just the right size.
            let outer: Vec<[f64; 2]> = rings[0].as_array().unwrap().iter().map(pt).collect();
            for w_pt in w["outer"].as_array().unwrap().iter().map(pt) {
                let nearest = outer.iter().map(|p| metres(*p, w_pt)).fold(f64::MAX, f64::min);
                assert!(nearest < 0.05, "{file}: {} corner {w_pt:?} is {nearest:.3} m from the nearest imported corner", d["name"]);
            }
        }
    }
}

#[tokio::test]
async fn a_nested_john_deere_zip_gives_drafts_from_every_polygon_layer() {
    let app = App::new().await;
    let (s, got) = app.preview_paddocks("jd_export.zip").await;
    assert_eq!(s, StatusCode::OK, "{got}");
    let mut drafts: Vec<(String, String, f64)> = got["drafts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| (d["name"].as_str().unwrap().to_owned(), d["layer"].as_str().unwrap().to_owned(), d["area_ha"].as_f64().unwrap()))
        .collect();
    drafts.sort_by(|a, b| a.0.cmp(&b.0));
    let want = expected()["paddocks"]["jd_export.zip"].clone();
    assert_eq!(drafts.len(), 3, "points (Flags) and lines (Guidance) are not paddocks, Mac shadows are skipped: {got}");
    for (d, w) in drafts.iter().zip(want.as_array().unwrap()) {
        assert_eq!(d.0, w["name"].as_str().unwrap());
        assert_eq!(d.1, w["layer"].as_str().unwrap());
        let wa = w["area_ha"].as_f64().unwrap();
        assert!((d.2 - wa).abs() <= wa * 0.01, "{} {} vs {wa}", d.0, d.2);
    }
}

#[tokio::test]
async fn commit_saves_kept_drafts_as_paddocks_with_fsa_props() {
    let app = App::new().await;
    app.farm().await;
    let (_, prev) = app.preview_paddocks("iowa_north_ftus_shp.zip").await;
    let id = prev["import_id"].as_str().unwrap();
    // North 40 under its own name, Hilltop 2 renamed; the rest dropped.
    let (s, got) = app.call("POST", "/api/import/paddocks/commit", Some(json!({"import_id": id, "keep": [0, 3], "names": [null, "Hill"]}))).await;
    assert_eq!(s, StatusCode::CREATED, "{got}");
    let made = got["paddocks"].as_array().unwrap();
    assert_eq!(made.iter().map(|p| p["name"].as_str().unwrap()).collect::<Vec<_>>(), vec!["North 40", "Hill"]);
    let (_, all) = app.call("GET", "/api/paddocks", None).await;
    let north = all.as_array().unwrap().iter().find(|p| p["name"] == "North 40").unwrap();
    assert_eq!(north["props"], json!({"fsa_farm": "1234", "fsa_tract": "5678", "fsa_field": "3"}));
    assert_eq!(north["area_ha"], prev["drafts"][0]["area_ha"]);
    assert_eq!(north["geometry"]["coordinates"].as_array().unwrap().len(), 2, "the pond stays a hole");
    assert_eq!(north["status"], "resting");
    // The preview is used up.
    let (s, _) = app.call("POST", "/api/import/paddocks/commit", Some(json!({"import_id": id, "keep": [1]}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn commit_refuses_bad_selections() {
    let app = App::new().await;
    let (_, prev) = app.preview_paddocks("fields.geojson").await;
    let id = prev["import_id"].as_str().unwrap();
    let (s, e) = app.call("POST", "/api/import/paddocks/commit", Some(json!({"import_id": id, "keep": [0]}))).await;
    assert_eq!((s, e["error"].as_str().unwrap()), (StatusCode::BAD_REQUEST, "Create the farm first."));
    app.farm().await;
    for (body, msg) in [
        (json!({"import_id": id, "keep": []}), "Keep at least one paddock."),
        (json!({"import_id": id, "keep": [9]}), "There is no draft 9."),
        (json!({"import_id": id, "keep": [1, 1]}), "Draft 1 is listed twice."),
        (json!({"import_id": id, "keep": [0, 1], "names": ["x"]}), "Give one name per kept paddock."),
        (json!({"import_id": "imp_nope", "keep": [0]}), "That preview has expired. Import the file again."),
    ] {
        let (s, e) = app.call("POST", "/api/import/paddocks/commit", Some(body)).await;
        assert!(s.is_client_error(), "{e}");
        assert_eq!(e["error"].as_str().unwrap(), msg);
    }
    // Nothing was saved by the refused commits.
    let (_, all) = app.call("GET", "/api/paddocks", None).await;
    assert_eq!(all.as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn bad_files_and_unknown_projections_give_readable_errors() {
    let app = App::new().await;
    let mut junk_zip = Vec::new();
    {
        use std::io::Write;
        let mut z = zip::ZipWriter::new(std::io::Cursor::new(&mut junk_zip));
        z.start_file("readme.txt", zip::write::SimpleFileOptions::default()).unwrap();
        z.write_all(b"hello").unwrap();
        z.finish().unwrap();
    }
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        ("albers_shp.zip", fixture("albers_shp.zip"), "uses the Albers projection, which openpasture can't read"),
        ("nad27_utm_shp.zip", fixture("nad27_utm_shp.zip"), "uses the D_North_American_1927 datum"),
        ("no_prj_utm_shp.zip", fixture("no_prj_utm_shp.zip"), "has no .prj file"),
        ("fields.shp", vec![0, 0, 0x27, 0x0a, 0, 0, 0, 0], "Zip the .shp with its .dbf, .shx and .prj"),
        ("notes.txt", b"just some words".to_vec(), "This isn't a file openpasture can read."),
        ("broken.zip", b"PK\x03\x04garbage that is not a zip".to_vec(), "The zip can't be opened"),
        ("readme.zip", junk_zip, "The zip holds no shapefile, KML or GeoJSON."),
        ("empty.geojson", br#"{"type":"FeatureCollection","features":[]}"#.to_vec(), "The file has no polygons."),
        ("gates.geojson", br#"{"type":"Feature","properties":{},"geometry":{"type":"Point","coordinates":[-93.62,42.03]}}"#.to_vec(), "The file has no polygons."),
        ("bad.geojson", b"{\"type\": \"FeatureCollection\", ".to_vec(), "The GeoJSON can't be read"),
        (
            "utm.geojson",
            br#"{"type":"Feature","properties":{},"geometry":{"type":"Polygon","coordinates":[[[448677,4653293],[448777,4653293],[448777,4653393],[448677,4653293]]]}}"#.to_vec(),
            "aren't longitude and latitude",
        ),
        (
            "albers.geojson",
            br#"{"type":"FeatureCollection","crs":{"type":"name","properties":{"name":"urn:ogc:def:crs:EPSG::5070"}},"features":[]}"#.to_vec(),
            "EPSG:5070, which openpasture can't read",
        ),
        ("x.kml", b"<svg></svg>".to_vec(), "This XML isn't KML."),
    ];
    for (name, bytes, msg) in cases {
        let (s, e) = app.upload("/api/import/paddocks/preview", name, &bytes, &[]).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{name}: {e}");
        let text = e["error"].as_str().unwrap_or_default();
        assert!(text.contains(msg), "{name}: {text:?} should say {msg:?}");
    }
    // A self-crossing polygon next to a good one: the good one is a draft, the other an error.
    let two = br#"{"type":"FeatureCollection","features":[
        {"type":"Feature","properties":{"NAME":"Good"},"geometry":{"type":"Polygon","coordinates":[[[-93.625,42.03],[-93.62,42.03],[-93.62,42.0336],[-93.625,42.03]]]}},
        {"type":"Feature","properties":{"NAME":"Bow"},"geometry":{"type":"Polygon","coordinates":[[[-93.62,42.03],[-93.61,42.04],[-93.61,42.03],[-93.62,42.04],[-93.62,42.03]]]}}]}"#;
    let (s, got) = app.upload("/api/import/paddocks/preview", "two.geojson", two, &[]).await;
    assert_eq!(s, StatusCode::OK);
    assert_eq!(got["drafts"].as_array().unwrap().len(), 1);
    assert_eq!(got["errors"], json!(["Bow: The shape crosses itself."]));
    // Over 20 MB: refused before it is read, as JSON.
    let big = vec![b' '; 21 * 1024 * 1024];
    let (s, e) = app.upload("/api/import/paddocks/preview", "big.geojson", &big, &[]).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE, "{e}");
    assert!(e["error"].as_str().unwrap_or_default().contains("larger than"), "{e}");
}

// ---------------------------------------------------------------- positions

async fn days(app: &App, import_id: &str) -> Vec<(String, String, String, i64, f64)> {
    let rows = sqlx::query_as::<_, (String, String, String, i64, f64)>(
        "SELECT date, collar_id, paddock_id, fixes, dwell_s FROM imported_paddock_days WHERE import_id = ? ORDER BY date, collar_id, paddock_id",
    )
    .bind(import_id)
    .fetch_all(app.ctx.db())
    .await
    .unwrap();
    rows
}

#[tokio::test]
async fn positions_csv_with_offsets_imports_and_writes_paddock_days() {
    let app = App::new().await;
    let farm = app.farm().await;
    let exp = expected()["positions"]["positions_offset.csv"].clone();
    let (s, prev) = app.upload("/api/import/positions/preview", "positions_offset.csv", &fixture("positions_offset.csv"), &[]).await;
    assert_eq!(s, StatusCode::OK, "{prev}");
    assert_eq!(prev["source"], "csv");
    assert_eq!(prev["mapping"], json!({"tag": "tag", "time": "timestamp", "lat": "lat", "lon": "lon", "accuracy": "accuracy"}));
    assert_eq!(prev["total"], exp["rows"]);
    assert_eq!(prev["needs_zone"], false, "every timestamp has an offset");
    assert_eq!(prev["rows"].as_array().unwrap().len(), 20);
    let labels = prev["labels"].as_array().unwrap();
    for (tag, w) in exp["labels"].as_object().unwrap() {
        let l = labels.iter().find(|l| l["label"] == *tag).unwrap_or_else(|| panic!("label {tag}"));
        assert_eq!(l["points"], w[0]);
        assert_eq!(l["from"].as_str().unwrap().replace(".000", ""), w[1].as_str().unwrap());
        assert_eq!(l["to"].as_str().unwrap().replace(".000", ""), w[2].as_str().unwrap());
        assert_eq!(l["animal_id"].as_str(), farm.animals.get(tag).map(String::as_str), "{tag}");
    }
    assert!(prev["tracks"].as_array().unwrap().iter().all(|t| t["points"].as_array().unwrap().len() <= 200));

    let mut events = app.ctx.subscribe();
    let id = prev["import_id"].as_str().unwrap().to_owned();
    let (s, done) = app.call("POST", &format!("/api/import/positions/{id}/commit"), Some(json!({}))).await;
    assert_eq!(s, StatusCode::CREATED, "{done}");
    assert_eq!(done["import"]["fixes"], 288);
    assert_eq!(done["import"]["animals"], 2);
    assert_eq!(done["skipped"], json!(["999"]));
    assert_eq!(done["duplicates"], 0);
    assert_eq!(done["import"]["created_by"], json!({"via": "local"}));
    assert_eq!(done["import"]["from"], "2025-06-14T00:00:00Z");
    assert_eq!(done["import"]["to"], "2025-06-15T23:45:00Z");
    match events.try_recv() {
        Ok(Event::AnimalsChanged { herd_id }) => assert_eq!(herd_id.as_deref(), Some(farm.herd.as_str())),
        other => panic!("expected animals_changed, got {other:?}"),
    }

    // Dwell by the rollup's rule: 15-minute fixes, the last of a day runs to midnight.
    let (a214, a031) = (farm.animals["214"].clone(), farm.animals["031"].clone());
    let mut want = vec![
        ("2025-06-14".to_owned(), a214.clone(), farm.p1.clone(), 48, 43_200.0),
        ("2025-06-14".to_owned(), a214.clone(), farm.p2.clone(), 48, 43_200.0),
        ("2025-06-15".to_owned(), a214.clone(), farm.p2.clone(), 96, 86_400.0),
        ("2025-06-14".to_owned(), a031.clone(), String::new(), 4, 3_600.0),
        ("2025-06-14".to_owned(), a031.clone(), farm.p1.clone(), 52, 46_800.0),
        ("2025-06-14".to_owned(), a031.clone(), farm.p2.clone(), 40, 36_000.0),
    ];
    want.sort_by(|a, b| (&a.0, &a.1, &a.2).cmp(&(&b.0, &b.1, &b.2)));
    assert_eq!(days(&app, &id).await, want);
    let herds: Vec<String> = sqlx::query_scalar("SELECT DISTINCT herd_id FROM imported_paddock_days").fetch_all(app.ctx.db()).await.unwrap();
    assert_eq!(herds, vec![farm.herd.clone()]);

    // Listed, and tracks served per animal and per import, thinned on request.
    let (_, list) = app.call("GET", "/api/import/positions", None).await;
    assert_eq!(list[0]["id"], id);
    assert_eq!(list[0]["file_name"], "positions_offset.csv");
    let (_, t) = app.call("GET", &format!("/api/import/positions/tracks?animal_id={a214}"), None).await;
    assert_eq!(t.as_array().unwrap().len(), 1);
    assert_eq!(t[0]["points"].as_array().unwrap().len(), 192);
    assert_eq!(t[0]["points"][0][2], 1_749_859_200.0);
    let (_, t) = app.call("GET", &format!("/api/import/positions/tracks?import_id={id}&max_points=10"), None).await;
    assert_eq!(t.as_array().unwrap().len(), 2);
    assert!(t.as_array().unwrap().iter().all(|x| x["points"].as_array().unwrap().len() <= 11), "{t}");
    let (_, t) = app.call("GET", &format!("/api/import/positions/tracks?animal_id={a214}&from=2025-06-15&to=2025-06-15T06:00:00Z"), None).await;
    assert_eq!(t[0]["points"].as_array().unwrap().len(), 24);
    let (s, _) = app.call("GET", "/api/import/positions/tracks?from=yesterday", None).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // The same file again stores nothing twice.
    let (_, prev2) = app.upload("/api/import/positions/preview", "positions_offset.csv", &fixture("positions_offset.csv"), &[]).await;
    let id2 = prev2["import_id"].as_str().unwrap();
    let (s, again) = app.call("POST", &format!("/api/import/positions/{id2}/commit"), Some(json!({}))).await;
    assert_eq!((s, again["error"].as_str().unwrap()), (StatusCode::CONFLICT, "Every point in this file is already imported."));
    assert!(days(&app, id2).await.is_empty());
    let (_, list) = app.call("GET", "/api/import/positions", None).await;
    assert_eq!(list.as_array().unwrap().len(), 1, "no empty import listed");
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM imported_fixes").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 288);
    // A GPX that repeats 214's first day and adds 118: only 118's points are new.
    let (_, gpx) = app.upload("/api/import/positions/preview", "tracks.gpx", &fixture("tracks.gpx"), &[]).await;
    let gid = gpx["import_id"].as_str().unwrap().to_owned();
    let (s, g) = app.call("POST", &format!("/api/import/positions/{gid}/commit"), Some(json!({}))).await;
    assert_eq!(s, StatusCode::CREATED, "{g}");
    assert_eq!((g["import"]["fixes"].as_i64(), g["duplicates"].as_i64(), g["import"]["animals"].as_i64()), (Some(16), Some(96), Some(1)));
    let (s, _) = app.call("DELETE", &format!("/api/import/positions/{gid}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);

    // Undo: fixes and days of that import go.
    let (s, _) = app.call("DELETE", &format!("/api/import/positions/{id}"), None).await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    assert!(days(&app, &id).await.is_empty());
    let (_, t) = app.call("GET", &format!("/api/import/positions/tracks?animal_id={a214}"), None).await;
    assert_eq!(t, json!([]));
    let (s, _) = app.call("DELETE", &format!("/api/import/positions/{id}"), None).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn positions_without_offsets_read_in_the_farm_zone_or_the_one_chosen() {
    let app = App::new().await;
    let farm = app.farm().await;
    let (s, prev) = app.upload("/api/import/positions/preview", "positions_local.csv", &fixture("positions_local.csv"), &[]).await;
    assert_eq!(s, StatusCode::OK, "{prev}");
    assert_eq!(prev["mapping"], json!({"tag": "Animal ID", "time": "Date/Time", "lat": "Latitude", "lon": "Longitude"}));
    assert_eq!(prev["needs_zone"], true);
    assert_eq!(prev["zone"], "America/Chicago");
    assert_eq!(prev["labels"][0]["from"], "2025-06-14T00:00:00Z", "19:00 CDT on the 13th");
    assert_eq!(prev["labels"][0]["animal_id"].as_str(), Some(farm.animals["031"].as_str()));
    let id = prev["import_id"].as_str().unwrap().to_owned();

    // The device logged UTC after all.
    let (s, utc) = app.call("POST", &format!("/api/import/positions/{id}/preview"), Some(json!({"zone": "UTC"}))).await;
    assert_eq!(s, StatusCode::OK, "{utc}");
    assert_eq!(utc["labels"][0]["from"], "2025-06-13T19:00:00Z");
    assert_eq!(utc["zone"], "UTC");
    let (s, e) = app.call("POST", &format!("/api/import/positions/{id}/preview"), Some(json!({"zone": "Mars/Olympus"}))).await;
    assert_eq!((s, e["error"].as_str().unwrap()), (StatusCode::BAD_REQUEST, "\"Mars/Olympus\" isn't a time zone."));

    // A mapping without the time column says which column to choose.
    let (_, m) = app
        .call("POST", &format!("/api/import/positions/{id}/preview"), Some(json!({"mapping": {"tag": "Animal ID", "lat": "Latitude", "lon": "Longitude"}})))
        .await;
    assert_eq!(m["points"], 0);
    assert_eq!(m["errors"][0], "Choose the time column.");

    let (s, done) = app.call("POST", &format!("/api/import/positions/{id}/commit"), Some(json!({"zone": "America/Chicago"}))).await;
    assert_eq!(s, StatusCode::CREATED, "{done}");
    assert_eq!(done["import"]["zone"], "America/Chicago");
    assert_eq!(done["import"]["from"], "2025-06-14T00:00:00Z");
    assert_eq!(done["import"]["fixes"], 96);
}

#[tokio::test]
async fn a_day_first_export_is_read_day_first_on_every_row() {
    // A tracker that writes 01/06/2026 for June 1st, on the Iowa farm.
    let app = App::new().await;
    app.farm().await;
    let mut csv = String::from("Animal ID,Date/Time,Latitude,Longitude\n");
    for d in 1..=30 {
        csv += &format!("031,{d:02}/06/2026 10:00,42.0318,-93.6225\n");
    }
    let (s, prev) = app.upload("/api/import/positions/preview", "tracker.csv", csv.as_bytes(), &[]).await;
    assert_eq!(s, StatusCode::OK, "{prev}");
    assert_eq!(prev["errors"], json!([]));
    // 10:00 CDT is 15:00 UTC: June 1 to June 30, not January 6 to December 6.
    assert_eq!((prev["labels"][0]["from"].as_str(), prev["labels"][0]["to"].as_str()), (Some("2026-06-01T15:00:00Z"), Some("2026-06-30T15:00:00Z")));
}

#[tokio::test]
async fn gpx_tracks_are_named_by_tag() {
    let app = App::new().await;
    let farm = app.farm().await;
    let (s, prev) = app.upload("/api/import/positions/preview", "tracks.gpx", &fixture("tracks.gpx"), &[]).await;
    assert_eq!(s, StatusCode::OK, "{prev}");
    assert_eq!(prev["source"], "gpx");
    assert_eq!(prev["needs_zone"], false);
    let exp = expected()["positions"]["tracks.gpx"].clone();
    assert_eq!(prev["total"], exp["rows"]);
    for (tag, w) in exp["labels"].as_object().unwrap() {
        let l = prev["labels"].as_array().unwrap().iter().find(|l| l["label"] == *tag).unwrap();
        assert_eq!((l["points"].clone(), l["animal_id"].as_str()), (w[0].clone(), Some(farm.animals[tag].as_str())));
        assert_eq!(l["from"], w[1]);
    }
    let id = prev["import_id"].as_str().unwrap();
    let (s, done) = app.call("POST", &format!("/api/import/positions/{id}/commit"), Some(json!({}))).await;
    assert_eq!(s, StatusCode::CREATED, "{done}");
    assert_eq!(done["import"]["fixes"], 112);
    assert_eq!(done["import"]["source"], "gpx");
    let src: Vec<String> = sqlx::query_scalar("SELECT DISTINCT source FROM imported_fixes").fetch_all(app.ctx.db()).await.unwrap();
    assert_eq!(src, vec!["gpx"]);
}

#[tokio::test]
async fn geojson_points_with_time_and_tag_import() {
    let app = App::new().await;
    let farm = app.farm().await;
    let (s, prev) = app.upload("/api/import/positions/preview", "points.geojson", &fixture("points.geojson"), &[]).await;
    assert_eq!(s, StatusCode::OK, "{prev}");
    assert_eq!(prev["source"], "geojson");
    assert_eq!(prev["mapping"], json!({"tag": "animal", "time": "time", "accuracy": "accuracy"}));
    assert_eq!(prev["labels"][0]["label"], "118");
    assert_eq!(prev["labels"][0]["points"], 16);
    let id = prev["import_id"].as_str().unwrap();
    let (s, done) = app.call("POST", &format!("/api/import/positions/{id}/commit"), Some(json!({}))).await;
    assert_eq!(s, StatusCode::CREATED, "{done}");
    let acc: Option<f64> = sqlx::query_scalar("SELECT accuracy_m FROM imported_fixes WHERE animal_id = ? ORDER BY t LIMIT 1")
        .bind(&farm.animals["118"])
        .fetch_one(app.ctx.db())
        .await
        .unwrap();
    assert!(acc.is_some_and(|a| (1.5..=4.5).contains(&a)), "{acc:?}");
}

#[tokio::test]
async fn labels_can_be_given_to_another_animal_or_left_out() {
    let app = App::new().await;
    let farm = app.farm().await;
    let (_, prev) = app.upload("/api/import/positions/preview", "positions_offset.csv", &fixture("positions_offset.csv"), &[]).await;
    let id = prev["import_id"].as_str().unwrap();
    let body = json!({"animals": {"999": farm.animals["118"], "031": null}});
    let (s, done) = app.call("POST", &format!("/api/import/positions/{id}/commit"), Some(body)).await;
    assert_eq!(s, StatusCode::CREATED, "{done}");
    assert_eq!(done["import"]["fixes"], 192 + 4);
    assert_eq!(done["skipped"], json!(["031"]));
    let n: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM imported_fixes WHERE animal_id = ?").bind(&farm.animals["118"]).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(n, 4);

    // Nothing matched at all: refused, nothing stored.
    let (_, prev) = app.upload("/api/import/positions/preview", "positions_offset.csv", &fixture("positions_offset.csv"), &[]).await;
    let id = prev["import_id"].as_str().unwrap();
    let (s, e) = app.call("POST", &format!("/api/import/positions/{id}/commit"), Some(json!({"animals": {"214": null, "031": null}}))).await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{e}");
    let (s, e) = app.call("POST", &format!("/api/import/positions/{id}/commit"), Some(json!({"animals": {"214": "ani_nope"}}))).await;
    assert_eq!((s, e["error"].as_str().unwrap()), (StatusCode::BAD_REQUEST, "No such animal for 214."));
}

#[tokio::test]
async fn position_files_that_cant_be_read_say_why() {
    let app = App::new().await;
    for (name, bytes, msg) in [
        ("x.bin", vec![0xff, 0xfe, 0x00, 0x01], "This isn't a text file."),
        ("x.xml", b"<kml></kml>".to_vec(), "This XML isn't GPX."),
        ("x.gpx", b"<gpx><trk><name>1</trk></gpx>".to_vec(), "The GPX can't be read"),
        ("x.geojson", br#"{"type":"FeatureCollection","features":[]}"#.to_vec(), "The GeoJSON has no points."),
        ("notes.md", b"hello world".to_vec(), "This isn't a file openpasture can read."),
    ] {
        let (s, e) = app.upload("/api/import/positions/preview", name, &bytes, &[]).await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{name}: {e}");
        assert!(e["error"].as_str().unwrap_or_default().contains(msg), "{name}: {e}");
    }
    let (s, _) = app.call("POST", "/api/import/positions/imp_gone/commit", Some(json!({}))).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}
