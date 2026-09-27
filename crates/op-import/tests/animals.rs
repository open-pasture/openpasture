//! K-animals: CSV import (header guessing, preview, idempotent commit, 250
//! rows), the head count, remove and swap, park and unpark, bulk collar
//! linking whose keys work on the collar endpoint, rekeying, and cards whose
//! QR codes decode to the provisioning payload.

use std::path::Path;
use std::time::Instant;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Ctx, Event, Identity, Role, Via};
use serde_json::{Value, json};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
    herd: String,
}

impl App {
    /// A farm in Iowa (month-first dates), paddock P1, herd Cows (count 250).
    async fn new() -> Self {
        Self::as_role(Role::Owner).await
    }

    async fn as_role(role: Role) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let routes = op_core::router().merge(op_ingest::router()).merge(op_import::router());
        let id = Identity { role, user_id: None, name: Some("Cody".into()), via: Via::Local };
        let router = op_core::with_identity(routes, id).with_state(ctx.clone());
        let mut app = Self { _dir: dir, ctx, router, herd: String::new() };
        let (s, _) = app.json("POST", "/api/farm", json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]})).await;
        assert_eq!(s, StatusCode::CREATED);
        let ring = json!([[[-93.625, 42.03], [-93.62, 42.03], [-93.62, 42.0336], [-93.625, 42.0336], [-93.625, 42.03]]]);
        let (_, p) = app.json("POST", "/api/paddocks", json!({"name": "P1", "geometry": {"type": "Polygon", "coordinates": ring}})).await;
        let (_, h) = app.json("POST", "/api/herds", json!({"name": "Cows", "species": "cattle", "count": 250, "paddock_id": p["id"]})).await;
        app.herd = h["id"].as_str().unwrap().to_owned();
        app
    }

    async fn send(&self, method: &str, path: &str, headers: &[(&str, &str)], body: Body) -> (StatusCode, Value) {
        let mut b = Request::builder().method(method).uri(path);
        for (k, v) in headers {
            b = b.header(*k, *v);
        }
        let res = self.router.clone().oneshot(b.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    async fn json(&self, method: &str, path: &str, body: Value) -> (StatusCode, Value) {
        self.send(method, path, &[("content-type", "application/json")], Body::from(body.to_string())).await
    }

    async fn get(&self, path: &str) -> (StatusCode, Value) {
        self.send("GET", path, &[], Body::empty()).await
    }

    async fn csv(&self, path: &str, text: impl Into<Body>) -> (StatusCode, Value) {
        self.send("POST", path, &[("content-type", "text/csv")], text.into()).await
    }

    async fn preview(&self, bytes: Vec<u8>) -> Value {
        let (s, v) = self.csv("/api/animals/import/preview", bytes).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }

    async fn commit(&self, preview: &Value) -> Value {
        let path = format!("/api/animals/import/{}/commit", preview["import_id"].as_str().unwrap());
        let (s, v) = self.json("POST", &path, json!({"mapping": preview["mapping"], "herd_id": self.herd})).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        v
    }

    async fn count(&self) -> u32 {
        self.ctx.store().get_herd(&self.herd).await.unwrap().unwrap().count
    }

    async fn animal(&self, tag: &str) -> op_core::Animal {
        self.ctx.store().list_animals(Some(&self.herd)).await.unwrap().into_iter().find(|a| a.tag == tag && a.removed_at.is_none()).unwrap()
    }

    async fn collar(&self, id: &str) -> op_core::Collar {
        let (s, v) = self.get(&format!("/api/collars/{id}")).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        serde_json::from_value(v).unwrap()
    }

    /// A report with one fix, as a collar would send it.
    async fn report(&self, key: &str, at: &str) -> StatusCode {
        let body = json!({"fixes": [{"at": at, "point": [-93.6225, 42.0318], "accuracy_m": 2.0, "sats": 9}], "battery": 0.9});
        self.send(
            "POST",
            "/collar/v1/report",
            &[("content-type", "application/json"), ("authorization", &format!("Bearer {key}"))],
            Body::from(body.to_string()),
        )
        .await
        .0
    }

    /// Animals 1..=n by CSV, then a collar each by bulk link. Returns (tag, collar id, key).
    async fn herd_of(&self, n: usize) -> Vec<(String, String, String)> {
        let mut csv = String::from("Tag,EID,Breed,Sex,DOB\n");
        for i in 1..=n {
            csv += &format!("{i},{},Angus,{},4/{}/2022\n", 982_000_100_000_000u64 + i as u64, if i % 2 == 0 { "F" } else { "Steer" }, 1 + i % 28);
        }
        let p = self.preview(csv.into_bytes()).await;
        assert_eq!(self.commit(&p).await["created"], n);
        let links: String = (1..=n).map(|i| format!("{i},C-{i:04}\n")).collect();
        let (s, v) = self.csv(&format!("/api/collars/bulk?herd_id={}", self.herd), links).await;
        assert_eq!(s, StatusCode::CREATED, "{v}");
        v["collars"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| (c["tag"].as_str().unwrap().to_owned(), c["collar"]["id"].as_str().unwrap().to_owned(), c["key"].as_str().unwrap().to_owned()))
            .collect()
    }
}

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/animals").join(name)).unwrap()
}

// Import

#[tokio::test]
async fn headers_are_guessed_the_way_herd_software_writes_them() {
    let app = App::new().await;
    let cases: [(&str, Value); 4] = [
        ("visual_id.csv", json!({"tag": "Visual ID", "eid": "EID", "name": "Name", "breed": "Breed", "sex": "Sex", "born": "DOB"})),
        ("ear_tag_semicolon.csv", json!({"tag": "Ear Tag", "eid": "RFID Tag", "sex": "Gender", "born": "Birth Date", "notes": "Comments"})),
        ("tag_iso_utf16.txt", json!({"tag": "Tag", "eid": "ISO", "born": "Born", "collar": "Collar"})),
        ("cow_tag_number.csv", json!({"tag": "Cow Tag Number", "born": "Date of Birth (mm/dd/yyyy)", "name": "Animal Name"})),
    ];
    for (file, want) in cases {
        let p = app.preview(fixture(file)).await;
        assert_eq!(p["mapping"], want, "{file}");
    }
}

#[tokio::test]
async fn the_preview_shows_the_first_rows_and_what_would_be_skipped() {
    let app = App::new().await;
    let p = app.preview(fixture("visual_id.csv")).await;
    assert_eq!(p["total"], 7);
    assert_eq!(p["columns"], json!(["Visual ID", "EID", "Name", "Breed", "Sex", "DOB"]));
    assert_eq!(p["rows"][0], json!(["214", "982000123456789", "Daisy", "Angus", "Cow", "4/1/2021"]));
    assert_eq!(
        p["errors"],
        json!([
            {"row": 6, "error": "EID 9.82E+14 isn't 15 digits."},
            {"row": 7, "error": "Sex unknown isn't female, male or castrated."},
            {"row": 8, "error": "Birth date 2021 isn't a date."},
        ])
    );

    // Only the first 20 rows come back.
    let big: String = std::iter::once("Tag\n".to_owned()).chain((1..=25).map(|i| format!("{i}\n"))).collect();
    let p = app.preview(big.into_bytes()).await;
    assert_eq!((p["total"].as_u64(), p["rows"].as_array().unwrap().len()), (Some(25), 20));

    let (s, e) = app.csv("/api/animals/import/preview", "").await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("The file is empty.")));
    let (s, _) = app.csv("/api/animals/import/nope/commit", "").await;
    assert_eq!(s, StatusCode::UNSUPPORTED_MEDIA_TYPE);
    let (s, e) = app.json("POST", "/api/animals/import/imp_nope/commit", json!({"mapping": {"tag": "Tag"}, "herd_id": app.herd})).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::NOT_FOUND, Some("This preview has expired. Choose the file again.")));
}

#[tokio::test]
async fn previews_take_files_up_to_five_megabytes() {
    let app = App::new().await;
    // Wider than the server's usual 2 MB body limit.
    let mut csv = String::from("Tag,Notes\n");
    let note = "x".repeat(300);
    for i in 0..10_000 {
        csv += &format!("{i},{note}\n");
    }
    assert!(csv.len() > 3_000_000);
    let p = app.preview(csv.into_bytes()).await;
    assert_eq!(p["total"], 10_000);
    let (s, _) = app.csv("/api/animals/import/preview", "Tag\n".to_owned() + &"1\n".repeat(3_000_000)).await;
    assert_eq!(s, StatusCode::PAYLOAD_TOO_LARGE);
}

#[tokio::test]
async fn one_wide_row_is_cheap_and_a_row_past_256_columns_is_refused() {
    let app = App::new().await;
    // A header, one row of 5,000 commas and 5,000 one-cell rows: every row
    // used to be padded to 5,001 cells (630 MB held for 30 minutes).
    let csv = format!("Tag\n214{}\n{}", ",".repeat(5000), "x\n".repeat(5000));
    let started = Instant::now();
    let p = app.preview(csv.into_bytes()).await;
    assert!(started.elapsed().as_secs_f64() < 2.0, "{:?}", started.elapsed());
    assert_eq!((p["total"].as_u64(), p["columns"].clone()), (Some(5001), json!(["Tag"])));
    assert_eq!(p["rows"][0], json!(["214"]));
    // The preview shows each row as wide as the header.
    let p = app.preview(b"Tag,Name,Breed\n214\n215,Bella\n".to_vec()).await;
    assert_eq!(p["rows"], json!([["214", "", ""], ["215", "Bella", ""]]));
    assert_eq!(app.commit(&p).await["created"], 2);

    let wide = format!("Tag\n214\n{}\n", vec!["x"; 257].join(","));
    let (s, e) = app.csv("/api/animals/import/preview", wide.clone()).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("Row 3 has more than 256 columns.")));
    let (s, e) = app.csv(&format!("/api/collars/bulk?herd_id={}", app.herd), wide).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("Row 3 has more than 256 columns.")));
}

#[tokio::test]
async fn a_preview_outlives_many_newer_ones() {
    // Previews are kept by bytes, not by count: sixteen newer ones (other
    // tests in this binary make as many in parallel) don't push one out.
    let app = App::new().await;
    let p = app.preview(b"Tag\n214\n".to_vec()).await;
    for i in 0..40 {
        app.preview(format!("Tag\n{}\n", 300 + i).into_bytes()).await;
    }
    assert_eq!(app.commit(&p).await["created"], 1);
    assert_eq!(app.commit(&p).await["unchanged"], 1);
}

#[tokio::test]
async fn a_commit_creates_or_updates_by_tag_and_twice_changes_nothing() {
    let app = App::new().await;
    let mut events = app.ctx.subscribe();
    let p = app.preview(fixture("visual_id.csv")).await;
    let c = app.commit(&p).await;
    assert_eq!((c["created"].as_u64(), c["updated"].as_u64(), c["unchanged"].as_u64(), c["total"].as_u64()), (Some(4), Some(0), Some(0), Some(7)));
    assert_eq!(c["errors"].as_array().unwrap().len(), 3);
    assert_eq!(events.recv().await.unwrap(), Event::AnimalsChanged { herd_id: Some(app.herd.clone()) });

    let daisy = app.animal("214").await;
    assert_eq!((daisy.name.as_deref(), daisy.eid.as_deref(), daisy.breed.as_deref()), (Some("Daisy"), Some("982000123456789"), Some("Angus")));
    assert_eq!((daisy.sex, daisy.born), (Some(op_core::Sex::Female), chrono::NaiveDate::from_ymd_opt(2021, 4, 1)));
    // US farm: 3/15/2023 is March 15; the EID lost its spaces.
    let a215 = app.animal("215").await;
    assert_eq!((a215.born, a215.eid.as_deref()), (chrono::NaiveDate::from_ymd_opt(2023, 3, 15), Some("982000123456790")));
    assert_eq!(app.animal("217").await.sex, Some(op_core::Sex::Male));
    assert_eq!(app.count().await, 4);

    // The same commit again: nothing changes.
    let again = app.commit(&p).await;
    assert_eq!((again["created"].as_u64(), again["updated"].as_u64(), again["unchanged"].as_u64()), (Some(0), Some(0), Some(4)));
    assert_eq!(app.ctx.store().list_animals(Some(&app.herd)).await.unwrap().len(), 4);

    // A new file: one breed changed, a blank name leaves Daisy's, one new animal.
    let p2 = app.preview(b"Tag,Name,Breed\n214,,Red Angus\n216,Bella,Hereford x Angus\n301,,Angus\n".to_vec()).await;
    let c = app.commit(&p2).await;
    assert_eq!((c["created"].as_u64(), c["updated"].as_u64(), c["unchanged"].as_u64()), (Some(1), Some(1), Some(1)));
    let daisy = app.animal("214").await;
    assert_eq!((daisy.name.as_deref(), daisy.breed.as_deref(), daisy.eid.as_deref()), (Some("Daisy"), Some("Red Angus"), Some("982000123456789")));
    assert_eq!(app.count().await, 5);

    // Rows that clash with each other or with the farm are skipped and said why.
    let p3 = app.preview(b"Tag,EID\n400,982000123456789\n401,982000999999999\n401,\n402,982000999999999\n".to_vec()).await;
    let c = app.commit(&p3).await;
    assert_eq!(
        c["errors"],
        json!([
            {"row": 2, "error": "EID 982000123456789 is on 214."},
            {"row": 4, "error": "Tag 401 is also on row 3."},
            {"row": 5, "error": "EID 982000999999999 is also on row 3."},
        ])
    );
    assert_eq!(c["created"], 1);

    // A mapping must name the tag column and only columns the file has.
    let path = format!("/api/animals/import/{}/commit", p3["import_id"].as_str().unwrap());
    let (s, e) = app.json("POST", &path, json!({"mapping": {"eid": "EID"}, "herd_id": app.herd})).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("Choose the column with the tags.")));
    let (s, _) = app.json("POST", &path, json!({"mapping": {"tag": "Tag", "breed": "Colour"}, "herd_id": app.herd})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app.json("POST", &path, json!({"mapping": {"tag": "Tag", "weight": "EID"}, "herd_id": app.herd})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn european_files_read_day_first_and_keep_quoted_notes() {
    let app = App::new().await;
    let p = app.preview(fixture("ear_tag_semicolon.csv")).await;
    let c = app.commit(&p).await;
    assert_eq!(c["created"], 2, "{c}");
    let y12 = app.animal("Y-12").await;
    // Dotted dates are day first everywhere.
    assert_eq!((y12.born, y12.notes.as_deref()), (chrono::NaiveDate::from_ymd_opt(2022, 4, 1), Some("calm; easy")));
    let p = app.preview(fixture("tag_iso_utf16.txt")).await;
    assert_eq!(app.commit(&p).await["created"], 2);
    assert_eq!(app.animal("7").await.eid.as_deref(), Some("982000323456789"));

    // A day-first file on this US farm: 13/04 says so, and 05/04 is April 5 too.
    let p = app.preview(b"Tag,DOB\n501,13/04/2021\n502,05/04/2021\n".to_vec()).await;
    assert_eq!(app.commit(&p).await["created"], 2);
    let d = |y, m, d| chrono::NaiveDate::from_ymd_opt(y, m, d);
    assert_eq!((app.animal("501").await.born, app.animal("502").await.born), (d(2021, 4, 13), d(2021, 4, 5)));
    // A month-first one: 05/04 is May 4.
    let p = app.preview(b"Tag,DOB\n601,04/13/2021\n602,05/04/2021\n".to_vec()).await;
    assert_eq!(app.commit(&p).await["created"], 2);
    assert_eq!(app.animal("602").await.born, d(2021, 5, 4));
}

#[tokio::test]
async fn a_collar_column_puts_collars_on_their_animals() {
    let app = App::new().await;
    let p = app.preview(b"Tag\n214\n215\n".to_vec()).await;
    app.commit(&p).await;
    let (s, v) =
        app.json("POST", "/api/collars/bulk", json!({"herd_id": app.herd, "items": [{"name": "C-0001"}, {"name": "C-0002"}, {"name": "C-0003"}]})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let ids: Vec<String> = v["collars"].as_array().unwrap().iter().map(|c| c["collar"]["id"].as_str().unwrap().to_owned()).collect();
    // A spare collar on the charger goes back on duty when it goes on an animal.
    app.json("POST", &format!("/api/collars/{}/park", ids[1]), json!({"reason": "charging"})).await;

    let p = app.preview(b"Tag,Collar\n214,c-0001\n215,C-0002\n216,C-0404\n".to_vec()).await;
    assert_eq!(p["mapping"]["collar"], "Collar");
    let c = app.commit(&p).await;
    assert_eq!((c["created"].as_u64(), c["updated"].as_u64()), (Some(0), Some(2)), "{c}");
    assert_eq!(c["errors"], json!([{"row": 4, "error": "No collar named C-0404."}]));
    assert_eq!(app.animal("214").await.collar_id.as_deref(), Some(ids[0].as_str()));
    let c2 = app.collar(&ids[1]).await;
    assert_eq!((c2.animal_id, c2.parked_at), (Some(app.animal("215").await.id), None));
    // Again: nothing to do, and a charging collar stays parked when nothing changes.
    app.json("POST", &format!("/api/collars/{}/park", ids[1]), json!({"reason": "charging"})).await;
    assert_eq!(app.commit(&p).await["unchanged"], 2);
    assert!(app.collar(&ids[1]).await.parked_at.is_some());

    // Clashes: a collar on another animal, an animal already wearing one, a collar of another herd.
    let (_, other) = app.json("POST", "/api/herds", json!({"name": "Heifers", "species": "cattle", "count": 1})).await;
    let (_, stray) = app.json("POST", "/api/collars", json!({"herd_id": other["id"], "name": "H-1"})).await;
    let csv = format!("Tag,Collar\n217,C-0001\n214,C-0003\n218,{}\n", stray["collar"]["id"].as_str().unwrap());
    let c = app.commit(&app.preview(csv.into_bytes()).await).await;
    assert_eq!(
        c["errors"],
        json!([
            {"row": 2, "error": "Collar C-0001 is on 214."},
            {"row": 3, "error": "214 wears C-0001. Swap it on 214's page."},
            {"row": 4, "error": "Collar H-1 is in another herd."},
        ])
    );
}

#[tokio::test]
async fn importing_250_rows_takes_under_two_seconds_and_sets_the_count() {
    let app = App::new().await;
    // The herd starts at a stated 250 with no animal rows; give it a different number first.
    let (s, _) = app.json("PATCH", &format!("/api/herds/{}", app.herd), json!({"count": 180})).await;
    assert_eq!(s, StatusCode::OK);
    let mut csv = String::from("Visual ID,EID,Name,Breed,Sex,DOB\n");
    for i in 1..=250 {
        csv += &format!("{},{},Cow {i},Angus,F,5/{}/2021\n", 1000 + i, 982_000_400_000_000u64 + i as u64, 1 + i % 28);
    }
    let t = Instant::now();
    let p = app.preview(csv.into_bytes()).await;
    let c = app.commit(&p).await;
    let took = t.elapsed();
    assert_eq!((c["created"].as_u64(), c["errors"].as_array().unwrap().len()), (Some(250), 0));
    assert!(took.as_secs_f64() < 2.0, "250 rows took {took:?}");
    assert_eq!(app.count().await, 250);
    // Idempotent at size too.
    let t = Instant::now();
    assert_eq!(app.commit(&p).await["unchanged"], 250);
    assert!(t.elapsed().as_secs_f64() < 2.0);
    // A manual count is refused now.
    let (s, e) = app.json("PATCH", &format!("/api/herds/{}", app.herd), json!({"count": 300})).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("Count follows the animals in this herd.")));
}

// Lifecycle

#[tokio::test]
async fn removing_animals_drops_the_count_unlinks_and_parks_their_collars() {
    let app = App::new().await;
    let herd = app.herd_of(250).await;
    assert_eq!(app.count().await, 250);
    // The herd and its animals on record since Sep 1.
    for sql in [
        "UPDATE herd_history SET at = '2026-09-01T12:00:00.000Z' WHERE herd_id = ?",
        "UPDATE animals SET created_at = '2026-09-01T12:00:00.000Z' WHERE herd_id = ?",
    ] {
        sqlx::query(sql).bind(&app.herd).execute(app.ctx.db()).await.unwrap();
    }
    let mut events = app.ctx.subscribe();
    for (tag, collar, _) in &herd[..3] {
        let a = app.animal(tag).await;
        let (s, v) = app.json("POST", &format!("/api/animals/{}/remove", a.id), json!({"reason": "sold", "at": "2026-09-20T15:00:00Z"})).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        assert_eq!((v["removed_reason"].as_str(), v["removed_at"].as_str()), (Some("sold"), Some("2026-09-20T15:00:00Z")));
        assert!(v.get("collar_id").is_none());
        let c = app.collar(collar).await;
        assert_eq!((c.animal_id, c.parked_reason), (None, Some(op_core::ParkReason::Shelf)));
        assert!(c.parked_at.is_some());
    }
    assert_eq!(app.count().await, 247);
    // Sold on Sep 20, entered now: the history drops to 247 on Sep 20.
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT at, count FROM herd_history WHERE herd_id = ? ORDER BY at, id").bind(&app.herd).fetch_all(app.ctx.db()).await.unwrap();
    assert_eq!(&rows[..2], [("2026-09-01T12:00:00.000Z".to_owned(), 250), ("2026-09-20T15:00:00.000Z".to_owned(), 247)]);
    assert!(rows[2..].iter().all(|(at, n)| at.as_str() > "2026-09-26" && *n == 247), "{rows:?}");
    let mut kinds = Vec::new();
    while let Ok(e) = events.try_recv() {
        kinds.push(match e {
            Event::AnimalsChanged { .. } => "animals_changed",
            Event::Collar { .. } => "collar",
            _ => "other",
        });
    }
    assert_eq!(kinds.iter().filter(|k| **k == "animals_changed").count(), 3);

    let first = app.ctx.store().get_animal(&herd_animal_id(&app, &herd[0].0).await).await.unwrap().unwrap();
    let (s, e) = app.json("POST", &format!("/api/animals/{}/remove", first.id), json!({"reason": "died"})).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::CONFLICT, Some("1 was already removed.")));
    let a = app.animal("10").await;
    let (s, _) = app.json("POST", &format!("/api/animals/{}/remove", a.id), json!({"reason": "sold", "at": "2999-01-01T00:00:00Z"})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app.json("POST", &format!("/api/animals/{}/remove", a.id), json!({"reason": "lost"})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);

    // The record stays, with an activity line.
    let events = app.ctx.store().list_events(Some(("animal", &first.id)), 10).await.unwrap();
    assert_eq!(events[0].title, "1 sold");
    assert_eq!(events[0].payload["by"], "Cody");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn removals_and_new_animals_at_once_each_count_from_their_own_date() {
    // Five sold on Sep 20 are entered while five new ones are registered in
    // the same herd. A new animal recounts the herd; landing between a
    // removal's mark and its count, it used to take that removal's backdate
    // with it, so the sold head kept counting until they were entered.
    let app = App::new().await;
    let csv: String = std::iter::once("Tag\n".to_owned()).chain((1..=20).map(|i| format!("{i}\n"))).collect();
    let p = app.preview(csv.into_bytes()).await;
    assert_eq!(app.commit(&p).await["created"], 20);
    for sql in [
        "UPDATE herd_history SET at = '2026-09-01T12:00:00.000Z' WHERE herd_id = ?",
        "UPDATE animals SET created_at = '2026-09-01T12:00:00.000Z' WHERE herd_id = ?",
    ] {
        sqlx::query(sql).bind(&app.herd).execute(app.ctx.db()).await.unwrap();
    }
    let mut tasks = tokio::task::JoinSet::new();
    for i in 1..=5 {
        let (router, removed, new) = (app.router.clone(), herd_animal_id(&app, &i.to_string()).await, format!("{}", 100 + i));
        let herd = app.herd.clone();
        tasks.spawn(async move {
            let call = |path: String, body: Value| {
                let router = router.clone();
                async move {
                    let req =
                        Request::builder().method("POST").uri(path).header("content-type", "application/json").body(Body::from(body.to_string())).unwrap();
                    router.oneshot(req).await.unwrap().status()
                }
            };
            let (a, b) = tokio::join!(
                call(format!("/api/animals/{removed}/remove"), json!({"reason": "sold", "at": "2026-09-20T15:00:00Z"})),
                call("/api/animals".into(), json!({"tag": new, "herd_id": herd})),
            );
            assert_eq!((a, b), (StatusCode::OK, StatusCode::CREATED));
        });
    }
    while let Some(r) = tasks.join_next().await {
        r.unwrap();
    }
    assert_eq!(app.count().await, 20);
    // 20 head from Sep 1, 15 from Sep 20 until the new ones came today.
    let rows: Vec<(String, i64)> =
        sqlx::query_as("SELECT at, count FROM herd_history WHERE herd_id = ? ORDER BY at, id").bind(&app.herd).fetch_all(app.ctx.db()).await.unwrap();
    let before_today: Vec<(&str, i64)> = rows.iter().filter(|(at, _)| at.as_str() < "2026-09-26").map(|(at, n)| (at.get(..10).unwrap(), *n)).collect();
    assert_eq!(before_today.last(), Some(&("2026-09-20", 15)), "{rows:?}");
    assert_eq!(before_today.iter().rev().find(|(at, _)| *at == "2026-09-01").map(|(_, n)| *n), Some(20), "{rows:?}");
    assert_eq!(before_today.len(), 3, "{rows:?}");
    // Today's rows run from 15 plus the new ones, back to 20 once all are in.
    assert_eq!(rows.last().map(|(_, n)| *n), Some(20), "{rows:?}");
}

async fn herd_animal_id(app: &App, tag: &str) -> String {
    app.ctx.store().list_animals(Some(&app.herd)).await.unwrap().into_iter().find(|a| a.tag == tag).unwrap().id
}

#[tokio::test]
async fn a_swap_keeps_the_fixes_on_the_animal() {
    let app = App::new().await;
    let herd = app.herd_of(2).await;
    let (tag, old, old_key) = herd[0].clone();
    let a = app.animal(&tag).await;
    assert_eq!(app.report(&old_key, "2026-09-26T10:00:00Z").await, StatusCode::OK);

    // A spare collar in the herd, then the swap.
    let (s, v) = app.json("POST", "/api/collars/bulk", json!({"herd_id": app.herd, "items": [{"name": "Spare 1"}]})).await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    let (new, new_key) = (v["collars"][0]["collar"]["id"].as_str().unwrap().to_owned(), v["collars"][0]["key"].as_str().unwrap().to_owned());
    let (s, v) = app.json("POST", &format!("/api/animals/{}/swap", a.id), json!({"collar_id": new})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v["collar_id"], new.as_str());
    assert_eq!(app.collar(&new).await.animal_id.as_deref(), Some(a.id.as_str()));
    let off = app.collar(&old).await;
    assert_eq!((off.animal_id, off.parked_reason), (None, Some(op_core::ParkReason::Shelf)));

    assert_eq!(app.report(&new_key, "2026-09-26T10:05:00Z").await, StatusCode::OK);
    // The parked old collar still reports, but its fixes don't count.
    assert_eq!(app.report(&old_key, "2026-09-26T10:06:00Z").await, StatusCode::OK);
    let (on_animal,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM fixes WHERE animal_id = ?").bind(&a.id).fetch_one(app.ctx.db()).await.unwrap();
    let (collars,): (i64,) =
        sqlx::query_as("SELECT COUNT(DISTINCT collar_id) FROM fixes WHERE animal_id = ?").bind(&a.id).fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!((on_animal, collars), (2, 2));

    // Refusals.
    let (s, _) = app.json("POST", &format!("/api/animals/{}/swap", a.id), json!({"collar_id": new})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let b = app.animal(&herd[1].0).await;
    let (s, e) = app.json("POST", &format!("/api/animals/{}/swap", b.id), json!({"collar_id": new})).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::CONFLICT, Some(format!("Spare 1 is on {tag}.").as_str())));
    let (_, other) = app.json("POST", "/api/herds", json!({"name": "Heifers", "species": "cattle", "count": 1})).await;
    let (_, c) = app.json("POST", "/api/collars", json!({"herd_id": other["id"]})).await;
    let (s, _) = app.json("POST", &format!("/api/animals/{}/swap", b.id), json!({"collar_id": c["collar"]["id"]})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    // A parked collar put on an animal is back on duty.
    let (s, v) = app.json("POST", &format!("/api/animals/{}/swap", b.id), json!({"collar_id": old})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert!(app.collar(&old).await.parked_at.is_none());
    assert_eq!(app.count().await, 2);
}

#[tokio::test]
async fn parked_collars_keep_battery_only_until_unparked() {
    let app = App::new().await;
    let herd = app.herd_of(1).await;
    let (_, collar, key) = herd[0].clone();
    assert_eq!(app.report(&key, "2026-09-26T10:00:00Z").await, StatusCode::OK);
    let (s, v) = app.json("POST", &format!("/api/collars/{collar}/park"), json!({"reason": "charging"})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!((v["parked_reason"].as_str(), v["state"].as_str()), (Some("charging"), Some("unknown")));
    let since = v["parked_at"].as_str().unwrap().to_owned();
    // Parking again changes the reason, not since when.
    let (_, v) = app.json("POST", &format!("/api/collars/{collar}/park"), json!({"reason": "repair"})).await;
    assert_eq!((v["parked_reason"].as_str(), v["parked_at"].as_str()), (Some("repair"), Some(since.as_str())));
    assert_eq!(app.report(&key, "2026-09-26T10:01:00Z").await, StatusCode::OK);
    let fixes = |app: &App| {
        let (db, c) = (app.ctx.db().clone(), collar.clone());
        async move { sqlx::query_as::<_, (i64,)>("SELECT COUNT(*) FROM fixes WHERE collar_id = ?").bind(c).fetch_one(&db).await.unwrap().0 }
    };
    assert_eq!(fixes(&app).await, 1);
    let (s, v) = app.json("POST", &format!("/api/collars/{collar}/unpark"), json!({})).await;
    assert_eq!(s, StatusCode::OK);
    assert!(v.get("parked_at").is_none() && v.get("parked_reason").is_none());
    assert_eq!(app.report(&key, "2026-09-26T10:02:00Z").await, StatusCode::OK);
    assert_eq!(fixes(&app).await, 2);
    let (s, _) = app.json("POST", "/api/collars/col_nope/park", json!({"reason": "shelf"})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
    let (s, _) = app.json("POST", &format!("/api/collars/{collar}/park"), json!({"reason": "lost"})).await;
    assert_eq!(s, StatusCode::UNPROCESSABLE_ENTITY);
}

// Collars

#[tokio::test]
async fn bulk_linked_keys_work_on_the_collar_endpoint() {
    let app = App::new().await;
    let p = app.preview(b"Tag\n214\n215\n216\n".to_vec()).await;
    app.commit(&p).await;

    // With a header, in any column order, and a spare collar with no animal.
    let (s, v) = app.csv(&format!("/api/collars/bulk?herd_id={}", app.herd), "Collar name,Tag\nC-0001,214\nC-0002,215\nC-0099,\n").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert!(v["batch_id"].as_str().unwrap().starts_with("bat_"));
    let collars = v["collars"].as_array().unwrap();
    assert_eq!(collars.len(), 3);
    assert_eq!((collars[0]["tag"].as_str(), collars[0]["collar"]["name"].as_str()), (Some("214"), Some("C-0001")));
    assert!(collars[2].get("tag").is_none());
    assert_eq!(collars[0]["endpoint"], format!("{}/collar/v1", app.ctx.base_url()));
    assert_eq!(collars[0]["public_key"], app.ctx.public_key_b64());
    for c in collars {
        assert_eq!(app.report(c["key"].as_str().unwrap(), "2026-09-26T10:00:00Z").await, StatusCode::OK);
    }
    let a = app.animal("214").await;
    assert_eq!(a.collar_id.as_deref(), collars[0]["collar"]["id"].as_str());
    assert_eq!(app.collar(a.collar_id.as_deref().unwrap()).await.animal_id.as_deref(), Some(a.id.as_str()));

    // Without a header; a missing name takes the tag.
    let (s, v) = app.csv(&format!("/api/collars/bulk?herd_id={}", app.herd), "216,\n").await;
    assert_eq!(s, StatusCode::CREATED, "{v}");
    assert_eq!(v["collars"][0]["collar"]["name"], "216");

    // Rows to fix: nothing is created.
    let before: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM collars").fetch_one(app.ctx.db()).await.unwrap();
    let (s, e) = app.csv(&format!("/api/collars/bulk?herd_id={}", app.herd), "tag,collar name\n999,C-0100\n214,C-0101\n,C-0001\n,X\n,X\n").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        e["errors"],
        json!([
            {"row": 2, "error": "No animal tagged 999 in this herd."},
            {"row": 3, "error": "214 already wears C-0001."},
            {"row": 4, "error": "A collar named C-0001 is already in this herd."},
            {"row": 6, "error": "Collar X is also on row 5."},
        ])
    );
    assert!(e["error"].as_str().unwrap().starts_with("4 rows to fix, nothing linked. row 2: No animal tagged 999"));
    let after: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM collars").fetch_one(app.ctx.db()).await.unwrap();
    assert_eq!(before, after);
    let (s, _) = app.csv("/api/collars/bulk", "214,C\n").await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = app.json("POST", "/api/collars/bulk", json!({"herd_id": app.herd, "items": []})).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn a_new_key_shuts_out_the_old_one_and_only_the_owner_can_make_it() {
    let app = App::new().await;
    let herd = app.herd_of(1).await;
    let (tag, collar, old_key) = herd[0].clone();
    let (s, v) = app.json("POST", &format!("/api/collars/{collar}/rekey"), json!({})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let new_key = v["key"].as_str().unwrap();
    assert_ne!(new_key, old_key);
    assert_eq!((v["tag"].as_str(), v["collar"]["id"].as_str()), (Some(tag.as_str()), Some(collar.as_str())));
    assert_eq!(app.report(&old_key, "2026-09-26T10:00:00Z").await, StatusCode::UNAUTHORIZED);
    assert_eq!(app.report(new_key, "2026-09-26T10:00:00Z").await, StatusCode::OK);

    let manager = App::as_role(Role::Manager).await;
    let herd = manager.herd_of(1).await;
    let (s, e) = manager.json("POST", &format!("/api/collars/{}/rekey", herd[0].1), json!({})).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::FORBIDDEN, Some("Your role can't do this.")));
}

#[tokio::test]
async fn cards_need_the_public_https_url_and_the_current_key() {
    let app = App::new().await;
    let herd = app.herd_of(2).await;
    let items = json!({"items": herd.iter().map(|(t, c, k)| json!({"collar_id": c, "key": k, "tag": t})).collect::<Vec<_>>()});
    let (s, e) = app.json("POST", "/api/cards", items.clone()).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::CONFLICT, Some("Cards need the server's public URL (Settings, Server): it goes into every collar.")));
    app.ctx.update_settings(&json!({"server": {"public_url": "http://192.168.1.20:7878"}})).await.unwrap();
    let (s, _) = app.json("POST", "/api/cards", items.clone()).await;
    assert_eq!(s, StatusCode::CONFLICT);

    app.ctx.update_settings(&json!({"server": {"public_url": "https://farm.example.com/"}})).await.unwrap();
    let (s, v) = app.json("POST", "/api/cards", items).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    assert_eq!(v.as_array().unwrap().len(), 2);
    assert_eq!(v[1]["collar_id"], herd[1].1.as_str());
    assert!(v[0]["qr_svg"].as_str().unwrap().starts_with("<svg"));

    let (s, e) = app.json("POST", "/api/cards", json!({"items": [{"collar_id": herd[0].1, "key": "0".repeat(64)}]})).await;
    assert_eq!((s, e["error"].as_str()), (StatusCode::BAD_REQUEST, Some("That isn't C-0001's key now. Give it a new key to print its card.")));
    let (s, _) = app.json("POST", "/api/cards", json!({"items": [{"collar_id": "col_nope", "key": herd[0].2}]})).await;
    assert_eq!(s, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn a_card_decodes_to_the_provisioning_payload() {
    let app = App::new().await;
    app.ctx.update_settings(&json!({"server": {"public_url": "https://farm.example.com"}})).await.unwrap();
    let herd = app.herd_of(1).await;
    let (_, collar, key) = &herd[0];
    let (s, v) = app.json("POST", "/api/cards", json!({"items": [{"collar_id": collar, "key": key}]})).await;
    assert_eq!(s, StatusCode::OK, "{v}");
    let modules = qr::modules_from_svg(v[0]["qr_svg"].as_str().unwrap());
    let text = String::from_utf8(qr::decode_m(&modules)).unwrap();
    let want =
        format!(r#"{{"v":1,"c":"{collar}","h":"{}","k":"{key}","e":"https://farm.example.com/collar/v1","s":"{}"}}"#, app.herd, app.ctx.public_key_b64());
    assert_eq!(text, want);
    // And that is exactly what the collar needs.
    let p: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(p["v"], 1);
    assert_eq!(op_ingest::collar_key_matches(&app.ctx, collar, p["k"].as_str().unwrap()).await.unwrap(), true);
}

/// A small QR reader for the cards' codes: model 2, error correction M,
/// numeric, alphanumeric and byte segments. It reads what the qrcode crate
/// drew, independently of how it encodes: mask found by trying all eight
/// (only the right one leaves valid padding), codewords read in the
/// standard zigzag and de-interleaved with the standard block table.
mod qr {
    use qrcode::Version;

    /// Level M blocks per version: (count, data codewords) for each group.
    const M: [&[(usize, usize)]; 20] = [
        &[(1, 16)],
        &[(1, 28)],
        &[(1, 44)],
        &[(2, 32)],
        &[(2, 43)],
        &[(4, 27)],
        &[(4, 31)],
        &[(2, 38), (2, 39)],
        &[(3, 36), (2, 37)],
        &[(4, 43), (1, 44)],
        &[(1, 50), (4, 51)],
        &[(6, 36), (2, 37)],
        &[(8, 37), (1, 38)],
        &[(4, 40), (5, 41)],
        &[(5, 41), (5, 42)],
        &[(7, 45), (3, 46)],
        &[(10, 46), (1, 47)],
        &[(9, 43), (4, 44)],
        &[(3, 44), (11, 45)],
        &[(3, 41), (13, 42)],
    ];

    /// Dark modules of the SVG (one unit per module), quiet zone removed.
    pub fn modules_from_svg(svg: &str) -> Vec<Vec<bool>> {
        let w: usize = svg.split("width=\"").nth(1).unwrap().split('"').next().unwrap().parse().unwrap();
        let mut g = vec![vec![false; w]; w];
        let d = svg.split(" d=\"").nth(1).unwrap().split('"').next().unwrap();
        for rect in d.split('M').filter(|s| !s.is_empty()) {
            // "{left} {top}h{w}v{h}H{left}V{top}"
            let (pos, rest) = rect.split_once('h').unwrap();
            let (l, t) = pos.split_once(' ').unwrap();
            let (rw, rest) = rest.split_once('v').unwrap();
            let rh = rest.split('H').next().unwrap();
            let (l, t, rw, rh): (usize, usize, usize, usize) = (l.parse().unwrap(), t.parse().unwrap(), rw.parse().unwrap(), rh.parse().unwrap());
            for row in g.iter_mut().skip(t).take(rh) {
                for m in row.iter_mut().skip(l).take(rw) {
                    *m = true;
                }
            }
        }
        g[4..w - 4].iter().map(|r| r[4..w - 4].to_vec()).collect()
    }

    fn masked(mask: u8, i: usize, j: usize) -> bool {
        match mask {
            0 => (i + j) % 2 == 0,
            1 => i % 2 == 0,
            2 => j % 3 == 0,
            3 => (i + j) % 3 == 0,
            4 => (i / 2 + j / 3) % 2 == 0,
            5 => (i * j) % 2 + (i * j) % 3 == 0,
            6 => ((i * j) % 2 + (i * j) % 3) % 2 == 0,
            _ => ((i + j) % 2 + (i * j) % 3) % 2 == 0,
        }
    }

    /// The bytes a level-M code holds.
    pub fn decode_m(g: &[Vec<bool>]) -> Vec<u8> {
        let n = g.len();
        let v = (n - 17) / 4;
        assert!((1..=20).contains(&v) && 17 + 4 * v == n, "size {n}");
        let blocks: Vec<usize> = M[v - 1].iter().flat_map(|&(k, d)| std::iter::repeat_n(d, k)).collect();
        // The table agrees with the encoder's capacity.
        assert_eq!(blocks.iter().sum::<usize>() * 8, qrcode::bits::Bits::new(Version::Normal(v as i16)).max_len(qrcode::EcLevel::M).unwrap());
        (0..8).find_map(|mask| read(g, v, &blocks, mask)).expect("no mask gives a valid level-M code")
    }

    fn read(g: &[Vec<bool>], v: usize, blocks: &[usize], mask: u8) -> Option<Vec<u8>> {
        let n = g.len();
        // The crate's check leaves out the version blocks (6 x 3, from version 7 on).
        let version_info = |x: usize, y: usize| v >= 7 && ((x < 6 && (n - 11..n - 8).contains(&y)) || (y < 6 && (n - 11..n - 8).contains(&x)));
        let func = |x: usize, y: usize| version_info(x, y) || qrcode::canvas::is_functional(Version::Normal(v as i16), n as i16, x as i16, y as i16);
        let mut bits = Vec::new();
        let mut x = n as isize - 1;
        let mut up = true;
        while x > 0 {
            if x == 6 {
                x -= 1;
            }
            for k in 0..n {
                let y = if up { n - 1 - k } else { k };
                for dx in 0..2 {
                    let xx = (x - dx) as usize;
                    if !func(xx, y) {
                        bits.push(g[y][xx] ^ masked(mask, y, xx));
                    }
                }
            }
            up = !up;
            x -= 2;
        }
        let cws: Vec<u8> = bits.chunks_exact(8).map(|c| c.iter().fold(0u8, |a, &b| (a << 1) | b as u8)).collect();
        // Data codewords are interleaved across blocks: the i-th of each in turn.
        let mut data: Vec<Vec<u8>> = blocks.iter().map(|&d| Vec::with_capacity(d)).collect();
        let mut at = 0;
        for i in 0..*blocks.iter().max()? {
            for (b, &d) in blocks.iter().enumerate() {
                if i < d {
                    data[b].push(*cws.get(at)?);
                    at += 1;
                }
            }
        }
        let data: Vec<u8> = data.concat();
        let bit = |i: usize| data[i / 8] >> (7 - i % 8) & 1 == 1;
        let total = data.len() * 8;
        let mut p = 0;
        let take = |p: &mut usize, k: usize| -> Option<u32> {
            if *p + k > total {
                return None;
            }
            let v = (0..k).fold(0u32, |a, i| (a << 1) | bit(*p + i) as u32);
            *p += k;
            Some(v)
        };
        let wide = v >= 10;
        let mut out = Vec::new();
        loop {
            let mode = if p + 4 <= total { take(&mut p, 4)? } else { 0 };
            match mode {
                0 => break,
                1 => {
                    let mut count = take(&mut p, if wide { 12 } else { 10 })? as usize;
                    while count >= 3 {
                        out.extend(format!("{:03}", take(&mut p, 10)?).bytes());
                        count -= 3;
                    }
                    match count {
                        2 => out.extend(format!("{:02}", take(&mut p, 7)?).bytes()),
                        1 => out.extend(format!("{}", take(&mut p, 4)?).bytes()),
                        _ => {}
                    }
                }
                2 => {
                    const AN: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ $%*+-./:";
                    let mut count = take(&mut p, if wide { 11 } else { 9 })? as usize;
                    while count >= 2 {
                        let pair = take(&mut p, 11)? as usize;
                        out.push(*AN.get(pair / 45)?);
                        out.push(*AN.get(pair % 45)?);
                        count -= 2;
                    }
                    if count == 1 {
                        out.push(*AN.get(take(&mut p, 6)? as usize)?);
                    }
                }
                4 => {
                    let count = take(&mut p, if wide { 16 } else { 8 })?;
                    for _ in 0..count {
                        out.push(take(&mut p, 8)? as u8);
                    }
                }
                _ => return None,
            }
        }
        // After the terminator: zero bits to a byte boundary, then 0xEC 0x11 padding.
        let rest = p.div_ceil(8);
        if (p..(rest * 8).min(total)).any(bit) {
            return None;
        }
        if !data[rest..].iter().enumerate().all(|(i, &b)| b == if i % 2 == 0 { 0xEC } else { 0x11 }) {
            return None;
        }
        Some(out)
    }
}

/// The reader below reads what the encoder wrote, across the sizes and
/// segment kinds cards use, so a failure above is about the card.
#[test]
fn the_card_reader_reads_every_size_a_card_can_be() {
    let long_url = format!("{}/collar/v1", "https://a-very-long-farm-name.example.com/".repeat(3));
    let texts = ["hello".to_owned(), "HELLO WORLD 123".to_owned(), "x".repeat(110), "a1B".repeat(80), "0123456789".repeat(30), long_url, "x".repeat(330)];
    for text in texts {
        let code = qrcode::QrCode::with_error_correction_level(text.as_bytes(), qrcode::EcLevel::M).unwrap();
        let svg = code.render::<qrcode::render::svg::Color>().module_dimensions(1, 1).quiet_zone(true).build();
        assert_eq!(qr::decode_m(&qr::modules_from_svg(&svg)), text.as_bytes(), "{:?}", code.version());
    }
}
