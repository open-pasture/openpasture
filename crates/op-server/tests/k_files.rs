//! Imported position history (K-files) through the whole server: the import
//! feeds pasture history (last grazed, rest), and the telemetry rollup of the
//! same day leaves the imported days alone.

use op_server::{ServeOptions, serve};
use serde_json::{Value, json};

const BOUNDARY: &str = "----k-files-server-test";

fn fixture(name: &str) -> Vec<u8> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../op-import/tests/fixtures/files").join(name);
    std::fs::read(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

fn multipart(file_name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body =
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: text/csv\r\n\r\n").into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

#[tokio::test]
async fn imported_history_shows_in_pasture_and_survives_the_rollup() {
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(ServeOptions { data_dir: Some(dir.path().into()), free_port: true, ..Default::default() }).await.unwrap();
    let base = handle.url().to_owned();
    let http = reqwest::Client::new();
    let post = |path: &str, body: Value| http.post(format!("{base}{path}")).json(&body).send();

    post("/api/farm", json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]})).await.unwrap().error_for_status().unwrap();
    let pad = |name: &str, w: f64, e: f64| json!({"name": name, "geometry": {"type": "Polygon", "coordinates": [[[w, 42.03], [e, 42.03], [e, 42.0336], [w, 42.0336], [w, 42.03]]]}});
    let p1: Value = post("/api/paddocks", pad("P1", -93.625, -93.62)).await.unwrap().json().await.unwrap();
    let p2: Value = post("/api/paddocks", pad("P2", -93.62, -93.615)).await.unwrap().json().await.unwrap();
    let herd: Value = post("/api/herds", json!({"name": "Cows", "species": "cattle", "count": 2, "paddock_id": p1["id"]})).await.unwrap().json().await.unwrap();
    for tag in ["214", "031"] {
        post("/api/animals", json!({"tag": tag, "herd_id": herd["id"]})).await.unwrap().error_for_status().unwrap();
    }

    // Import last season's CSV.
    let prev: Value = http
        .post(format!("{base}/api/import/positions/preview"))
        .header("content-type", format!("multipart/form-data; boundary={BOUNDARY}"))
        .body(multipart("positions_offset.csv", &fixture("positions_offset.csv")))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = prev["import_id"].as_str().unwrap();
    let done: Value = post(&format!("/api/import/positions/{id}/commit"), json!({})).await.unwrap().json().await.unwrap();
    assert_eq!(done["import"]["fixes"], 288, "{done}");

    // A collar fix on the same day, so the rollup has that day to roll.
    let collar: Value = post("/api/collars", json!({"herd_id": herd["id"], "name": "Bench"})).await.unwrap().json().await.unwrap();
    let key = collar["key"].as_str().unwrap();
    let report = json!({"fixes": [{"at": "2025-06-14T12:00:00Z", "point": [-93.6222, 42.0318], "accuracy_m": 2.0, "sats": 9}], "battery": 0.8});
    let res = http.post(format!("{base}/collar/v1/report")).bearer_auth(key).json(&report).send().await.unwrap();
    assert_eq!(res.status(), 200, "{}", res.text().await.unwrap());

    let pasture = || async {
        let rows: Value =
            http.get(format!("{base}/api/analytics/pasture?herd_id={}", herd["id"].as_str().unwrap())).send().await.unwrap().json().await.unwrap();
        let get = |pid: &Value| rows.as_array().unwrap().iter().find(|r| r["paddock_id"] == *pid).cloned().unwrap();
        (get(&p1["id"]), get(&p2["id"]))
    };
    let (a1, a2) = pasture().await;
    assert_eq!(a1["last_grazed"], "2025-06-14T23:45:00.000Z", "031 in P1 until the end of the 14th: {a1}");
    assert_eq!(a2["last_grazed"], "2025-06-15T23:45:00.000Z", "214 in P2 through the 15th: {a2}");
    assert!(a2["rest_days"].as_f64().unwrap() > 400.0);

    let days = || async { http.get(format!("{base}/api/import/positions/{id}")).send().await.unwrap().json::<Value>().await.unwrap()["days"].clone() };
    let before = days().await;
    assert_eq!(before.as_array().unwrap().len(), 6, "{before}");
    let rolled = op_analytics::rollup::rollup(handle.ctx(), 3, op_core::time::now()).await.unwrap();
    assert!(rolled.iter().any(|d| d.table == "fixes" && d.date == "2025-06-14" && d.rows == 1), "{rolled:?}");
    assert_eq!(days().await, before, "imported days untouched");
    let (b1, b2) = pasture().await;
    assert_eq!((b1["last_grazed"].clone(), b2["last_grazed"].clone()), (a1["last_grazed"].clone(), a2["last_grazed"].clone()));

    handle.shutdown().await.unwrap();
}
