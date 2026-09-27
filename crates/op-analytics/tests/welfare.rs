//! The welfare record (field-ready H): one track walked by two collars, one on
//! firmware 0.2 that reports its episodes and one on firmware 0.1 that
//! doesn't, gives the same ledger, the same episodes and the same learning
//! status once the server has rebuilt the second's; learning status; farm
//! days; training mode; map ticks; the record read back from Parquet; fit
//! checks and still collars; `get_welfare`.

mod g_support;

use chrono::Duration;
use g_support::*;
use op_analytics::welfare;
use op_core::{Identity, Role, Via, time};
use op_geo::{CollarLimits, Cue, CueConfig, Geofence, GeofenceConfig, GeofenceState, LonLat, Polygon, Projection};
use serde_json::{Value, json};

/// A 100 m square with its south-west corner at the grid origin.
fn square() -> Polygon {
    let p = Projection::new(ORIGIN);
    Polygon::from_ring(vec![p.offset(0.0, 0.0), p.offset(100.0, 0.0), p.offset(100.0, 100.0), p.offset(0.0, 100.0)])
}

/// Where the animal is each second, in metres east and north of the corner,
/// and the boundary version its collar holds.
fn walk() -> Vec<([f64; 2], u32)> {
    let mut out: Vec<([f64; 2], u32)> = Vec::new();
    let mut at = [50.0, 50.0];
    let mut v = 7;
    let go = |out: &mut Vec<([f64; 2], u32)>, at: &mut [f64; 2], to: [f64; 2], steps: usize, v: u32| {
        let from = *at;
        for k in 1..=steps {
            let f = k as f64 / steps as f64;
            *at = [from[0] + (to[0] - from[0]) * f, from[1] + (to[1] - from[1]) * f];
            out.push((*at, v));
        }
    };
    let hold = |out: &mut Vec<([f64; 2], u32)>, at: [f64; 2], steps: usize, v: u32| out.extend(std::iter::repeat_n((at, v), steps));
    hold(&mut out, at, 5, v);
    // Twice up to the north edge and back: turned back.
    for _ in 0..2 {
        go(&mut out, &mut at, [50.0, 96.5], 10, v);
        hold(&mut out, at, 3, v);
        go(&mut out, &mut at, [50.0, 88.0], 4, v);
        hold(&mut out, at, 3, v);
    }
    // In the warning zone for a minute: 20 s of tones, a rest, tones again, then back.
    go(&mut out, &mut at, [50.0, 97.0], 6, v);
    hold(&mut out, at, 60, v);
    go(&mut out, &mut at, [50.0, 88.0], 4, v);
    hold(&mut out, at, 3, v);
    // Across the east edge, a while outside, then back in.
    go(&mut out, &mut at, [104.0, 88.0], 12, v);
    hold(&mut out, at, 15, v);
    go(&mut out, &mut at, [80.0, 60.0], 8, v);
    hold(&mut out, at, 3, v);
    // A new boundary arrives while it is being warned.
    go(&mut out, &mut at, [50.0, 97.0], 8, v);
    hold(&mut out, at, 2, v);
    v = 8;
    hold(&mut out, at, 3, v);
    go(&mut out, &mut at, [50.0, 88.0], 4, v);
    hold(&mut out, at, 3, v);
    // Five more times up and back: trained again.
    for _ in 0..5 {
        go(&mut out, &mut at, [50.0, 96.5], 8, v);
        hold(&mut out, at, 2, v);
        go(&mut out, &mut at, [50.0, 88.0], 4, v);
        hold(&mut out, at, 2, v);
    }
    out
}

struct Fx {
    t: i64,
    point: LonLat,
    state: GeofenceState,
    margin: f64,
    version: u32,
}

struct Played {
    t: i64,
    point: LonLat,
    warn: bool,
    level: u8,
    margin: f64,
    ring: usize,
    version: u32,
}

/// The collar's own fence and cue policy over the walk, a fix a second
/// from `t0`: what it stored (fixes with the fence state), played and ended.
fn run(t0: i64) -> (Vec<Fx>, Vec<Played>, Vec<(op_geo::Episode, u32)>) {
    let proj = Projection::new(ORIGIN);
    let mut fence = Geofence::from_polygon(GeofenceConfig::default(), &square(), 7, &CollarLimits::V0).unwrap();
    let mut cue = Cue::new(CueConfig::default());
    let (mut fixes, mut played, mut eps) = (Vec::new(), Vec::new(), Vec::new());
    let mut held = 7;
    for (i, (xy, v)) in walk().into_iter().enumerate() {
        let t = t0 + i as i64 * 1000;
        if v != held {
            cue.rearm_at(t);
            eps.extend(cue.take_episodes().into_iter().map(|e| (e, held)));
            held = v;
        }
        let point = proj.offset(xy[0], xy[1]);
        let r = fence.update(point, 2.0);
        let c = cue.update(&r, 5.0, t);
        fixes.push(Fx { t, point, state: r.state, margin: r.margin_m, version: v });
        if c.active {
            played.push(Played {
                t,
                point,
                warn: c.kind == Some(op_geo::CueKind::Warn),
                level: c.volume,
                margin: (r.margin_m * 10.0).round() / 10.0,
                ring: r.nearest_ring,
                version: v,
            });
        }
        eps.extend(cue.take_episodes().into_iter().map(|e| (e, v)));
    }
    (fixes, played, eps)
}

/// Store what a collar reported, as op-ingest does: firmware 0.2 with cue
/// kinds, tone lengths, rings and its episodes; firmware 0.1 with none of
/// those.
async fn store(app: &App, collar: &str, animal: &str, v02: bool, fixes: &[Fx], played: &[Played], eps: &[(op_geo::Episode, u32)]) {
    let db = app.ctx.db();
    for f in fixes {
        sqlx::query(
            "INSERT INTO fixes (collar_id, herd_id, animal_id, at, t, lon, lat, accuracy_m, sats, boundary_version, state, margin_m) VALUES (?, 'herd_1', ?, ?, ?, ?, ?, 2.0, 9, ?, ?, ?)",
        )
        .bind(collar)
        .bind(animal)
        .bind(time::to_db(&time::from_unix_ms(f.t)))
        .bind(f.t)
        .bind(f.point[0])
        .bind(f.point[1])
        .bind(f.version as i64)
        .bind(f.state.as_str())
        .bind(f.margin)
        .execute(db)
        .await
        .unwrap();
    }
    for c in played {
        let kind = if c.warn { "warn" } else { "outside" };
        sqlx::query(
            "INSERT INTO cues (collar_id, herd_id, animal_id, at, t, level, margin_m, lon, lat, boundary_version, kind, ring, dur_ms) VALUES (?, 'herd_1', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(collar)
        .bind(animal)
        .bind(time::to_db(&time::from_unix_ms(c.t)))
        .bind(c.t)
        .bind(c.level as i64)
        .bind(c.margin)
        .bind(c.point[0])
        .bind(c.point[1])
        .bind(c.version as i64)
        .bind(v02.then_some(kind))
        .bind(v02.then_some(c.ring as i64))
        .bind(v02.then_some(300i64))
        .execute(db)
        .await
        .unwrap();
    }
    if v02 {
        for (i, (e, v)) in eps.iter().enumerate() {
            sqlx::query(
                "INSERT INTO episodes (id, collar_id, herd_id, animal_id, start_t, end_t, start_at, end_at, boundary_version, ring, cues, max_level, min_margin_m, outcome)
                 VALUES (?, ?, 'herd_1', ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(format!("epi_{collar}_{i}"))
            .bind(collar)
            .bind(animal)
            .bind(e.start)
            .bind(e.end)
            .bind(time::to_db(&time::from_unix_ms(e.start)))
            .bind(time::to_db(&time::from_unix_ms(e.end)))
            .bind(*v as i64)
            .bind(e.ring as i64)
            .bind(e.cues as i64)
            .bind(e.max_level as i64)
            .bind(e.min_margin_m)
            .bind(e.outcome.as_str())
            .execute(db)
            .await
            .unwrap();
        }
    }
}

/// Yesterday 12:00 UTC (07:00 on the farm).
fn start() -> i64 {
    midnight(time::now() - Duration::days(1)).timestamp_millis() + 12 * HOUR
}

/// Two collars on animals 101 and 102 walking the same track, 101 on
/// firmware 0.2, 102 on 0.1; the server rebuilds 102's episodes.
async fn two_collars() -> (App, i64, usize) {
    let app = App::new(2).await;
    let t0 = start();
    let (fixes, played, eps) = run(t0);
    store(&app, "col_1", "ani_1", true, &fixes, &played, &eps).await;
    store(&app, "col_2", "ani_2", false, &fixes, &played, &eps).await;
    let pass = welfare::derive(&app.ctx, time::now().timestamp_millis()).await.unwrap();
    assert_eq!((pass.collars, pass.waiting), (1, 0), "{pass:?}");
    assert_eq!(pass.episodes, eps.len());
    (app, t0, played.len())
}

/// A time in a response, as unix ms.
fn ms(v: &Value) -> i64 {
    time::from_db(v.as_str().unwrap_or_else(|| panic!("not a time: {v}"))).unwrap().timestamp_millis()
}

fn range(t0: i64) -> String {
    let q = |ms: i64| time::to_db(&time::from_unix_ms(ms)).replace('+', "%2B");
    format!("from={}&to={}", q(t0 - HOUR), q(t0 + 2 * HOUR))
}

#[tokio::test]
async fn stored_and_derived_episodes_agree_on_the_same_track() {
    let (app, t0, n_cues) = two_collars().await;
    let (_, _, eps) = run(t0);
    let outcomes: Vec<&str> = eps.iter().map(|(e, _)| e.outcome.as_str()).collect();
    // The walk has every ending.
    for o in ["turned_back", "crossed", "rest", "boundary_changed"] {
        assert!(outcomes.contains(&o), "{outcomes:?}");
    }

    let a = app.get(&format!("/api/welfare/animals/ani_1/cues?{}", range(t0))).await;
    let b = app.get(&format!("/api/welfare/animals/ani_2/cues?{}", range(t0))).await;
    let ea = a["episodes"].as_array().unwrap();
    let eb = b["episodes"].as_array().unwrap();
    assert_eq!(ea.len(), eps.len());
    assert_eq!(ea.len(), eb.len());
    for (x, y) in ea.iter().zip(eb) {
        for k in ["start", "end", "outcome", "cues", "max_level", "ring"] {
            assert_eq!(x[k], y[k], "{k}: {x} vs {y}");
        }
        assert!((x["min_margin_m"].as_f64().unwrap() - y["min_margin_m"].as_f64().unwrap()).abs() < 0.01, "{x} vs {y}");
        assert!(x.get("derived").is_none() && y["derived"] == true, "{x} {y}");
    }
    // The same ledger: kind, level, tone and how each cue's episode ended.
    let ca = a["cues"].as_array().unwrap();
    let cb = b["cues"].as_array().unwrap();
    assert_eq!((ca.len(), cb.len()), (n_cues, n_cues));
    for (x, y) in ca.iter().zip(cb) {
        for k in ["at", "kind", "level", "tone_ms", "margin_m", "outcome", "boundary_version"] {
            assert_eq!(x[k], y[k], "{k}: {x} vs {y}");
        }
        assert_eq!((x["dur_ms"].as_u64(), y.get("dur_ms")), (Some(300), None), "firmware 0.1 reports no tone length");
    }
    assert!(ca.iter().any(|c| c["kind"] == "outside" && c["outcome"] == "crossed"));
    // The same days and the same standing.
    assert_eq!(a["days"], b["days"]);
    for k in ["status", "since", "streak", "outcomes"] {
        assert_eq!(a["learning"][k], b["learning"][k], "{k}");
    }
    assert_eq!(a["learning"]["status"], "trained", "{}", a["learning"]);
    assert_eq!(a["learning"]["streak"], 6, "turned back six times since the crossing");
    assert_eq!(b["learning"]["derived"], true);

    // Running again changes nothing.
    let pass = welfare::derive(&app.ctx, time::now().timestamp_millis()).await.unwrap();
    assert_eq!(pass.collars, 0);
    assert_eq!(app.count("SELECT COUNT(*) FROM episodes WHERE derived = 1").await, eps.len() as i64);
}

#[tokio::test]
async fn a_rolled_day_reads_the_same_from_parquet() {
    let (app, t0, _) = two_collars().await;
    let path = format!("/api/welfare/animals/ani_2/cues?{}", range(t0));
    let hot = app.get(&path).await;
    // The day file written, rows still in SQLite: counted once.
    app.copy_day_to_parquet("fixes", t0).await;
    let rolled = op_analytics::rollup::rollup(&app.ctx, 0, time::now()).await.unwrap();
    assert!(rolled.iter().any(|d| d.table == "cues" && d.rows > 0), "{rolled:?}");
    assert_eq!(app.count("SELECT COUNT(*) FROM cues").await, 0);
    let cold = app.get(&path).await;
    for k in ["cues", "episodes", "days", "learning"] {
        assert_eq!(hot[k], cold[k], "{k}");
    }
}

/// An episode of `animal` on `collar` starting `start` ms, 4 s long.
async fn episode(app: &App, collar: &str, animal: &str, start: i64, outcome: &str, derived: bool) {
    sqlx::query(
        "INSERT INTO episodes (id, collar_id, herd_id, animal_id, start_t, end_t, start_at, end_at, ring, cues, max_level, min_margin_m, outcome, derived)
         VALUES (?, ?, 'herd_1', ?, ?, ?, ?, ?, 0, 3, 2, 1.5, ?, ?)",
    )
    .bind(op_core::id::new_id("epi"))
    .bind(collar)
    .bind(animal)
    .bind(start)
    .bind(start + 4000)
    .bind(time::to_db(&time::from_unix_ms(start)))
    .bind(time::to_db(&time::from_unix_ms(start + 4000)))
    .bind(outcome)
    .bind(derived as i64)
    .execute(app.ctx.db())
    .await
    .unwrap();
}

#[tokio::test]
async fn trained_after_n_turned_back_in_a_row_reset_by_a_crossing() {
    let app = App::new(4).await;
    let t = time::now().timestamp_millis() - 10 * HOUR;
    // 101: five turned back. 102: four. 103: five, then crossed. 104: none.
    for i in 0..5 {
        episode(&app, "col_1", "ani_1", t + i * MIN, "turned_back", false).await;
        episode(&app, "col_3", "ani_3", t + i * MIN, "turned_back", false).await;
        if i < 4 {
            episode(&app, "col_2", "ani_2", t + i * MIN, "turned_back", false).await;
        }
    }
    episode(&app, "col_2", "ani_2", t + 10 * MIN, "rest", true).await;
    episode(&app, "col_3", "ani_3", t + 30 * MIN, "crossed", false).await;
    let v = app.get("/api/welfare/animals?herd_id=herd_1").await;
    assert_eq!((v["head"].as_u64(), v["trained"].as_u64(), v["learning"].as_u64()), (Some(4), Some(1), Some(2)), "{v}");
    let row = |tag: &str| v["animals"].as_array().unwrap().iter().find(|r| r["tag"] == tag).cloned().unwrap();
    assert_eq!(row("101")["status"], "trained");
    assert_eq!(ms(&row("101")["since"]), t + 4 * MIN + 4000, "the fifth turned back made it");
    assert_eq!((row("102")["status"].as_str(), row("102")["streak"].as_u64()), (Some("learning"), Some(4)));
    assert_eq!(row("102")["derived"], true);
    assert_eq!((row("103")["status"].as_str(), ms(&row("103")["since"])), (Some("learning"), t + 30 * MIN));
    assert!(row("104").get("status").is_none(), "nothing without episodes: {}", row("104"));
    assert_eq!(v["training"]["trained_after"], 5);

    // Training asks for four: 102 is trained too.
    let (s, tr) = app.call("PUT", "/api/welfare/training/herd_1", Some(json!({ "trained_after": 4 }))).await;
    assert_eq!(s, 200, "{tr}");
    let v = app.get("/api/welfare/animals?herd_id=herd_1").await;
    assert_eq!(v["trained"], 2);
    // Every herd at once.
    assert_eq!(app.get("/api/welfare/animals").await["animals"].as_array().unwrap().len(), 4);
    let (s, _) = app.call("GET", "/api/welfare/animals?herd_id=herd_nope", None).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn training_mode_reads_saves_and_refuses_what_it_cant_be() {
    let app = App::new(1).await;
    let v = app.get("/api/welfare/training/herd_1").await;
    assert_eq!(v, json!({ "herd_id": "herd_1", "enabled": false, "warn_m": 10.0, "trained_after": 5 }));
    let (s, v) = app.call("PUT", "/api/welfare/training/herd_1", Some(json!({ "enabled": true, "warn_m": 12.0 }))).await;
    assert_eq!((s.as_u16(), v["enabled"].as_bool(), v["warn_m"].as_f64(), v["trained_after"].as_u64()), (200, Some(true), Some(12.0), Some(5)));
    // Another herd's change keeps this one's.
    let (s, _) = app.call("PUT", "/api/welfare/training/herd_2", Some(json!({ "trained_after": 3 }))).await;
    assert_eq!(s, 200);
    let stored = app.ctx.store().get_setting_json(welfare::TRAINING_KEY).await.unwrap().unwrap();
    assert_eq!(stored["herd_1"], json!({ "enabled": true, "warn_m": 12.0, "trained_after": 5 }));
    assert_eq!(stored["herd_2"]["trained_after"], 3);
    for bad in [json!({ "warn_m": 0.5 }), json!({ "warn_m": 250.0 }), json!({ "trained_after": 0 }), json!({ "trained_after": 51 })] {
        let (s, v) = app.call("PUT", "/api/welfare/training/herd_1", Some(bad.clone())).await;
        assert_eq!(s, 400, "{bad}: {v}");
    }
    // The farm is imperial (America/Chicago): the refusal speaks in feet.
    app.ctx.update_settings(&json!({"units": "imperial"})).await.unwrap();
    let (s, v) = app.call("PUT", "/api/welfare/training/herd_1", Some(json!({ "warn_m": 0.2 }))).await;
    assert_eq!(s, 400);
    assert!(v["error"].as_str().unwrap().starts_with("The warning zone must be"), "{v}");
    let (s, _) = app.call("PUT", "/api/welfare/training/herd_1", Some(json!({ "loud": true }))).await;
    assert!(s.is_client_error());
    let (s, _) = app.call("GET", "/api/welfare/training/herd_nope", None).await;
    assert_eq!(s, 404);
    let (s, _) = app.call("PUT", "/api/welfare/training/herd_nope", Some(json!({ "enabled": true }))).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn cue_ticks_where_they_fired() {
    let (app, t0, n) = two_collars().await;
    let v = app.get(&format!("/api/welfare/cues/points?herd_id=herd_1&{}", range(t0))).await;
    let ticks = v["ticks"].as_array().unwrap();
    assert!(!ticks.is_empty());
    let (w, o): (f64, f64) = ticks.iter().fold((0.0, 0.0), |a, t| (a.0 + t[2].as_f64().unwrap(), a.1 + t[3].as_f64().unwrap()));
    assert_eq!((w + o) as usize, 2 * n, "both collars' cues");
    assert!(o > 0.0 && w > o);
    assert_eq!(v["cell_m"], 2.0);
    let size = v["size"].as_array().unwrap();
    assert!((size[1].as_f64().unwrap() * 111_000.0 - 2.0).abs() < 0.05, "{v}");
    // Cells hold the cues near the edges: every tick is within 10 m of the square's edge.
    let proj = Projection::new(ORIGIN);
    for t in ticks {
        let [x, y] = proj.forward([t[0].as_f64().unwrap(), t[1].as_f64().unwrap()]);
        assert!(y > 90.0 || x > 90.0, "{x} {y}");
    }
    assert!(app.get("/api/welfare/cues/points?herd_id=herd_2").await["ticks"].as_array().unwrap().is_empty());
}

#[tokio::test]
async fn farm_days_fit_checks_and_still_collars_on_the_record() {
    let (app, t0, _) = two_collars().await;
    // A fit check of 102's collar, and a time it lay still.
    sqlx::query("INSERT INTO collar_fit_checks (id, collar_id, checked_at, \"by\", notes) VALUES ('fit_1', 'col_2', ?, '{\"via\":\"local\"}', 'snug')")
        .bind(time::to_db(&time::from_unix_ms(t0 - HOUR)))
        .execute(app.ctx.db())
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO alerts (id, kind, key, severity, status, herd_id, title, targets, opened_at, updated_at, resolved_at, seen_at)
         VALUES ('alr_1', 'drop_off', 'drop_off:col_2', 'warning', 'resolved', 'herd_1', '102 not moving', '[[\"collar\",\"col_2\"],[\"animal\",\"ani_2\"]]', ?, ?, ?, ?)",
    )
    .bind(time::to_db(&time::from_unix_ms(t0 + 30 * MIN)))
    .bind(time::to_db(&time::from_unix_ms(t0 + 90 * MIN)))
    .bind(time::to_db(&time::from_unix_ms(t0 + 90 * MIN)))
    .bind(time::to_db(&time::from_unix_ms(t0 + 90 * MIN)))
    .execute(app.ctx.db())
    .await
    .unwrap();
    let v = app.get(&format!("/api/welfare/animals/ani_2/cues?{}", range(t0))).await;
    assert_eq!(v["fit_checks"][0]["notes"], "snug");
    assert_eq!(v["fit_checks"][0]["by"]["via"], "local");
    let drops = v["drop_offs"].as_array().unwrap();
    assert_eq!(drops.len(), 1, "{v}");
    assert_eq!((ms(&drops[0]["from"]), ms(&drops[0]["to"])), (t0 + 30 * MIN, t0 + 90 * MIN));
    assert!(app.get(&format!("/api/welfare/animals/ani_1/cues?{}", range(t0))).await["drop_offs"].as_array().unwrap().is_empty());
    // Farm days: the walk is on one farm day (07:00 in Chicago), the rest of the range empty.
    let days = v["days"].as_array().unwrap();
    let cued: Vec<&Value> = days.iter().filter(|d| d["warn"].as_u64().unwrap() > 0).collect();
    assert_eq!(cued.len(), 1);
    let d = cued[0];
    let tone = d["tone_s"].as_f64().unwrap();
    let cues = (d["warn"].as_u64().unwrap() + d["outside"].as_u64().unwrap()) as f64;
    assert!((tone - cues * 0.3).abs() < 0.05, "{d}");
    assert!(d["longest_s"].as_f64().unwrap() >= 20.0, "the rest episode ran 20 s: {d}");
    assert_eq!(d["max_level"], 4);
    let (s, _) = app.call("GET", "/api/welfare/animals/ani_nope/cues", None).await;
    assert_eq!(s, 404);
}

#[tokio::test]
async fn get_welfare_through_the_tool_registry() {
    let (app, t0, _) = two_collars().await;
    op_analytics::register_tools(&app.ctx);
    let viewer = Identity { role: Role::Viewer, user_id: None, name: None, via: Via::UserToken };
    let scope = op_core::tools::ToolScope::Full;
    let herd = app.ctx.tools().call(&app.ctx, "get_welfare", json!({ "herd_id": "herd_1" }), None, viewer.clone(), &scope).await.unwrap();
    assert_eq!(herd["trained"], 2);
    let from = time::to_db(&time::from_unix_ms(t0 - HOUR));
    let one = app.ctx.tools().call(&app.ctx, "get_welfare", json!({ "animal_id": "ani_2", "from": from }), None, viewer.clone(), &scope).await.unwrap();
    assert!(one["cues"].as_array().unwrap().len() <= 50);
    assert_eq!(one["truncated"].as_u64().map(|n| n > 0), Some(true));
    assert_eq!(one["learning"]["derived"], true);
    let spec = app.ctx.tools().get("get_welfare").unwrap();
    assert!(spec.read && !spec.brain && spec.min_role == Role::Viewer);
    assert!(app.ctx.tools().call(&app.ctx, "get_welfare", json!({ "loud": 1 }), None, viewer, &scope).await.is_err());
}

#[tokio::test]
async fn the_record_reads_one_animal_or_herd_through_indexes() {
    let app = App::new(1).await;
    let plan = |sql: &'static str| {
        let ctx = app.ctx.clone();
        async move {
            let rows: Vec<(i64, i64, i64, String)> = sqlx::query_as(&format!("EXPLAIN QUERY PLAN {sql}")).fetch_all(ctx.db()).await.unwrap();
            rows.into_iter().map(|r| r.3).collect::<Vec<_>>().join(" / ")
        }
    };
    for (sql, index) in [
        ("SELECT id, t FROM cues WHERE t >= 0 AND t < 9 AND animal_id = 'a'", "cues_animal_t"),
        ("SELECT id, t FROM cues WHERE t >= 0 AND t < 9 AND herd_id = 'h'", "cues_herd_t"),
        ("SELECT id FROM episodes WHERE start_t < 9 AND end_t >= 0 AND animal_id = 'a'", "episodes_animal"),
        (
            "SELECT animal_id, start_t, end_t, outcome, derived FROM episodes WHERE animal_id IN ('a', 'b') ORDER BY animal_id, start_t",
            "COVERING INDEX episodes_animal",
        ),
        ("SELECT t, state, margin_m, boundary_version FROM fixes WHERE collar_id = 'c' AND t >= 0 ORDER BY t", "fixes_collar_t"),
        ("SELECT MIN(start_t) FROM episodes WHERE collar_id = 'c' AND derived = 1 AND end_t >= 0", "episodes_collar_start"),
    ] {
        let p = plan(sql).await;
        assert!(p.contains(index), "{sql}\n{p}");
        assert!(!p.contains("SCAN cues") && !p.contains("SCAN episodes") && !p.contains("SCAN fixes"), "{sql}\n{p}");
    }
}

#[tokio::test]
async fn a_herd_of_250_with_a_season_of_episodes_reads_fast() {
    let app = App::new(250).await;
    // 200 episodes each (50,000): mostly turned back, a crossing now and then.
    let t0 = time::now().timestamp_millis() - 100 * DAY;
    sqlx::query(
        "WITH RECURSIVE k(i) AS (SELECT 0 UNION ALL SELECT i + 1 FROM k WHERE i + 1 < 50000)
         INSERT INTO episodes (id, collar_id, herd_id, animal_id, start_t, end_t, start_at, end_at, ring, cues, max_level, min_margin_m, outcome)
         SELECT 'epi_' || i, 'col_' || (i % 250 + 1), 'herd_1', 'ani_' || (i % 250 + 1), ?1 + i * 60000, ?1 + i * 60000 + 8000, '', '', 0, 3, 2, 1.0,
                CASE WHEN i % 37 = 0 THEN 'crossed' WHEN i % 11 = 0 THEN 'rest' ELSE 'turned_back' END FROM k",
    )
    .bind(t0)
    .execute(app.ctx.db())
    .await
    .unwrap();
    let started = std::time::Instant::now();
    let v = app.get("/api/welfare/animals?herd_id=herd_1").await;
    let took = started.elapsed();
    assert_eq!(v["head"], 250);
    assert_eq!(v["trained"].as_u64().unwrap() + v["learning"].as_u64().unwrap(), 250);
    assert!(took < std::time::Duration::from_secs(2), "{took:?}");
    eprintln!("250 animals, 50,000 episodes: {took:?}");
}
