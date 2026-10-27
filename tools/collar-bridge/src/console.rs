//! The collar's serial console, line by line: what firmware 0.2 prints
//! (`opencollar/firmware/src/main.c`, `app.c`).
//!
//! Log lines carry Zephyr's prefix, `[00:01:02.345,678] <inf> opencollar: `,
//! which gives the collar's uptime. Replies to console commands (`provision`,
//! `status`, `boundary`, `config`) are printed bare, so a line without the
//! prefix is a reply.

use chrono::{DateTime, Utc};
use op_protocol::{AckStatus, CueKind, EpisodeOutcome, RejectCode};

/// One line from the collar.
#[derive(Debug, Clone, PartialEq)]
pub struct Logged {
    /// Collar uptime from the log prefix; `None` for replies.
    pub uptime_ms: Option<u64>,
    pub line: Line,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Line {
    /// `OpenCollar 0.2.0 starting`
    Boot { fw: String },
    Fix(FixLine),
    /// `searching: 7 tracked, 0 in fix`
    Searching { tracked: u32, in_fix: u32 },
    Episode(EpisodeLine),
    /// `ack v59 applied at 2026-10-26T15:04:05Z (1 pending)`
    Ack(AckLine),
    Reply(Reply),
    /// Any other log line (events, warnings), as printed without the prefix.
    Log(String),
    /// Bare text that isn't a reply: the boot banner, noise.
    Other(String),
}

/// `fix <lat>,<lon> acc=3.2m sats=9 v59 state=inside margin=200.2m ring=0 cue=off vol=0 at=…`
/// (or `… sats=9 (no boundary) at=…`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct FixLine {
    pub lat: f64,
    pub lon: f64,
    pub accuracy_m: f64,
    pub sats: Option<u32>,
    /// GNSS UTC of the fix. Firmware before the `at=` field prints none.
    pub at: Option<DateTime<Utc>>,
    /// Present while a boundary is in force.
    pub fence: Option<FenceLine>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct FenceLine {
    pub version: u32,
    pub state: String,
    pub margin_m: f64,
    pub ring: Option<u16>,
    /// The cue this fix played and its volume (0-4).
    pub cue: Option<(CueKind, u8)>,
    pub degraded: bool,
    pub track: bool,
    pub unarmed: bool,
}

/// `episode: crossed ring=0 cues=5 max=4 min_margin=-1.2m 15000 ms`
#[derive(Debug, Clone, PartialEq)]
pub struct EpisodeLine {
    pub outcome: EpisodeOutcome,
    pub ring: u16,
    pub cues: u32,
    pub max_level: u8,
    pub min_margin_m: f64,
    pub dur_ms: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct AckLine {
    pub version: u32,
    pub status: AckStatus,
    pub code: Option<RejectCode>,
    /// `None` when the collar had no GNSS time (`at -`).
    pub at: Option<DateTime<Utc>>,
}

/// What the collar said to a command.
#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    /// `provision`: `ok <collar id>`
    Provisioned(String),
    /// `config`: `ok <version>`
    ConfigOk(u32),
    /// `error <code>` (provisioning, config, or a command it couldn't read)
    Error(String),
    /// `boundary`: `applied N` or `received N`
    Boundary { version: u32, status: AckStatus },
    /// `boundary`: `rejected N <code>`
    Rejected { version: u32, code: String },
    Status(Status),
    /// `status` before provisioning: `unprovisioned fw 0.2.0`
    Unprovisioned { fw: String },
}

/// `collar <id> herd <id> fw 0.2.0 config 3 active 59 slots 59 60`
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Status {
    pub collar_id: String,
    pub herd_id: String,
    pub fw: String,
    pub config_version: Option<u32>,
    pub active: Option<u32>,
    /// Every version held, applied or staged.
    pub slots: Vec<u32>,
}

impl Status {
    /// Highest version held: the `have` of a download.
    pub fn have(&self) -> u32 {
        self.slots.iter().copied().max().unwrap_or(0)
    }
}

/// Parse one line as read from the port (any line ending already removed).
pub fn parse(raw: &str) -> Logged {
    let text = strip_ansi(raw);
    let text = text.trim_end_matches(['\r', '\n']).trim();
    match log_prefix(text) {
        Some((uptime_ms, msg)) => Logged { uptime_ms: Some(uptime_ms), line: parse_log(msg) },
        None => Logged { uptime_ms: None, line: parse_reply(text).map_or_else(|| Line::Other(text.to_owned()), Line::Reply) },
    }
}

/// Terminal colour codes Zephyr may add around log lines.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

/// `[hh:mm:ss.mmm,uuu] <lvl> module: msg` → (uptime ms, msg).
fn log_prefix(s: &str) -> Option<(u64, &str)> {
    let rest = s.strip_prefix('[')?;
    let (stamp, rest) = rest.split_once(']')?;
    let (hms, frac) = stamp.split_once('.')?;
    let mut parts = hms.split(':').map(|p| p.parse::<u64>().ok());
    let (h, m, sec) = (parts.next()??, parts.next()??, parts.next()??);
    let ms: u64 = frac.split(',').next()?.parse().ok()?;
    let rest = rest.trim_start().strip_prefix('<')?;
    let (_level, rest) = rest.split_once('>')?;
    let (_module, msg) = rest.trim_start().split_once(": ")?;
    Some(((h * 3600 + m * 60 + sec) * 1000 + ms, msg.trim()))
}

fn parse_log(msg: &str) -> Line {
    if let Some(rest) = msg.strip_prefix("fix ") {
        return parse_fix(rest).map_or_else(|| Line::Log(msg.to_owned()), Line::Fix);
    }
    if let Some(rest) = msg.strip_prefix("episode: ") {
        return parse_episode(rest).map_or_else(|| Line::Log(msg.to_owned()), Line::Episode);
    }
    if let Some(rest) = msg.strip_prefix("ack v") {
        return parse_ack(rest).map_or_else(|| Line::Log(msg.to_owned()), Line::Ack);
    }
    if let Some(rest) = msg.strip_prefix("searching: ") {
        let mut n = rest.split(|c: char| !c.is_ascii_digit()).filter(|p| !p.is_empty()).map(|p| p.parse().ok());
        if let (Some(Some(tracked)), Some(Some(in_fix))) = (n.next(), n.next()) {
            return Line::Searching { tracked, in_fix };
        }
    }
    if let Some(fw) = msg.strip_prefix("OpenCollar ").and_then(|r| r.strip_suffix(" starting")) {
        return Line::Boot { fw: fw.to_owned() };
    }
    Line::Log(msg.to_owned())
}

fn metres(v: &str) -> Option<f64> {
    v.strip_suffix('m').unwrap_or(v).parse().ok()
}

fn time(v: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(v).ok().map(|t| t.with_timezone(&Utc))
}

fn parse_fix(rest: &str) -> Option<FixLine> {
    let mut words = rest.split_whitespace();
    let (lat, lon) = words.next()?.split_once(',')?;
    let mut fix = FixLine { lat: lat.parse().ok()?, lon: lon.parse().ok()?, ..FixLine::default() };
    let mut fence = FenceLine::default();
    let mut fenced = false;
    let mut cue = None;
    let mut vol = 0u8;
    for w in words {
        match w.split_once('=') {
            Some(("acc", v)) => fix.accuracy_m = metres(v)?,
            Some(("sats", v)) => fix.sats = v.parse().ok(),
            Some(("at", v)) => fix.at = time(v),
            Some(("state", v)) => fence.state = v.to_owned(),
            Some(("margin", v)) => fence.margin_m = metres(v)?,
            Some(("ring", v)) => fence.ring = v.parse().ok(),
            Some(("cue", v)) => cue = serde_json::from_value::<CueKind>(serde_json::Value::String(v.into())).ok(),
            Some(("vol", v)) => vol = v.parse().unwrap_or(0),
            Some(_) => {}
            None => match w {
                "(degraded)" => fence.degraded = true,
                "(track)" => fence.track = true,
                "(unarmed)" => fence.unarmed = true,
                v if v.starts_with('v') && v.len() > 1 && v[1..].bytes().all(|b| b.is_ascii_digit()) => {
                    fence.version = v[1..].parse().ok()?;
                    fenced = true;
                }
                _ => {}
            },
        }
    }
    fence.cue = cue.map(|k| (k, vol));
    fix.fence = fenced.then_some(fence);
    Some(fix)
}

fn parse_episode(rest: &str) -> Option<EpisodeLine> {
    let mut words = rest.split_whitespace();
    let outcome = serde_json::from_value(serde_json::Value::String(words.next()?.into())).ok()?;
    let mut e = EpisodeLine { outcome, ring: 0, cues: 0, max_level: 0, min_margin_m: 0.0, dur_ms: 0 };
    let mut last_number = None;
    for w in words {
        match w.split_once('=') {
            Some(("ring", v)) => e.ring = v.parse().ok()?,
            Some(("cues", v)) => e.cues = v.parse().ok()?,
            Some(("max", v)) => e.max_level = v.parse().ok()?,
            Some(("min_margin", v)) => e.min_margin_m = metres(v)?,
            Some(_) => {}
            None if w == "ms" => e.dur_ms = last_number?,
            None => last_number = w.parse().ok(),
        }
    }
    Some(e)
}

/// After `ack v`: `59 applied at 2026-…Z (1 pending)` or `60 rejected bad_sig at - (2 pending)`.
fn parse_ack(rest: &str) -> Option<AckLine> {
    let mut words = rest.split_whitespace();
    let version = words.next()?.parse().ok()?;
    let status = AckStatus::parse(words.next()?)?;
    let mut code = None;
    let mut at = None;
    while let Some(w) = words.next() {
        if w == "at" {
            at = words.next().and_then(time);
            break;
        }
        code = RejectCode::parse(w);
    }
    Some(AckLine { version, status, code, at })
}

fn parse_reply(s: &str) -> Option<Reply> {
    let mut words = s.split_whitespace();
    let first = words.next()?;
    let second = words.next();
    let reply = match (first, second) {
        ("ok", Some(v)) => match v.parse() {
            Ok(n) => Reply::ConfigOk(n),
            Err(_) => Reply::Provisioned(v.to_owned()),
        },
        ("error", Some(code)) => Reply::Error(code.to_owned()),
        ("applied" | "received", Some(v)) => Reply::Boundary { version: v.parse().ok()?, status: AckStatus::parse(first)? },
        ("rejected", Some(v)) => Reply::Rejected { version: v.parse().ok()?, code: words.next().unwrap_or_default().to_owned() },
        ("unprovisioned", Some("fw")) => Reply::Unprovisioned { fw: words.next()?.to_owned() },
        ("collar", Some(id)) => Reply::Status(parse_status(id, words)?),
        _ => return None,
    };
    Some(reply)
}

fn parse_status<'a>(collar_id: &str, mut words: impl Iterator<Item = &'a str>) -> Option<Status> {
    let mut st = Status { collar_id: collar_id.to_owned(), ..Status::default() };
    let num = |w: &str| if w == "-" { None } else { w.parse().ok() };
    while let Some(w) = words.next() {
        match w {
            "herd" => st.herd_id = words.next()?.to_owned(),
            "fw" => st.fw = words.next()?.to_owned(),
            "config" => st.config_version = num(words.next()?),
            "active" => st.active = num(words.next()?),
            "slots" => {
                st.slots = words.by_ref().filter_map(num).collect();
                break;
            }
            _ => return None,
        }
    }
    (!st.herd_id.is_empty() && !st.fw.is_empty()).then_some(st)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        time(s).unwrap()
    }

    #[test]
    fn fix_lines() {
        let l = parse(
            "\u{1b}[0m[00:01:02.345,678] <inf> opencollar: fix 38.120000,-92.410000 acc=3.2m sats=9 v59 state=warning margin=2.5m ring=1 cue=warn vol=3 at=2026-10-26T15:04:05Z\u{1b}[0m\r",
        );
        assert_eq!(l.uptime_ms, Some(62_345));
        let Line::Fix(f) = l.line else { panic!("{l:?}") };
        assert_eq!((f.lat, f.lon, f.accuracy_m, f.sats, f.at), (38.12, -92.41, 3.2, Some(9), Some(t("2026-10-26T15:04:05Z"))));
        let fence = f.fence.unwrap();
        assert_eq!((fence.version, fence.state.as_str(), fence.margin_m, fence.ring, fence.cue), (59, "warning", 2.5, Some(1), Some((CueKind::Warn, 3))));

        let l = parse("[00:00:05.000,000] <inf> opencollar: fix 38.1,-92.4 acc=4.0m sats=6 v2 state=inside margin=20.0m ring=0 (unarmed) cue=off vol=0 at=2026-10-26T15:04:06Z");
        let Line::Fix(f) = l.line else { panic!() };
        let fence = f.fence.unwrap();
        assert!(fence.unarmed && fence.cue.is_none());

        let l = parse("[00:00:05.000,000] <inf> opencollar: fix 38.1,-92.4 acc=4.0m sats=6 (no boundary) at=2026-10-26T15:04:06Z");
        let Line::Fix(f) = l.line else { panic!() };
        assert!(f.fence.is_none());
        assert_eq!(f.at, Some(t("2026-10-26T15:04:06Z")));

        // Firmware without the time field: parsed, but with no time.
        let l = parse("[00:00:05.000,000] <inf> opencollar: fix 38.1,-92.4 acc=4.0m sats=6 (no boundary)");
        let Line::Fix(f) = l.line else { panic!() };
        assert_eq!(f.at, None);
    }

    #[test]
    fn other_log_lines() {
        assert_eq!(parse("[00:00:00.001,000] <inf> opencollar: OpenCollar 0.2.0 starting").line, Line::Boot { fw: "0.2.0".into() });
        assert_eq!(parse("[00:00:10.000,000] <inf> opencollar: searching: 7 tracked, 0 in fix").line, Line::Searching { tracked: 7, in_fix: 0 });
        let Line::Episode(e) = parse("[00:01:00.000,000] <inf> opencollar: episode: crossed ring=0 cues=5 max=4 min_margin=-1.2m 15000 ms").line else { panic!() };
        assert_eq!((e.outcome, e.ring, e.cues, e.max_level, e.min_margin_m, e.dur_ms), (EpisodeOutcome::Crossed, 0, 5, 4, -1.2, 15_000));
        let Line::Ack(a) = parse("[00:01:00.000,000] <inf> opencollar: ack v59 applied at 2026-10-26T15:04:05Z (1 pending)").line else { panic!() };
        assert_eq!((a.version, a.status, a.code, a.at), (59, AckStatus::Applied, None, Some(t("2026-10-26T15:04:05Z"))));
        let Line::Ack(a) = parse("[00:01:00.000,000] <inf> opencollar: ack v60 rejected bad_sig at - (2 pending)").line else { panic!() };
        assert_eq!((a.status, a.code, a.at), (AckStatus::Rejected, Some(RejectCode::BadSig), None));
        assert_eq!(parse("[00:01:00.000,000] <inf> opencollar: event: state -> outside").line, Line::Log("event: state -> outside".into()));
        assert_eq!(parse("*** Booting nRF Connect SDK v3.4.1 ***").line, Line::Other("*** Booting nRF Connect SDK v3.4.1 ***".into()));
    }

    #[test]
    fn replies() {
        let r = |s: &str| match parse(s).line {
            Line::Reply(r) => r,
            other => panic!("{other:?}"),
        };
        assert_eq!(r("ok col_01M5V9KCZ5G9SSW6WG3PW24743"), Reply::Provisioned("col_01M5V9KCZ5G9SSW6WG3PW24743".into()));
        assert_eq!(r("ok 3"), Reply::ConfigOk(3));
        assert_eq!(r("error bad_endpoint"), Reply::Error("bad_endpoint".into()));
        assert_eq!(r("applied 59"), Reply::Boundary { version: 59, status: AckStatus::Applied });
        assert_eq!(r("received 60"), Reply::Boundary { version: 60, status: AckStatus::Received });
        assert_eq!(r("rejected 159 bad_sig"), Reply::Rejected { version: 159, code: "bad_sig".into() });
        assert_eq!(r("unprovisioned fw 0.2.0"), Reply::Unprovisioned { fw: "0.2.0".into() });
        let Reply::Status(s) = r("collar col_1 herd hrd_1 fw 0.2.0 config 3 active 59 slots 59 60") else { panic!() };
        assert_eq!((s.config_version, s.active, s.slots.clone(), s.have()), (Some(3), Some(59), vec![59, 60], 60));
        let Reply::Status(s) = r("collar col_1 herd hrd_1 fw 0.2.0 config - active - slots -") else { panic!() };
        assert_eq!((s.config_version, s.active, s.slots.len(), s.have()), (None, None, 0, 0));
    }
}
