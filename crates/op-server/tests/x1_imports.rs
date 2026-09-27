//! Imported position history never counts an animal-hour its collar already
//! recorded: points inside collar data (hot fixes, and days already rolled to
//! Parquet) are left out at commit, so pasture dwell isn't doubled, and the
//! rest of the file (other times, animals without collars) is imported.

use op_server::{ServeOptions, serve};
use serde_json::{Value, json};

const BOUNDARY: &str = "----x1-imports-test";
const MIN: i64 = 60 * 1000;
const HOUR: i64 = 60 * MIN;
const HALF_HOUR: i64 = 30 * MIN;
const IN_P1: [f64; 2] = [-93.6222, 42.0318];
const IN_P2: [f64; 2] = [-93.6172, 42.0318];

fn multipart(file_name: &str, bytes: &[u8]) -> Vec<u8> {
    let mut body =
        format!("--{BOUNDARY}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{file_name}\"\r\nContent-Type: text/csv\r\n\r\n").into_bytes();
    body.extend_from_slice(bytes);
    body.extend_from_slice(format!("\r\n--{BOUNDARY}--\r\n").as_bytes());
    body
}

/// Unix ms of an RFC 3339 time.
fn at(s: &str) -> i64 {
    op_core::time::from_db(s).unwrap().timestamp_millis()
}

fn z(t: i64) -> String {
    op_core::time::from_unix_ms(t).format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// `n` times (unix ms) from `from`, `step` ms apart.
fn every(from: i64, step: i64, n: i64) -> Vec<i64> {
    (0..n).map(|i| from + step * i).collect()
}

#[tokio::test]
async fn points_the_collar_already_recorded_are_left_out_of_an_import() {
    let dir = tempfile::tempdir().unwrap();
    let handle = serve(ServeOptions { data_dir: Some(dir.path().into()), free_port: true, ..Default::default() }).await.unwrap();
    let base = handle.url().to_owned();
    let http = reqwest::Client::new();
    let post = |path: &str, body: Value| http.post(format!("{base}{path}")).json(&body).send();
    let ok = |r: reqwest::Response| async move {
        let s = r.status();
        let v: Value = r.json().await.unwrap_or(Value::Null);
        assert!(s.is_success(), "{s} {v}");
        v
    };

    ok(post("/api/farm", json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]})).await.unwrap()).await;
    let pad = |name: &str, w: f64, e: f64| json!({"name": name, "geometry": {"type": "Polygon", "coordinates": [[[w, 42.03], [e, 42.03], [e, 42.0336], [w, 42.0336], [w, 42.03]]]}});
    let p1 = ok(post("/api/paddocks", pad("P1", -93.625, -93.62)).await.unwrap()).await;
    let p2 = ok(post("/api/paddocks", pad("P2", -93.62, -93.615)).await.unwrap()).await;
    let herd = ok(post("/api/herds", json!({"name": "Cows", "species": "cattle", "count": 2, "paddock_id": p1["id"]})).await.unwrap()).await;
    let collar = ok(post("/api/collars", json!({"herd_id": herd["id"], "name": "Bench"})).await.unwrap()).await;
    let key = collar["key"].as_str().unwrap().to_owned();
    ok(post("/api/animals", json!({"tag": "214", "herd_id": herd["id"], "collar_id": collar["collar"]["id"]})).await.unwrap()).await;
    ok(post("/api/animals", json!({"tag": "031", "herd_id": herd["id"]})).await.unwrap()).await;
    let report = |times: Vec<i64>| {
        let fixes: Vec<Value> = times.iter().map(|t| json!({"at": z(*t), "point": IN_P1, "accuracy_m": 2.0, "sats": 9})).collect();
        http.post(format!("{base}/collar/v1/report")).bearer_auth(&key).json(&json!({"fixes": fixes, "battery": 0.8})).send()
    };

    // An old day of collar data, 10:00 to 14:00, rolled to Parquet since.
    let old_day = at("2025-06-20T00:00:00Z");
    ok(report(every(old_day + 10 * HOUR, 5 * MIN, 49)).await.unwrap()).await;
    let rolled = op_analytics::rollup::rollup(handle.ctx(), 3, op_core::time::now()).await.unwrap();
    assert!(rolled.iter().any(|d| d.table == "fixes" && d.date == "2025-06-20" && d.rows == 49), "{rolled:?}");
    // And today, still hot: an hour of fixes, on half-hour boundaries so the buckets are plain.
    let now_ms = op_core::time::now().timestamp_millis();
    let hot = now_ms.div_euclid(HALF_HOUR) * HALF_HOUR - 3 * HOUR;
    ok(report(every(hot + 60 * MIN, 5 * MIN, 13)).await.unwrap()).await;

    // The file: 214 on the old day 08:00-16:00 and today around the hot hour, 031 all of it.
    let mut csv = String::from("tag,timestamp,lat,lon\n");
    let old = every(old_day + 8 * HOUR, 10 * MIN, 49);
    let today = every(hot, 10 * MIN, 18);
    for tag in ["214", "031"] {
        for t in old.iter().chain(today.iter()) {
            csv.push_str(&format!("{tag},{},{},{}\n", z(*t), IN_P2[1], IN_P2[0]));
        }
    }
    let prev = ok(http
        .post(format!("{base}/api/import/positions/preview"))
        .header("content-type", format!("multipart/form-data; boundary={BOUNDARY}"))
        .body(multipart("walk.csv", csv.as_bytes()))
        .send()
        .await
        .unwrap())
    .await;
    let id = prev["import_id"].as_str().unwrap().to_owned();
    let done = ok(post(&format!("/api/import/positions/{id}/commit"), json!({})).await.unwrap()).await;

    // 214's collar covered the old day 10:00-14:30 (its 4.5 h of dwell, ending half an hour
    // after its last fix): 27 points. Today it covered the half-hours holding its fixes,
    // hot+60 to hot+150 min: 9 points. 031 wears no collar: all 67 of its points.
    assert_eq!(done["collar_covered"], 27 + 9, "{done}");
    assert_eq!(done["duplicates"], 0);
    assert_eq!(done["import"]["fixes"], 2 * 67 - 36, "{done}");

    let detail = ok(http.get(format!("{base}/api/import/positions/{id}")).send().await.unwrap()).await;
    let animals: Value = ok(http.get(format!("{base}/api/animals")).send().await.unwrap()).await;
    let animal = |tag: &str| animals.as_array().unwrap().iter().find(|a| a["tag"] == tag).unwrap()["id"].as_str().unwrap().to_owned();
    let dwell = |animal: &str, date: &str| -> f64 {
        detail["days"].as_array().unwrap().iter().filter(|d| d["animal_id"] == animal && d["date"] == date).map(|d| d["dwell_s"].as_f64().unwrap()).sum()
    };
    // Old day: 08:00-09:50 (11 gaps of 10 min, then 30 min to the next point), 14:30-16:00 (9 gaps, 30 min after).
    assert_eq!(dwell(&animal("214"), "2025-06-20"), ((11 * 10 + 30 + 9 * 10 + 30) * 60) as f64);
    // Without a collar: the whole 08:00-16:00 walk.
    assert_eq!(dwell(&animal("031"), "2025-06-20"), ((48 * 10 + 30) * 60) as f64);
    // All the imported dwell is in P2; the collar's is in P1.
    assert!(detail["days"].as_array().unwrap().iter().all(|d| d["paddock_id"] == p2["id"]), "{detail}");

    // The file again: nothing new at all.
    let again = ok(http
        .post(format!("{base}/api/import/positions/preview"))
        .header("content-type", format!("multipart/form-data; boundary={BOUNDARY}"))
        .body(multipart("walk.csv", csv.as_bytes()))
        .send()
        .await
        .unwrap())
    .await;
    let res = post(&format!("/api/import/positions/{}/commit", again["import_id"].as_str().unwrap()), json!({})).await.unwrap();
    assert_eq!(res.status(), 409);
    let msg: Value = res.json().await.unwrap();
    assert!(msg["error"].as_str().unwrap().contains("collars"), "{msg}");
    handle.shutdown().await.unwrap();
}
