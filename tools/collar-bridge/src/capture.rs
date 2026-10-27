//! Capture files: every exchange, one JSON object a line, appended and
//! flushed as it happens so a crash or an unplug keeps everything before it.
//!
//! ```text
//! {"t":"…","kind":"serial","dir":"in","line":"[00:00:01.000,000] <inf> opencollar: …"}
//! {"t":"…","kind":"http","method":"POST","path":"/collar/v1/report","body":"{…}","status":200,"reply":"{…}","ms":12}
//! {"t":"…","kind":"note","text":"collar unplugged"}
//! ```
//!
//! Keys never go in: the collar key travels in a header that isn't kept,
//! and the key in a `provision` line is replaced with `[redacted]`.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::Context;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dir {
    /// From the collar.
    In,
    /// To the collar.
    Out,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum Record {
    Serial {
        t: DateTime<Utc>,
        dir: Dir,
        line: String,
    },
    Http {
        t: DateTime<Utc>,
        method: String,
        /// Path and query, e.g. `/collar/v1/boundary?have=3&free=14`.
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<String>,
        /// `None` when the server couldn't be reached.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
        ms: u64,
    },
    Note {
        t: DateTime<Utc>,
        text: String,
    },
}

/// Where records go; a capture without a file keeps nothing.
#[derive(Default)]
pub struct Capture {
    file: Option<Mutex<File>>,
    path: Option<PathBuf>,
}

impl Capture {
    pub fn none() -> Self {
        Self::default()
    }

    /// Append to `path`, creating it.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path).with_context(|| format!("opening capture file {}", path.display()))?;
        Ok(Self { file: Some(Mutex::new(file)), path: Some(path.to_owned()) })
    }

    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub fn write(&self, rec: &Record) {
        let Some(file) = &self.file else { return };
        let Ok(mut line) = serde_json::to_string(rec) else { return };
        line.push('\n');
        let mut f = file.lock().unwrap_or_else(|e| e.into_inner());
        if let Err(e) = f.write_all(line.as_bytes()) {
            eprintln!("capture: couldn't write: {e}");
        }
    }

    pub fn serial(&self, dir: Dir, line: &str) {
        self.write(&Record::Serial { t: Utc::now(), dir, line: redact_line(line) });
    }

    pub fn note(&self, text: impl Into<String>) {
        self.write(&Record::Note { t: Utc::now(), text: text.into() });
    }
}

/// A `provision` line with its collar key replaced.
pub fn redact_line(line: &str) -> String {
    let Some(payload) = line.strip_prefix("provision ") else { return line.to_owned() };
    match serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(payload) {
        Ok(mut m) => {
            if m.contains_key("k") {
                m.insert("k".into(), "[redacted]".into());
            }
            format!("provision {}", serde_json::Value::Object(m))
        }
        Err(_) => "provision [not JSON, not kept]".into(),
    }
}

/// Every record in a capture file, in order. Lines that don't read are
/// reported with their number.
pub fn read(path: &Path) -> anyhow::Result<Vec<Record>> {
    let f = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut out = Vec::new();
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        out.push(serde_json::from_str(&line).with_context(|| format!("{} line {}", path.display(), i + 1))?);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip_and_keys_stay_out() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cap.jsonl");
        let cap = Capture::open(&path).unwrap();
        cap.serial(Dir::Out, r#"provision {"v":1,"c":"col_1","h":"hrd_1","k":"0123456789abcdef0123","e":"https://x/collar/v1","s":"AAAA"}"#);
        cap.serial(Dir::In, "ok col_1");
        cap.write(&Record::Http {
            t: Utc::now(),
            method: "POST".into(),
            path: "/collar/v1/report".into(),
            body: Some("{}".into()),
            status: Some(200),
            reply: Some(r#"{"latest_version":null}"#.into()),
            error: None,
            ms: 3,
        });
        cap.note("unplugged");
        drop(cap);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("0123456789abcdef0123"));
        let recs = read(&path).unwrap();
        assert_eq!(recs.len(), 4);
        let Record::Serial { line, .. } = &recs[0] else { panic!() };
        assert!(line.contains(r#""k":"[redacted]""#) && line.contains(r#""c":"col_1""#));
        assert!(matches!(&recs[2], Record::Http { status: Some(200), .. }));
        assert_eq!(redact_line("provision nope"), "provision [not JSON, not kept]");
        assert_eq!(redact_line("status"), "status");
    }
}
