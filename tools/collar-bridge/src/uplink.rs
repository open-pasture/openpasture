//! The collar's side of `/collar/v1`, spoken by the bridge on its behalf
//! with the collar's key. Every request and reply goes to the capture.

use std::time::{Duration, Instant};

use chrono::Utc;
use op_protocol::{Ack, ConfigCommand, PositionReport};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::capture::{Capture, Record};

/// How a request went.
#[derive(Debug)]
pub enum Sent<T> {
    Ok(T),
    /// 401: the server doesn't know this key.
    Unlinked(String),
    /// Another 4xx: the server will never take it, so it isn't resent.
    Refused {
        status: u16,
        message: String,
    },
    /// 5xx or no answer: worth trying again.
    Down(String),
}

/// A report reply, with the config exactly as signed.
#[derive(Debug, Clone)]
pub struct ReportReply {
    pub latest_version: Option<u32>,
    pub config: Option<(String, ConfigCommand)>,
}

/// A downloaded boundary command, exactly as signed.
#[derive(Debug, Clone)]
pub struct Download {
    /// One line: the server's bytes with any line breaks turned to spaces
    /// (whitespace outside strings isn't signed).
    pub wire: String,
    pub command_id: String,
    pub version: u32,
}

pub struct Uplink {
    http: reqwest::Client,
    base: String,
    key: String,
    pub capture: std::sync::Arc<Capture>,
}

/// `http://host:7878`, `…/collar/v1` or `…/collar/v1/` all mean the same server.
pub fn device_base(server: &str) -> String {
    let s = server.trim_end_matches('/');
    let s = s.strip_suffix("/collar/v1").unwrap_or(s);
    format!("{s}/collar/v1")
}

/// The error text a server gave (`{"error": …}`), else the body.
pub fn error_text(body: &str) -> String {
    #[derive(Deserialize)]
    struct E {
        error: String,
    }
    serde_json::from_str::<E>(body).map(|e| e.error).unwrap_or_else(|_| body.chars().take(300).collect())
}

impl Uplink {
    pub fn new(server: &str, key: &str, capture: std::sync::Arc<Capture>) -> Self {
        let http = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().unwrap_or_default();
        Self { http, base: device_base(server), key: key.to_owned(), capture }
    }

    /// One request; the raw status and body, captured either way.
    async fn send(&self, method: reqwest::Method, path: &str, body: Option<String>) -> Result<(u16, String), String> {
        let started = Instant::now();
        let mut req = self.http.request(method.clone(), format!("{}{path}", self.base)).bearer_auth(&self.key);
        if let Some(b) = &body {
            req = req.header(reqwest::header::CONTENT_TYPE, "application/json").body(b.clone());
        }
        let res = match req.send().await {
            Ok(r) => {
                let status = r.status().as_u16();
                r.text().await.map(|t| (status, t)).map_err(|e| e.to_string())
            }
            Err(e) => Err(e.to_string()),
        };
        let (status, reply, error) = match &res {
            Ok((s, t)) => (Some(*s), Some(t.clone()), None),
            Err(e) => (None, None, Some(e.clone())),
        };
        self.capture.write(&Record::Http {
            t: Utc::now(),
            method: method.to_string(),
            path: format!("/collar/v1{path}"),
            body,
            status,
            reply,
            error,
            ms: started.elapsed().as_millis() as u64,
        });
        res
    }

    fn sorted<T>(res: Result<(u16, String), String>, ok: impl FnOnce(u16, String) -> Result<T, String>) -> Sent<T> {
        match res {
            Err(e) => Sent::Down(e),
            Ok((401, body)) => Sent::Unlinked(error_text(&body)),
            Ok((s, body)) if (200..300).contains(&s) => match ok(s, body) {
                Ok(v) => Sent::Ok(v),
                Err(e) => Sent::Refused { status: s, message: e },
            },
            Ok((s, body)) if (400..500).contains(&s) => Sent::Refused { status: s, message: error_text(&body) },
            Ok((s, body)) => Sent::Down(format!("{s}: {}", error_text(&body))),
        }
    }

    pub async fn report(&self, rep: &PositionReport) -> Sent<ReportReply> {
        let body = match serde_json::to_string(rep) {
            Ok(b) => b,
            Err(e) => return Sent::Refused { status: 0, message: e.to_string() },
        };
        let res = self.send(reqwest::Method::POST, "/report", Some(body)).await;
        Self::sorted(res, |_, text| parse_report_reply(&text))
    }

    /// `None`: the collar holds everything it should (204).
    pub async fn boundary(&self, have: u32, free: Option<usize>) -> Sent<Option<Download>> {
        let mut path = format!("/boundary?have={have}");
        if let Some(f) = free {
            path.push_str(&format!("&free={f}"));
        }
        let res = self.send(reqwest::Method::GET, &path, None).await;
        Self::sorted(res, |status, text| if status == 204 { Ok(None) } else { parse_download(&text).map(Some) })
    }

    pub async fn ack(&self, ack: &Ack) -> Sent<()> {
        let body = match serde_json::to_string(ack) {
            Ok(b) => b,
            Err(e) => return Sent::Refused { status: 0, message: e.to_string() },
        };
        let res = self.send(reqwest::Method::POST, "/ack", Some(body)).await;
        Self::sorted(res, |_, _| Ok(()))
    }

    /// Re-send a captured request as it was (replay).
    pub async fn resend(&self, method: &str, path: &str, body: Option<String>) -> Result<(u16, String), String> {
        let method = reqwest::Method::from_bytes(method.as_bytes()).map_err(|e| e.to_string())?;
        let path = path.strip_prefix("/collar/v1").unwrap_or(path);
        self.send(method, path, body).await
    }
}

pub fn parse_report_reply(text: &str) -> Result<ReportReply, String> {
    #[derive(Deserialize)]
    struct Raw<'a> {
        latest_version: Option<u32>,
        #[serde(borrow)]
        config: Option<&'a RawValue>,
    }
    let raw: Raw = serde_json::from_str(text).map_err(|e| format!("unreadable report reply: {e}"))?;
    let config = match raw.config.filter(|c| c.get() != "null") {
        Some(c) => {
            let cmd: ConfigCommand = serde_json::from_str(c.get()).map_err(|e| format!("unreadable config: {e}"))?;
            Some((one_line(c.get()), cmd))
        }
        None => None,
    };
    Ok(ReportReply { latest_version: raw.latest_version, config })
}

pub fn parse_download(text: &str) -> Result<Download, String> {
    #[derive(Deserialize)]
    struct Head {
        command_id: String,
        version: u32,
    }
    let head: Head = serde_json::from_str(text).map_err(|e| format!("unreadable boundary: {e}"))?;
    Ok(Download { wire: one_line(text), command_id: head.command_id, version: head.version })
}

/// The console reads one line. JSON strings can't hold raw line breaks, so
/// any in the text sit between tokens, where they aren't signed.
pub fn one_line(text: &str) -> String {
    text.trim().replace(['\r', '\n'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_keep_signed_bytes() {
        let r = parse_report_reply(
            r#"{"latest_version":4,"config":{"command_id":"cfg_1","collar_id":"col_1","version":2,"report_s":60,"poll_s":60,"endpoint":"https:\/\/x.example\/collar\/v1","sig":"s"}}"#,
        )
        .unwrap();
        let (wire, cmd) = r.config.unwrap();
        // The escaped slashes stay as the server signed them.
        assert!(wire.contains(r#""endpoint":"https:\/\/x.example\/collar\/v1""#));
        assert_eq!((cmd.version, cmd.endpoint.as_deref()), (2, Some("https://x.example/collar/v1")));
        assert_eq!(r.latest_version, Some(4));
        assert!(parse_report_reply(r#"{"latest_version":null}"#).unwrap().config.is_none());

        let d = parse_download("{\n  \"command_id\": \"bnd_1\",\n  \"version\": 7,\n  \"boundary\": []\n}\n").unwrap();
        assert_eq!((d.command_id.as_str(), d.version), ("bnd_1", 7));
        assert!(!d.wire.contains('\n'));
    }

    #[test]
    fn server_urls() {
        for s in ["http://127.0.0.1:7993", "http://127.0.0.1:7993/", "http://127.0.0.1:7993/collar/v1", "http://127.0.0.1:7993/collar/v1/"] {
            assert_eq!(device_base(s), "http://127.0.0.1:7993/collar/v1");
        }
        assert_eq!(error_text(r#"{"error":"Unknown collar key."}"#), "Unknown collar key.");
        assert_eq!(error_text("gateway"), "gateway");
    }
}
