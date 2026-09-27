//! A farm in a temp dir with rows written straight into the tables the rules
//! read, so each test states the database it evaluates. Shared by the
//! A-engine test files (`rules`, `engine`, `routing`, `text`).

#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, Duration, Utc};
use http_body_util::BodyExt;
use op_alerts::engine::{self, Changes};
use op_alerts::routing;
use op_core::alert::{Alert, MessageLog};
use op_core::time::{from_db, to_db};
use op_core::users::NewUser;
use op_core::{Ctx, Identity, LonLat, Role, Via};
use op_geo::Projection;
use serde_json::{Value, json};
use tower::ServiceExt;

pub const HERD_CENTER: LonLat = [-93.6225, 42.0318];

pub fn t(s: &str) -> DateTime<Utc> {
    from_db(s).unwrap()
}

/// 12:00 UTC = 07:00 on the farm (America/Chicago, daylight time).
pub fn t0() -> DateTime<Utc> {
    t("2026-09-27T12:00:00.000Z")
}

pub fn mins(m: i64) -> Duration {
    Duration::minutes(m)
}

pub fn secs(s: i64) -> Duration {
    Duration::seconds(s)
}

pub struct Farm {
    pub dir: tempfile::TempDir,
    pub ctx: Ctx,
    pub herd: String,
    /// P1, P2, P3 (the §6 paddocks around Ames).
    pub paddocks: Vec<String>,
    n: std::sync::atomic::AtomicU32,
}

fn rect(w: f64, s: f64, e: f64, n: f64) -> Value {
    json!({"type": "Polygon", "coordinates": [[[w, s], [e, s], [e, n], [w, n], [w, s]]]})
}

impl Farm {
    pub async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let ctx = Ctx::open(dir.path()).await.unwrap();
        op_alerts::register_tools(&ctx);
        let f = Farm { dir, ctx, herd: String::new(), paddocks: vec![], n: Default::default() };
        let (s, _) = f.core("POST", "/api/farm", Some(json!({"name": "Test farm", "timezone": "America/Chicago", "center": [-93.62, 42.03]}))).await;
        assert_eq!(s, StatusCode::CREATED);
        let mut paddocks = vec![];
        for (name, g) in
            [("P1", rect(-93.625, 42.03, -93.62, 42.0336)), ("P2", rect(-93.62, 42.03, -93.615, 42.0336)), ("P3", rect(-93.625, 42.0336, -93.62, 42.0372))]
        {
            let (_, p) = f.core("POST", "/api/paddocks", Some(json!({"name": name, "geometry": g}))).await;
            paddocks.push(p["id"].as_str().unwrap().to_owned());
        }
        let (_, h) = f.core("POST", "/api/herds", Some(json!({"name": "Cows", "species": "cattle", "count": 250, "paddock_id": paddocks[0]}))).await;
        let herd = h["id"].as_str().unwrap().to_owned();
        Farm { herd, paddocks, ..f }
    }

    /// op-core routes as the local owner.
    pub async fn core(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let router = op_core::with_identity(op_core::router(), Identity::owner(Via::Local)).with_state(self.ctx.clone());
        call(router, method, path, body).await
    }

    /// op-alerts routes as `id`.
    pub async fn api(&self, id: Identity, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        let router = op_core::with_identity(op_alerts::router(), id).with_state(self.ctx.clone());
        call(router, method, path, body).await
    }

    pub async fn owner(&self, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
        self.api(Identity::owner(Via::Local), method, path, body).await
    }

    fn next(&self) -> u32 {
        self.n.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1
    }

    pub async fn exec(&self, sql: &str) {
        sqlx::query(sql).execute(self.ctx.db()).await.unwrap();
    }

    /// A collar (with an animal tagged `tag` when given) in the herd, fixed
    /// at `at`, inside, last seen at `seen`.
    pub async fn collar(&self, tag: Option<&str>, seen: DateTime<Utc>) -> String {
        self.collar_in(&self.herd.clone(), tag, seen).await
    }

    pub async fn collar_in(&self, herd: &str, tag: Option<&str>, seen: DateTime<Utc>) -> String {
        let n = self.next();
        let id = format!("col_{n:04}");
        let point = self.spot(n as f64 * 3.0, 20.0);
        let fix = json!({"at": to_db(&seen), "point": point, "accuracy_m": 3.0, "sats": 9});
        sqlx::query("INSERT INTO collars (id, name, herd_id, state, last_seen, battery, last_fix, created_at) VALUES (?, ?, ?, 'inside', ?, 0.8, ?, ?)")
            .bind(&id)
            .bind(format!("C{n}"))
            .bind(herd)
            .bind(to_db(&seen))
            .bind(fix.to_string())
            // Added a day ago, so its first fit check isn't due (H's fit_check_due).
            .bind(to_db(&(seen - Duration::days(1))))
            .execute(self.ctx.db())
            .await
            .unwrap();
        if let Some(tag) = tag {
            let aid = format!("ani_{n:04}");
            sqlx::query("INSERT INTO animals (id, tag, herd_id, collar_id, created_at) VALUES (?, ?, ?, ?, ?)")
                .bind(&aid)
                .bind(tag)
                .bind(herd)
                .bind(&id)
                .bind(to_db(&seen))
                .execute(self.ctx.db())
                .await
                .unwrap();
            sqlx::query("UPDATE collars SET animal_id = ? WHERE id = ?").bind(&aid).bind(&id).execute(self.ctx.db()).await.unwrap();
        }
        id
    }

    /// `n` collars tagged 101, 102, …
    pub async fn collars(&self, n: usize, seen: DateTime<Utc>) -> Vec<String> {
        let mut out = vec![];
        for i in 0..n {
            out.push(self.collar(Some(&format!("{}", 101 + i)), seen).await);
        }
        out
    }

    /// A point `east`/`north` metres from P1's south-west corner.
    pub fn spot(&self, east: f64, north: f64) -> LonLat {
        Projection::new([-93.625, 42.03]).offset(east, north)
    }

    pub async fn set(&self, collar: &str, sets: &str, binds: &[&str]) {
        let sql = format!("UPDATE collars SET {sets} WHERE id = ?");
        let mut q = sqlx::query(&sql);
        for b in binds {
            q = q.bind(*b);
        }
        q.bind(collar).execute(self.ctx.db()).await.unwrap();
    }

    /// The collar has been outside since `since` (last fix there too).
    pub async fn outside(&self, collar: &str, since: DateTime<Utc>, seen: DateTime<Utc>) {
        let fix = json!({"at": to_db(&seen), "point": self.spot(200.0, -60.0), "accuracy_m": 3.0, "sats": 9});
        self.set(collar, "state = 'outside', outside_since = ?, last_seen = ?, last_fix = ?", &[&to_db(&since), &to_db(&seen), &fix.to_string()]).await;
    }

    pub async fn inside(&self, collar: &str, seen: DateTime<Utc>) {
        self.set(collar, "state = 'inside', outside_since = NULL, last_seen = ?", &[&to_db(&seen)]).await;
    }

    pub async fn seen(&self, collar: &str, seen: DateTime<Utc>) {
        self.set(collar, "last_seen = ?", &[&to_db(&seen)]).await;
    }

    /// Health rows (one per report) at these times.
    pub async fn reports(&self, collar: &str, times: &[DateTime<Utc>]) {
        for at in times {
            sqlx::query("INSERT INTO health (collar_id, herd_id, at, t, battery) VALUES (?, ?, ?, ?, 0.8)")
                .bind(collar)
                .bind(&self.herd)
                .bind(to_db(at))
                .bind(at.timestamp_millis())
                .execute(self.ctx.db())
                .await
                .unwrap();
        }
    }

    pub async fn fix(&self, collar: &str, at: DateTime<Utc>, point: LonLat, accuracy_m: f64) {
        sqlx::query("INSERT INTO fixes (collar_id, herd_id, at, t, lon, lat, accuracy_m, sats) VALUES (?, ?, ?, ?, ?, ?, ?, 9)")
            .bind(collar)
            .bind(&self.herd)
            .bind(to_db(&at))
            .bind(at.timestamp_millis())
            .bind(point[0])
            .bind(point[1])
            .bind(accuracy_m)
            .execute(self.ctx.db())
            .await
            .unwrap();
    }

    pub async fn escape(&self, collar: &str, status: &str, started: DateTime<Utc>, ended: Option<DateTime<Utc>>) -> String {
        let id = format!("esc_{:04}", self.next());
        sqlx::query("INSERT INTO escapes (id, herd_id, collar_id, status, step, remaining_m, pen, started_at, updated_at, ended_at) VALUES (?, ?, ?, ?, 1, 20, '{}', ?, ?, ?)")
            .bind(&id)
            .bind(&self.herd)
            .bind(collar)
            .bind(status)
            .bind(to_db(&started))
            .bind(to_db(&ended.unwrap_or(started)))
            .bind(ended.as_ref().map(to_db))
            .execute(self.ctx.db())
            .await
            .unwrap();
        id
    }

    /// A herd boundary (P1's shape) of `version`.
    pub async fn boundary(&self, version: u32, created: DateTime<Utc>, effective: Option<DateTime<Utc>>) {
        sqlx::query(
            "INSERT INTO boundaries (id, herd_id, version, geometry, warn_m, hysteresis_m, effective_at, decision_id, created_at) VALUES (?, ?, ?, ?, 5, 1, ?, 'dec_x', ?)",
        )
        .bind(format!("bnd_{:04}", self.next()))
        .bind(&self.herd)
        .bind(version as i64)
        .bind(rect(-93.625, 42.03, -93.62, 42.0336).to_string())
        .bind(effective.as_ref().map(to_db))
        .bind(to_db(&created))
        .execute(self.ctx.db())
        .await
        .unwrap();
    }

    pub async fn holds(&self, collar: &str, version: u32, status: &str) {
        sqlx::query(
            "INSERT INTO collar_boundary_state (collar_id, herd_id, version, status, command_id, at) VALUES (?, ?, ?, ?, 'bnd_x', ?)
             ON CONFLICT(collar_id) DO UPDATE SET version = excluded.version, status = excluded.status",
        )
        .bind(collar)
        .bind(&self.herd)
        .bind(version as i64)
        .bind(status)
        .bind(to_db(&t0()))
        .execute(self.ctx.db())
        .await
        .unwrap();
    }

    /// A decision; `action` MOVE (to P2) or STAY.
    pub async fn decision(&self, action: &str, status: &str, created: DateTime<Utc>, apply_at: Option<DateTime<Utc>>) -> String {
        let id = format!("dec_{:04}", self.next());
        let (to, geometry, inputs) = if action == "MOVE" {
            let p2 = &self.paddocks[1];
            (
                Some(p2.clone()),
                Some(rect(-93.62, 42.03, -93.615, 42.0336).to_string()),
                json!({"signals": {"herd_animal_units": 250.0, "forage": {p2: {"available_kg_dm_per_ha": 1500.0}}}}),
            )
        } else {
            (None, None, json!({}))
        };
        sqlx::query(
            "INSERT INTO decisions (id, herd_id, source, status, action, to_paddock_id, geometry, inputs, apply_at, created_at) VALUES (?, ?, 'heuristic', ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&self.herd)
        .bind(status)
        .bind(action)
        .bind(to)
        .bind(geometry)
        .bind(inputs.to_string())
        .bind(apply_at.as_ref().map(to_db))
        .bind(to_db(&created))
        .execute(self.ctx.db())
        .await
        .unwrap();
        id
    }

    pub async fn decision_status(&self, id: &str, status: &str) {
        sqlx::query("UPDATE decisions SET status = ? WHERE id = ?").bind(status).bind(id).execute(self.ctx.db()).await.unwrap();
    }

    /// A move to P2 for the herd.
    pub async fn movement(&self, status: &str, started: DateTime<Utc>, last_step: Option<DateTime<Utc>>, stragglers: &[&str]) -> String {
        let id = format!("mov_{:04}", self.next());
        let dec = self.decision("MOVE", "applied", started, None).await;
        let sweep = match last_step {
            Some(t) => json!({"last_step_at": to_db(&t)}),
            None => json!({}),
        };
        sqlx::query(
            "INSERT INTO moves (id, herd_id, decision_id, target, status, step, remaining_m, stragglers, warn_m, hysteresis_m, sweep, started_at, updated_at)
             VALUES (?, ?, ?, ?, ?, 3, 50, ?, 5, 1, ?, ?, ?)",
        )
        .bind(&id)
        .bind(&self.herd)
        .bind(dec)
        .bind(rect(-93.62, 42.03, -93.615, 42.0336).to_string())
        .bind(status)
        .bind(json!(stragglers).to_string())
        .bind(sweep.to_string())
        .bind(to_db(&started))
        .bind(to_db(&last_step.unwrap_or(started)))
        .execute(self.ctx.db())
        .await
        .unwrap();
        id
    }

    /// A person; `verified` marks the phone verified.
    pub async fn person(&self, name: &str, role: Role, phone: Option<&str>, verified: bool, email: Option<&str>) -> String {
        let u = op_core::users::create_user(&self.ctx, NewUser { name: name.into(), role, phone: phone.map(Into::into), email: email.map(Into::into) })
            .await
            .unwrap();
        if verified {
            op_core::users::set_phone_verified(&self.ctx, &u.id, t0() - Duration::days(1)).await.unwrap();
        }
        u.id
    }

    pub async fn prefs(&self, user: &str, patch: Value) {
        op_alerts::routing::prefs::put(&self.ctx, user, &patch).await.unwrap();
    }

    /// Twilio SMS (and optionally more) set up, as A-notify stores it.
    pub async fn channels(&self, cfg: Value) {
        self.ctx.store().set_setting_json(op_core::notify_config::CHANNELS_KEY, &cfg).await.unwrap();
        self.ctx.secrets().set("twilio_account_sid", "ACtest").unwrap();
        self.ctx.secrets().set("twilio_auth_token", "secret").unwrap();
        self.ctx.secrets().set("webhook_secret", "whsec").unwrap();
    }

    pub async fn sms(&self) {
        self.channels(json!({"sms": {"from": "+15155550100"}})).await;
    }

    pub async fn policy(&self, p: Value) {
        let (s, v) = self.owner("PUT", "/api/alerts/rules", Some(json!({"policy": p}))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    pub async fn rule(&self, kind: &str, cfg: Value) {
        let (s, v) = self.owner("PUT", "/api/alerts/rules", Some(json!({"rules": {kind: cfg}}))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }

    pub async fn eval(&self, now: DateTime<Utc>) -> Changes {
        engine::evaluate_all(&self.ctx, now).await.unwrap()
    }

    pub async fn route(&self, now: DateTime<Utc>) -> Vec<MessageLog> {
        routing::route(&self.ctx, now).await.unwrap().messages
    }

    /// Unresolved alerts.
    pub async fn open(&self) -> Vec<Alert> {
        let rows = sqlx::query("SELECT * FROM alerts WHERE status != 'resolved' ORDER BY opened_at, key").fetch_all(self.ctx.db()).await.unwrap();
        rows.iter().map(|r| op_alerts::engine::store::alert_from_row(r).unwrap()).collect()
    }

    pub async fn open_kind(&self, kind: &str) -> Vec<Alert> {
        self.open().await.into_iter().filter(|a| a.kind == kind).collect()
    }

    pub async fn every_alert(&self) -> Vec<Alert> {
        let rows = sqlx::query("SELECT * FROM alerts ORDER BY opened_at, key").fetch_all(self.ctx.db()).await.unwrap();
        rows.iter().map(|r| op_alerts::engine::store::alert_from_row(r).unwrap()).collect()
    }

    pub async fn alert(&self, id: &str) -> Alert {
        op_alerts::engine::store::get(&self.ctx, id).await.unwrap().unwrap()
    }

    /// Every message queued so far, oldest first.
    pub async fn messages(&self) -> Vec<MessageLog> {
        let rows = sqlx::query("SELECT * FROM messages WHERE direction = 'out' ORDER BY created_at, rowid").fetch_all(self.ctx.db()).await.unwrap();
        rows.iter().map(|r| op_core::messages::message_from_row(r).unwrap()).collect()
    }

    pub async fn messages_to(&self, user: &str) -> Vec<MessageLog> {
        self.messages().await.into_iter().filter(|m| m.user_id.as_deref() == Some(user)).collect()
    }
}

pub async fn call(router: axum::Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    let mut req = Request::builder().method(method).uri(path);
    let body = match body {
        Some(b) => {
            req = req.header("content-type", "application/json");
            Body::from(b.to_string())
        }
        None => Body::empty(),
    };
    let res = router.oneshot(req.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

pub fn person_identity(role: Role, user_id: &str) -> Identity {
    Identity { role, user_id: Some(user_id.to_owned()), name: None, via: Via::UserToken }
}
