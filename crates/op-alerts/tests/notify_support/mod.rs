//! In-test stand-ins for the outside world only: a Twilio-shaped HTTP server,
//! a plain HTTP receiver for webhooks, an SMTP sink, and a hosting server
//! (a second openpasture data dir serving op-alerts' routes). Product code
//! runs unchanged against them.

#![allow(dead_code)]

use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use http_body_util::BodyExt;
use op_core::Ctx;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tower::ServiceExt;

pub const SID: &str = "AC0123456789abcdef0123456789abcdef";
pub const TOKEN: &str = "twilio-test-token";
pub const FROM: &str = "+15155550100";

pub async fn ctx() -> (tempfile::TempDir, Ctx) {
    let dir = tempfile::tempdir().unwrap();
    let ctx = Ctx::open(dir.path()).await.unwrap();
    (dir, ctx)
}

/// op-alerts' routes for `ctx`, as the server mounts them.
pub fn app(ctx: &Ctx) -> Router {
    op_alerts::router().with_state(ctx.clone())
}

pub async fn call(app: &Router, method: &str, path: &str, body: Option<Value>) -> (StatusCode, Value) {
    call_with(app, method, path, body, &[]).await
}

pub async fn call_with(app: &Router, method: &str, path: &str, body: Option<Value>, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut b = axum::http::Request::builder().method(method).uri(path);
    for (k, v) in headers {
        b = b.header(*k, *v);
    }
    let body = match body {
        Some(v) => {
            b = b.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    let res = app.clone().oneshot(b.body(body).unwrap()).await.unwrap();
    let status = res.status();
    let bytes = res.into_body().collect().await.unwrap().to_bytes();
    (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

/// Serve `app` on a free loopback port; returns its base URL.
pub async fn serve(app: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr: SocketAddr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}")
}

// ---- HTTP receiver ------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct Hit {
    pub method: String,
    pub path: String,
    pub headers: HeaderMap,
    pub body: Bytes,
}

impl Hit {
    /// The body as a form (Twilio's requests).
    pub fn form(&self) -> HashMap<String, String> {
        form_urlencoded::parse(&self.body).into_owned().collect()
    }
    pub fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }
    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_owned)
    }
}

/// Records every request and answers from a queue of replies, else in the
/// shape of Twilio's API (a new `SM…` for a message, the status set for it
/// when read back, an active account).
#[derive(Clone, Default)]
pub struct Receiver {
    pub url: String,
    hits: Arc<Mutex<Vec<Hit>>>,
    replies: Arc<Mutex<VecDeque<(u16, Value)>>>,
    statuses: Arc<Mutex<HashMap<String, Value>>>,
    delay_ms: Arc<Mutex<u64>>,
}

impl Receiver {
    pub async fn start() -> Self {
        let mut r = Receiver::default();
        let app = Router::new().fallback(receive).with_state(r.clone());
        r.url = serve(app).await;
        r
    }
    pub fn hits(&self) -> Vec<Hit> {
        self.hits.lock().unwrap().clone()
    }
    /// POSTs that created a Twilio message.
    pub fn texts(&self) -> Vec<Hit> {
        self.hits().into_iter().filter(|h| h.method == "POST" && h.path.ends_with("/Messages.json")).collect()
    }
    pub fn reply(&self, status: u16, body: Value) {
        self.replies.lock().unwrap().push_back((status, body));
    }
    /// What `GET …/Messages/{sid}.json` answers for `sid`.
    pub fn set_status(&self, sid: &str, body: Value) {
        self.statuses.lock().unwrap().insert(sid.to_owned(), body);
    }
    pub fn delay(&self, ms: u64) {
        *self.delay_ms.lock().unwrap() = ms;
    }
}

async fn receive(State(r): State<Receiver>, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let body = body.collect().await.unwrap().to_bytes();
    let hit = Hit { method: parts.method.to_string(), path: parts.uri.path().to_owned(), headers: parts.headers, body };
    let n = {
        let mut hits = r.hits.lock().unwrap();
        hits.push(hit.clone());
        hits.len()
    };
    let delay = *r.delay_ms.lock().unwrap();
    if delay > 0 {
        tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
    }
    if let Some((status, v)) = r.replies.lock().unwrap().pop_front() {
        return (StatusCode::from_u16(status).unwrap(), axum::Json(v)).into_response();
    }
    if hit.method == "POST" && hit.path.ends_with("/Messages.json") {
        return (StatusCode::CREATED, axum::Json(json!({ "sid": format!("SM{n:032}"), "status": "queued" }))).into_response();
    }
    if hit.method == "GET"
        && let Some(sid) = hit.path.rsplit('/').next().and_then(|f| f.strip_suffix(".json"))
    {
        if let Some(v) = r.statuses.lock().unwrap().get(sid) {
            return (StatusCode::OK, axum::Json(v.clone())).into_response();
        }
        if sid.starts_with("SM") {
            return (StatusCode::OK, axum::Json(json!({ "sid": sid, "status": "sent" }))).into_response();
        }
        if sid.starts_with("AC") {
            return (StatusCode::OK, axum::Json(json!({ "sid": sid, "status": "active" }))).into_response();
        }
    }
    (StatusCode::OK, axum::Json(json!({}))).into_response()
}

/// Point `ctx`'s SMS (and WhatsApp, when given) at `twilio`.
pub async fn setup_twilio(ctx: &Ctx, twilio: &Receiver, whatsapp: Option<Value>) {
    let mut body = json!({
        "sms": { "from": FROM },
        "twilio_api_base": twilio.url,
        "secrets": { "twilio_account_sid": SID, "twilio_auth_token": TOKEN },
    });
    if let Some(w) = whatsapp {
        body["whatsapp"] = w;
    }
    let (s, v) = call(&app(ctx), "PUT", "/api/notify/channels", Some(body)).await;
    assert_eq!(s, StatusCode::OK, "{v}");
}

// ---- SMTP sink ----------------------------------------------------------------------

#[derive(Debug, Clone, Default)]
pub struct Mail {
    pub auth: Option<String>,
    pub from: String,
    pub to: Vec<String>,
    pub data: String,
}

/// A minimal SMTP server: EHLO, AUTH PLAIN/LOGIN, MAIL, RCPT, DATA, QUIT.
/// `rcpt_reply` sets what RCPT TO answers (default `250 ok`).
#[derive(Clone, Default)]
pub struct SmtpSink {
    pub port: u16,
    mails: Arc<Mutex<Vec<Mail>>>,
    rcpt_reply: Arc<Mutex<Option<String>>>,
}

impl SmtpSink {
    pub async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let sink = SmtpSink { port: listener.local_addr().unwrap().port(), ..Default::default() };
        let s = sink.clone();
        tokio::spawn(async move {
            loop {
                let Ok((sock, _)) = listener.accept().await else { break };
                tokio::spawn(smtp_session(sock, s.clone()));
            }
        });
        sink
    }
    pub fn mails(&self) -> Vec<Mail> {
        self.mails.lock().unwrap().clone()
    }
    pub fn refuse_rcpt(&self, reply: &str) {
        *self.rcpt_reply.lock().unwrap() = Some(reply.to_owned());
    }
}

async fn smtp_session(sock: tokio::net::TcpStream, sink: SmtpSink) {
    let (rd, mut wr) = sock.into_split();
    let mut rd = BufReader::new(rd);
    let mut mail = Mail::default();
    let _ = wr.write_all(b"220 sink ESMTP ready\r\n").await;
    let mut line = String::new();
    loop {
        line.clear();
        if rd.read_line(&mut line).await.unwrap_or(0) == 0 {
            break;
        }
        let cmd = line.trim_end().to_owned();
        let upper = cmd.to_ascii_uppercase();
        let reply: String = if upper.starts_with("EHLO") || upper.starts_with("HELO") {
            "250-sink\r\n250-AUTH PLAIN LOGIN\r\n250 8BITMIME\r\n".into()
        } else if upper.starts_with("AUTH PLAIN") {
            mail.auth = Some(cmd[10..].trim().to_owned());
            "235 2.7.0 ok\r\n".into()
        } else if upper.starts_with("AUTH LOGIN") {
            let _ = wr.write_all(b"334 VXNlcm5hbWU6\r\n").await;
            let mut u = String::new();
            let _ = rd.read_line(&mut u).await;
            let _ = wr.write_all(b"334 UGFzc3dvcmQ6\r\n").await;
            let mut p = String::new();
            let _ = rd.read_line(&mut p).await;
            mail.auth = Some(format!("{}:{}", u.trim(), p.trim()));
            "235 2.7.0 ok\r\n".into()
        } else if upper.starts_with("MAIL FROM:") {
            mail.from = cmd[10..].trim().to_owned();
            "250 ok\r\n".into()
        } else if upper.starts_with("RCPT TO:") {
            let refused = sink.rcpt_reply.lock().unwrap().clone();
            match refused {
                Some(r) => format!("{r}\r\n"),
                None => {
                    mail.to.push(cmd[8..].trim().to_owned());
                    "250 ok\r\n".into()
                }
            }
        } else if upper == "DATA" {
            let _ = wr.write_all(b"354 go ahead\r\n").await;
            let mut data = String::new();
            loop {
                let mut l = String::new();
                if rd.read_line(&mut l).await.unwrap_or(0) == 0 || l == ".\r\n" {
                    break;
                }
                data.push_str(&l);
            }
            mail.data = data;
            sink.mails.lock().unwrap().push(std::mem::take(&mut mail));
            "250 2.0.0 Ok: queued as 4F1C2\r\n".into()
        } else if upper == "QUIT" {
            let _ = wr.write_all(b"221 bye\r\n").await;
            break;
        } else {
            "250 ok\r\n".into()
        };
        if wr.write_all(reply.as_bytes()).await.is_err() {
            break;
        }
    }
}

// ---- hosting server -----------------------------------------------------------------

/// A second openpasture server relaying for others: its own data dir, its own
/// Twilio (a [`Receiver`]), hosting on, and one `oph_` key.
pub struct Host {
    pub _dir: tempfile::TempDir,
    pub ctx: Ctx,
    pub url: String,
    pub twilio: Receiver,
    pub key: String,
    pub key_id: String,
}

impl Host {
    pub async fn start() -> Self {
        let (dir, ctx) = ctx().await;
        let twilio = Receiver::start().await;
        setup_twilio(&ctx, &twilio, None).await;
        let (s, v) = call(&app(&ctx), "PUT", "/api/notify/hosting", Some(json!({ "enabled": true }))).await;
        assert_eq!(s, StatusCode::OK, "{v}");
        let (info, key) = op_brain::hosted::create_key(&ctx, "farm").await.unwrap();
        let url = serve(op_alerts::router().with_state(ctx.clone())).await;
        Host { _dir: dir, ctx, url, twilio, key, key_id: info.id }
    }

    pub fn bearer(&self) -> String {
        format!("Bearer {}", self.key)
    }

    /// Prove `to` for this host's key the way a farm does, reading the code
    /// off the host's Twilio.
    pub async fn verify_recipient(&self, channel: &str, to: &str) {
        let app = app(&self.ctx);
        let auth = self.bearer();
        let (s, v) = call_with(&app, "POST", "/v1/notify/recipients", Some(json!({ "channel": channel, "to": to })), &[("authorization", &auth)]).await;
        assert_eq!(s, StatusCode::ACCEPTED, "{v}");
        let code = last_code(&self.twilio);
        let (s, v) =
            call_with(&app, "POST", "/v1/notify/recipients/verify", Some(json!({ "channel": channel, "to": to, "code": code })), &[("authorization", &auth)])
                .await;
        assert_eq!(s, StatusCode::OK, "{v}");
    }
}

/// The 6-digit code in the last text `twilio` took.
pub fn last_code(twilio: &Receiver) -> String {
    let body = twilio.texts().last().expect("a text").form()["Body"].clone();
    let code: String = body.chars().filter(char::is_ascii_digit).collect();
    assert_eq!(code.len(), 6, "{body}");
    code
}

/// Every message row, oldest first.
pub async fn messages(ctx: &Ctx) -> Vec<op_core::alert::MessageLog> {
    let rows = sqlx::query("SELECT * FROM messages ORDER BY created_at, rowid").fetch_all(ctx.db()).await.unwrap();
    rows.iter().map(|r| op_core::messages::message_from_row(r).unwrap()).collect()
}

pub async fn message(ctx: &Ctx, id: &str) -> op_core::alert::MessageLog {
    op_core::messages::get_message(ctx, id).await.unwrap().unwrap()
}

pub async fn next_attempt_at(ctx: &Ctx, id: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    let (t,): (Option<String>,) = sqlx::query_as("SELECT next_attempt_at FROM messages WHERE id = ?").bind(id).fetch_one(ctx.db()).await.unwrap();
    op_core::time::opt_from_db(t).unwrap()
}

pub fn out(key: &str, channel: &str, to: &str, text: &str) -> op_core::messages::Outbound {
    op_core::messages::Outbound {
        idempotency_key: key.into(),
        channel: channel.into(),
        to: to.into(),
        text: text.into(),
        kind: "alert".into(),
        ..Default::default()
    }
}
