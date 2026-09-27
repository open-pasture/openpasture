//! The welfare record report (field-ready H): per animal, cues by kind, tone
//! seconds (a day on average, the most in one farm day), the longest
//! episode and the loudest level; episodes by outcome, trained or learning
//! and since when, a marker for episodes rebuilt from fixes; fit checks and
//! times the collar lay still; the farm days with cues; the method notes;
//! and the same as CSV.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use op_core::{Animal, Ctx, Identity, Via, time, with_identity};
use serde_json::{Value, json};
use tower::ServiceExt;

struct App {
    _dir: tempfile::TempDir,
    ctx: Ctx,
    router: axum::Router,
}

impl App {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        let router = with_identity(op_core::router().merge(op_reports::router()), Identity::owner(Via::Local)).with_state(ctx.clone());
        Self { _dir: dir, ctx, router }
    }

    async fn send(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, String, String) {
        let mut req = Request::builder().method(method).uri(path);
        let body = match body {
            Some(b) => {
                req = req.header("content-type", "application/json");
                Body::from(b.to_string())
            }
            None => Body::empty(),
        };
        let res = self.router.clone().oneshot(req.body(body).unwrap()).await.unwrap();
        let status = res.status();
        let ctype = res.headers().get("content-type").map(|v| v.to_str().unwrap().to_owned()).unwrap_or_default();
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        (status, ctype, String::from_utf8(bytes.to_vec()).unwrap())
    }

    async fn ok(&self, method: &str, path: &str, body: Option<Value>) -> Value {
        let (s, _, text) = self.send(method, path, body).await;
        assert!(s.is_success(), "{method} {path}: {s} {text}");
        serde_json::from_str(&text).unwrap()
    }

    async fn exec(&self, sql: &str, binds: &[Value]) {
        let mut q = sqlx::query(sql);
        for b in binds {
            q = match b {
                Value::String(s) => q.bind(s.clone()),
                Value::Number(n) if n.is_i64() => q.bind(n.as_i64()),
                Value::Number(n) => q.bind(n.as_f64()),
                Value::Bool(v) => q.bind(*v as i64),
                _ => q.bind(None::<String>),
            };
        }
        q.execute(self.ctx.db()).await.unwrap();
    }
}

fn ms(s: &str) -> i64 {
    time::from_db(s).unwrap().timestamp_millis()
}

/// A cue of `animal` on `collar` at `at`; `legacy` stores it as firmware 0.1 does.
async fn cue(app: &App, collar: &str, animal: &str, at: &str, kind: &str, level: i64, legacy: bool) {
    let margin = if kind == "outside" { -1.2 } else { 2.0 };
    app.exec(
        "INSERT INTO cues (collar_id, herd_id, animal_id, at, t, level, margin_m, boundary_version, kind, ring, dur_ms) VALUES (?, 'herd_1', ?, ?, ?, ?, ?, 7, ?, ?, ?)",
        &[
            json!(collar),
            json!(animal),
            json!(at),
            json!(ms(at)),
            json!(level),
            json!(margin),
            if legacy { Value::Null } else { json!(kind) },
            if legacy { Value::Null } else { json!(0) },
            if legacy { Value::Null } else { json!(400) },
        ],
    )
    .await;
}

async fn episode(app: &App, collar: &str, animal: &str, start: &str, secs: i64, outcome: &str, derived: bool) {
    let s = ms(start);
    app.exec(
        "INSERT INTO episodes (id, collar_id, herd_id, animal_id, start_t, end_t, start_at, end_at, ring, cues, max_level, min_margin_m, outcome, derived)
         VALUES (?, ?, 'herd_1', ?, ?, ?, ?, ?, 0, 2, 3, 1.0, ?, ?)",
        &[
            json!(op_core::id::new_id("epi")),
            json!(collar),
            json!(animal),
            json!(s),
            json!(s + secs * 1000),
            json!(start),
            json!(time::to_db(&time::from_unix_ms(s + secs * 1000))),
            json!(outcome),
            json!(derived),
        ],
    )
    .await;
}

/// Farm in America/Chicago; herd Cows with 214 (collar A, firmware 0.2), 031
/// (collar B, firmware 0.1) and 118 (no cues). September 2026.
async fn farm() -> App {
    let app = App::new().await;
    app.ok("POST", "/api/farm", Some(json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]}))).await;
    app.ctx.update_settings(&json!({ "units": "metric" })).await.unwrap();
    let s = app.ctx.store();
    s.insert_herd(&op_core::Herd {
        id: "herd_1".into(),
        name: "Cows".into(),
        species: op_core::Species::Cattle,
        count: 3,
        paddock_id: None,
        autonomy: op_core::Autonomy::Propose,
        timer_minutes: 60,
        created_at: time::from_db("2026-09-01T00:00:00Z").unwrap(),
    })
    .await
    .unwrap();
    for (c, a, tag) in [("col_a", "ani_a", "214"), ("col_b", "ani_b", "031"), ("col_c", "ani_c", "118")] {
        app.exec("INSERT INTO collars (id, name, herd_id, created_at) VALUES (?, ?, 'herd_1', '2026-09-01T00:00:00.000Z')", &[json!(c), json!(c)]).await;
        s.insert_animal(&Animal { id: a.into(), tag: tag.into(), herd_id: "herd_1".into(), collar_id: Some(c.into()), ..Default::default() }).await.unwrap();
    }
    // 214 on Sep 20 (07:00 and 20:00 on the farm, two UTC days): 3 warn, 1 outside, episodes 12 s and 30 s.
    cue(&app, "col_a", "ani_a", "2026-09-20T12:00:00.000Z", "warn", 1, false).await;
    cue(&app, "col_a", "ani_a", "2026-09-20T12:00:01.000Z", "warn", 3, false).await;
    cue(&app, "col_a", "ani_a", "2026-09-21T01:00:00.000Z", "warn", 4, false).await;
    cue(&app, "col_a", "ani_a", "2026-09-21T01:00:05.000Z", "outside", 4, false).await;
    episode(&app, "col_a", "ani_a", "2026-09-20T12:00:00.000Z", 12, "turned_back", false).await;
    episode(&app, "col_a", "ani_a", "2026-09-21T01:00:00.000Z", 30, "crossed", false).await;
    // 031 on Sep 21: three warn cues from firmware 0.1, five turned-back episodes rebuilt from fixes.
    for s in 0..3 {
        cue(&app, "col_b", "ani_b", &format!("2026-09-21T15:00:0{s}.000Z"), "warn", 2, true).await;
    }
    for i in 0..5 {
        episode(&app, "col_b", "ani_b", &format!("2026-09-21T15:0{i}:00.000Z"), 4, "turned_back", true).await;
    }
    // A fit check of 031's collar and a time 214's lay still.
    app.exec(
        "INSERT INTO collar_fit_checks (id, collar_id, checked_at, \"by\") VALUES ('fit_1', 'col_b', '2026-09-21T18:00:00.000Z', '{\"via\":\"local\"}')",
        &[],
    )
    .await;
    app.exec(
        "INSERT INTO alerts (id, kind, key, severity, status, herd_id, title, targets, opened_at, updated_at, resolved_at, seen_at)
         VALUES ('alr_1', 'drop_off', 'drop_off:col_a', 'warning', 'resolved', 'herd_1', '214 not moving', '[[\"collar\",\"col_a\"]]', '2026-09-20T13:00:00.000Z', '2026-09-20T15:00:00.000Z', '2026-09-20T15:00:00.000Z', '2026-09-20T15:00:00.000Z')",
        &[],
    )
    .await;
    app
}

const DATES: &str = "from=2026-09-19&to=2026-09-22";

fn section<'a>(doc: &'a Value, title: &str) -> &'a Value {
    doc["sections"].as_array().unwrap().iter().find(|s| s["title"] == title).unwrap_or_else(|| panic!("no section {title}: {doc}"))
}

fn row<'a>(s: &'a Value, tag: &str) -> Vec<&'a Value> {
    s["rows"].as_array().unwrap().iter().find(|r| r[0] == tag).unwrap_or_else(|| panic!("no row {tag}: {s}")).as_array().unwrap().iter().collect()
}

#[tokio::test]
async fn the_welfare_record_per_animal_with_its_method_notes() {
    let app = farm().await;
    let list = app.ok("GET", "/api/reports", None).await;
    assert!(list.as_array().unwrap().iter().any(|r| r["id"] == "welfare" && r["title"] == "Welfare record"), "{list}");
    let doc = app.ok("GET", &format!("/api/reports/welfare?{DATES}&herd_id=herd_1"), None).await;
    assert_eq!(doc["title"], "Welfare record");
    assert_eq!(doc["signatures"], json!(["Operator"]));

    let cues = section(&doc, "Cues");
    let labels: Vec<&str> = cues["columns"].as_array().unwrap().iter().map(|c| c["label"].as_str().unwrap()).collect();
    assert_eq!(labels, ["Tag", "Warn cues", "Outside cues", "Tone", "Tone a day", "Most in a day", "Longest episode", "Loudest level"]);
    assert_eq!(cues["columns"][3]["unit"], "s");
    let tags: Vec<&str> = cues["rows"].as_array().unwrap().iter().map(|r| r[0].as_str().unwrap()).collect();
    assert_eq!(tags, ["031", "118", "214"], "every animal, in tag order");
    // 214: 4 cues × 0.4 s = 1.6 s, all on one farm day (Sep 20); 4 days in the dates.
    assert_eq!(row(cues, "214"), [&json!("214"), &json!(3), &json!(1), &json!(1.6), &json!(0.4), &json!(1.6), &json!(30.0), &json!(4)]);
    // 031: firmware 0.1 reports no tone length, 0.3 s a cue.
    assert_eq!(row(cues, "031"), [&json!("031"), &json!(3), &json!(0), &json!(0.9), &json!(0.2), &json!(0.9), &json!(4.0), &json!(2)]);
    assert_eq!(row(cues, "118")[1..4], [&json!(0), &json!(0), &json!(0.0)]);
    assert_eq!(cues["totals"], json!(["Total", 6, 1, 2.5, 0.6, null, null, null]));

    let learning = section(&doc, "Learning");
    let r = row(learning, "031");
    assert_eq!(r[1..5], [&json!(5), &json!(0), &json!(0), &json!(0)]);
    assert_eq!((r[5], r[6], r[7]), (&json!("trained"), &json!("2026-09-21"), &json!("Yes")));
    let r = row(learning, "214");
    assert_eq!((r[1], r[2], r[5], r[6], r[7]), (&json!(1), &json!(1), &json!("learning"), &json!("2026-09-20"), &Value::Null));
    assert_eq!(row(learning, "118")[5], &Value::Null, "no episodes, no status");

    let care = section(&doc, "Collar care");
    assert_eq!(row(care, "031")[1..], [&json!(1), &json!("2026-09-21"), &json!(0)]);
    assert_eq!(row(care, "214")[1..], [&json!(0), &Value::Null, &json!(1)]);

    let days = section(&doc, "Days with cues");
    assert_eq!(days["rows"], json!([["2026-09-20", 1, 3, 1, 1.6, 30.0], ["2026-09-21", 1, 3, 0, 0.9, 4.0]]));

    let notes: Vec<&str> = doc["notes"].as_array().unwrap().iter().map(|n| n.as_str().unwrap()).collect();
    assert_eq!(notes[0], op_reports::AUDIO_ONLY_NOTE);
    assert!(notes.iter().any(|n| n.starts_with("Warn: the warning tone")));
    assert!(notes.iter().any(|n| n.starts_with("Trained: 5 turned-back episodes in a row")));
    assert!(notes.iter().any(|n| n.starts_with("From fixes:")));
    assert!(notes.iter().any(|n| n.contains("each of their cues counts 0.3 s")));
    assert!(notes.iter().any(|n| n == &"Days are farm days (America/Chicago)."));

    // Status is as of the end of the dates: before 031's episodes it was learning nothing yet.
    let early = app.ok("GET", "/api/reports/welfare?from=2026-09-19&to=2026-09-20&herd_id=herd_1", None).await;
    assert_eq!(row(section(&early, "Learning"), "031")[5], &Value::Null);
    assert_eq!(section(&early, "Days with cues")["rows"], json!([["2026-09-20", 1, 3, 1, 1.6, 30.0]]));
    // Every herd: a Herd column.
    let all = app.ok("GET", &format!("/api/reports/welfare?{DATES}"), None).await;
    assert_eq!(section(&all, "Cues")["columns"][1]["label"], "Herd");
    assert_eq!(row(section(&all, "Cues"), "214")[1], &json!("Cows"));
}

#[tokio::test]
async fn the_welfare_record_as_csv() {
    let app = farm().await;
    let (s, ctype, text) = app.send("GET", &format!("/api/reports/welfare?{DATES}&herd_id=herd_1&format=csv"), None).await;
    assert_eq!((s, ctype.as_str()), (StatusCode::OK, "text/csv; charset=utf-8"));
    let rows: Vec<Vec<String>> = csv::ReaderBuilder::new()
        .has_headers(false)
        .flexible(true)
        .from_reader(text.as_bytes())
        .records()
        .map(|r| r.unwrap().iter().map(str::to_owned).collect())
        .collect();
    assert_eq!(rows[0][0], "Welfare record");
    let at = |first: &str| rows.iter().position(|r| r.first().map(String::as_str) == Some(first)).unwrap_or_else(|| panic!("no {first} row"));
    let h = at("Cues");
    assert_eq!(rows[h + 1][..4], ["Tag", "Warn cues", "Outside cues", "Tone (s)"]);
    let r214 = rows.iter().find(|r| r.first().map(String::as_str) == Some("214")).unwrap();
    assert_eq!(r214[..4], ["214", "3", "1", "1.6"]);
    // 118 has no cues: its tone is an empty sum, which reads 0.0, never -0.0.
    let r118 = rows.iter().find(|r| r.first().map(String::as_str) == Some("118")).unwrap();
    assert_eq!(r118[..5], ["118", "0", "0", "0.0", "0.0"]);
    assert!(!text.contains("-0.0"), "{text}");
    at("Learning");
    at("Collar care");
    at("Days with cues");
    assert!(text.contains(op_reports::AUDIO_ONLY_NOTE));
}
