//! Alert texts: one template per kind, GSM-7 only, at most 160 characters,
//! with names cut to fit. Numbers go through `op_core::units`; places come
//! from `op_core::place::describe`. Pure functions of an alert and its
//! place, so every template is testable with worst-case names.
//!
//! - `214 outside P3, 200 ft N of east gate, 6m. Reply OK to ack`
//! - `31 outside P3 since 06:12. Reply OK to ack`
//! - `Cows: 180 of 250 collars silent 25m. Check coverage or the server`
//! - `Cows: move to P4 (30.6 ac, 3 d)? Reply Y or N. Code 4821`

pub mod brief;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use op_core::Ctx;
use op_core::alert::Alert;
use op_core::units::Fmt;
use serde_json::Value;

/// Longest alert text, in GSM-7 septets.
pub const MAX_ALERT: usize = 160;

// ---- GSM-7 ----------------------------------------------------------------------------

const BASIC: &str = "@£$¥èéùìòÇ\nØø\rÅåΔ_ΦΓΛΩΠΨΣΘΞÆæßÉ !\"#¤%&'()*+,-./0123456789:;<=>?¡ABCDEFGHIJKLMNOPQRSTUVWXYZÄÖÑÜ§¿abcdefghijklmnopqrstuvwxyzäöñüà";
const EXTENDED: &str = "^{}\\[~]|€\u{c}";

/// Septets a GSM-7 string takes (extension characters take two).
pub fn septets(s: &str) -> usize {
    s.chars().map(|c| if EXTENDED.contains(c) { 2 } else { 1 }).sum()
}

pub fn is_gsm7(s: &str) -> bool {
    s.chars().all(|c| BASIC.contains(c) || EXTENDED.contains(c))
}

/// The nearest GSM-7 text: curly quotes, dashes and odd spaces become their
/// plain forms, accents come off letters GSM-7 lacks, anything else (emoji,
/// symbols) is dropped. Runs of spaces collapse.
pub fn gsm7(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let mapped: Option<&str> = match c {
            _ if BASIC.contains(c) && c != '\n' && c != '\r' => {
                out.push(c);
                continue;
            }
            '\n' | '\r' | '\t' | '\u{a0}' | '\u{2002}'..='\u{200a}' | '\u{202f}' => Some(" "),
            '‘' | '’' | '‚' | '′' | '`' | '´' => Some("'"),
            '“' | '”' | '„' | '″' => Some("\""),
            '–' | '—' | '−' | '‐' | '‑' => Some("-"),
            '…' => Some("..."),
            '²' => Some("2"),
            '³' => Some("3"),
            '°' => Some(" deg"),
            'á' | 'â' | 'ã' | 'ā' | 'ą' => Some("a"),
            'Á' | 'À' | 'Â' | 'Ã' | 'Ā' | 'Ą' => Some("A"),
            'ç' | 'ć' | 'č' => Some("c"),
            'Ć' | 'Č' => Some("C"),
            'ê' | 'ë' | 'ē' | 'ę' | 'ě' => Some("e"),
            'È' | 'Ê' | 'Ë' | 'Ē' | 'Ę' | 'Ě' => Some("E"),
            'í' | 'î' | 'ï' | 'ī' => Some("i"),
            'Í' | 'Ì' | 'Î' | 'Ï' | 'Ī' => Some("I"),
            'ó' | 'ô' | 'õ' | 'ō' | 'ő' => Some("o"),
            'Ó' | 'Ò' | 'Ô' | 'Õ' | 'Ō' | 'Ő' => Some("O"),
            'ú' | 'û' | 'ū' | 'ů' | 'ű' => Some("u"),
            'Ú' | 'Ù' | 'Û' | 'Ū' | 'Ů' | 'Ű' => Some("U"),
            'ý' | 'ÿ' => Some("y"),
            'Ý' | 'Ÿ' => Some("Y"),
            'ł' => Some("l"),
            'Ł' => Some("L"),
            'ń' | 'ň' => Some("n"),
            'Ń' | 'Ň' => Some("N"),
            'ř' => Some("r"),
            'Ř' => Some("R"),
            'ś' | 'š' | 'ș' | 'ş' => Some("s"),
            'Ś' | 'Š' | 'Ș' | 'Ş' => Some("S"),
            'ť' | 'ț' | 'ţ' => Some("t"),
            'Ť' | 'Ț' | 'Ţ' => Some("T"),
            'ź' | 'ż' | 'ž' => Some("z"),
            'Ź' | 'Ż' | 'Ž' => Some("Z"),
            'œ' => Some("oe"),
            'Œ' => Some("OE"),
            'ð' => Some("d"),
            'Ð' => Some("D"),
            'þ' => Some("th"),
            'Þ' => Some("Th"),
            _ if EXTENDED.contains(c) && c != '\u{c}' => {
                out.push(c);
                continue;
            }
            _ => None,
        };
        if let Some(m) = mapped {
            out.push_str(m);
        }
    }
    let mut clean = String::with_capacity(out.len());
    for c in out.chars() {
        if c == ' ' && (clean.is_empty() || clean.ends_with(' ')) {
            continue;
        }
        clean.push(c);
    }
    clean.trim_end().to_owned()
}

/// Cut to `max` septets.
fn cut(s: &str, max: usize) -> String {
    let mut n = 0;
    let mut out = String::new();
    for c in s.chars() {
        n += septets(c.encode_utf8(&mut [0; 4]));
        if n > max {
            break;
        }
        out.push(c);
    }
    out
}

/// A piece of a text: fixed words, or a name that may be cut to fit.
enum Part {
    Fix(String),
    Name(String),
}

fn fix(s: impl Into<String>) -> Part {
    Part::Fix(s.into())
}

fn name(s: impl AsRef<str>) -> Part {
    Part::Name(s.as_ref().to_owned())
}

/// Join the parts in GSM-7; while too long, cut the longest name.
fn compose(parts: Vec<Part>, max: usize) -> String {
    let mut parts: Vec<(String, bool)> = parts
        .into_iter()
        .map(|p| match p {
            // Fixed pieces keep their edge spaces; names lose theirs.
            Part::Fix(s) => (gsm7_keep_edges(&s), false),
            Part::Name(s) => (gsm7(&s), true),
        })
        .collect();
    loop {
        let s: String = parts.iter().map(|p| p.0.as_str()).collect();
        let len = septets(&s);
        if len <= max {
            return s;
        }
        let over = len - max;
        let mut names: Vec<(usize, usize)> = parts.iter().enumerate().filter(|(_, p)| p.1).map(|(i, p)| (i, septets(&p.0))).filter(|(_, l)| *l > 1).collect();
        names.sort_by_key(|(_, l)| std::cmp::Reverse(*l));
        let Some(&(i, longest)) = names.first() else { return cut(&s, max) };
        let second = names.get(1).map_or(1, |x| x.1);
        let by = over.min(longest - second + 1).max(1);
        parts[i].0 = cut(&parts[i].0, longest.saturating_sub(by).max(1)).trim_end().to_owned();
    }
}

fn gsm7_keep_edges(s: &str) -> String {
    let lead = s.starts_with(' ');
    let trail = s.ends_with(' ') && s.trim() != "";
    let core = gsm7(s);
    format!("{}{}{}", if lead && !core.is_empty() { " " } else { "" }, core, if trail { " " } else { "" })
}

// ---- facts ----------------------------------------------------------------------------

/// Units, farm time zone and the time a text is written.
#[derive(Debug, Clone, Copy)]
pub struct TextCtx {
    pub fmt: Fmt,
    pub tz: Tz,
    pub now: DateTime<Utc>,
}

impl TextCtx {
    pub async fn of(ctx: &Ctx, now: DateTime<Utc>) -> anyhow::Result<Self> {
        let tz = ctx.store().get_farm().await?.and_then(|f| f.timezone.parse().ok()).unwrap_or(Tz::UTC);
        Ok(Self { fmt: Fmt::of(ctx).await?, tz, now })
    }
}

fn s<'a>(a: &'a Alert, k: &str) -> Option<&'a str> {
    a.data.get(k).and_then(Value::as_str).filter(|v| !v.trim().is_empty())
}

fn n(a: &Alert, k: &str) -> Option<f64> {
    a.data.get(k).and_then(Value::as_f64)
}

fn since(a: &Alert) -> DateTime<Utc> {
    s(a, "since").and_then(|t| op_core::time::from_db(t).ok()).unwrap_or(a.opened_at)
}

/// "6m", "3h", "2d".
pub fn age(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    let m = (now - since).num_minutes().max(1);
    if m < 60 {
        format!("{m}m")
    } else if m < 48 * 60 {
        format!("{}h", m / 60)
    } else {
        format!("{}d", m / 1440)
    }
}

/// "06:12" in farm time.
pub fn clock(t: DateTime<Utc>, tz: Tz) -> String {
    t.with_timezone(&tz).format("%H:%M").to_string()
}

/// A rollup's key ends in `:herd:<herd id>`; `herd_silent` is a herd alert of its own.
fn is_rollup(a: &Alert) -> bool {
    a.kind != "herd_silent" && a.key.starts_with(&format!("{}:herd:", a.kind))
}

fn label(a: &Alert) -> String {
    s(a, "label").map(str::to_owned).unwrap_or_else(|| a.title.clone())
}

fn herd(a: &Alert) -> String {
    s(a, "herd").unwrap_or("Herd").to_owned()
}

fn count(a: &Alert) -> usize {
    n(a, "count").map_or(1, |c| c as usize)
}

fn days(d: f64) -> String {
    if d >= 1.0 { format!("{}", d.round() as i64) } else { format!("{:.1}", (d * 10.0).round() / 10.0) }
}

/// A rollup's title: "31 collars outside P3", "31 collars silent". A count
/// always has its noun, so it never reads as a collar's label beside one
/// ("5 collars GPS weak" next to "138 GPS weak", collar 138).
pub fn rollup_title(kind: &str, n: usize, paddock: Option<&str>) -> String {
    let collars = if n == 1 { "collar" } else { "collars" };
    match kind {
        "escaped" | "outside" => match paddock {
            Some(p) => format!("{n} {collars} outside {p}"),
            None => format!("{n} {collars} outside the boundary"),
        },
        "silent" => format!("{n} {collars} silent"),
        "low_battery" => format!("{n} batteries low"),
        "boundary_not_applied" => format!("{n} boundaries not applied"),
        "drop_off" => format!("{n} {collars} not moving"),
        "gps_degraded" => format!("{n} {collars} GPS weak"),
        // @S
        "schedule_not_stored" => format!("{n} {collars} missing the next strip"),
        // @H
        "fit_check_due" => format!("{n} fit checks due"),
        other => format!("{n} {}", other.replace('_', " ")),
    }
}

// ---- templates ------------------------------------------------------------------------

/// The text for one alert (a rollup included). `place` is where it is, in
/// words ("60 m N of P3").
pub fn alert_text(a: &Alert, place: Option<&str>, t: &TextCtx) -> String {
    let ack = ". Reply OK to ack";
    let age = age(since(a), t.now);
    let paddock = s(a, "paddock");
    let outside_of = |parts: &mut Vec<Part>| match paddock {
        Some(p) => parts.push(name(p)),
        None => parts.push(fix("the boundary")),
    };
    let mut p: Vec<Part> = Vec::new();
    match a.kind.as_str() {
        "escaped" | "outside" if is_rollup(a) => {
            p.push(fix(format!("{} outside ", count(a))));
            outside_of(&mut p);
            p.push(fix(format!(" since {}{ack}", clock(since(a), t.tz))));
        }
        "escaped" | "outside" => {
            p.push(name(label(a)));
            p.push(fix(" outside "));
            outside_of(&mut p);
            if let Some(pl) = place {
                p.push(fix(", "));
                p.push(name(pl));
            }
            p.push(fix(format!(", {age}{ack}")));
        }
        "silent" if is_rollup(a) => {
            p.push(name(herd(a)));
            p.push(fix(format!(": {} collars silent {age}. Check coverage or the server", count(a))));
        }
        "silent" => {
            p.push(name(label(a)));
            p.push(fix(format!(" silent {age}")));
            if let Some(pl) = place {
                p.push(fix(", last "));
                p.push(name(pl));
            }
            p.push(fix(ack));
        }
        "herd_silent" => {
            p.push(name(herd(a)));
            let total = n(a, "total").map_or(String::new(), |t| format!(" of {}", t as i64));
            p.push(fix(format!(": {}{total} collars silent {age}. Check coverage or the server", count(a))));
        }
        "low_battery" if is_rollup(a) => {
            p.push(name(herd(a)));
            p.push(fix(format!(": {} collar batteries low. Charge or swap them", count(a))));
        }
        "low_battery" => {
            p.push(name(label(a)));
            p.push(fix(format!(" battery {}%", n(a, "pct").unwrap_or(0.0) as i64)));
        }
        "boundary_not_applied" if is_rollup(a) => {
            p.push(name(herd(a)));
            p.push(fix(format!(": {} collars on an old boundary {age}. Check coverage", count(a))));
        }
        "boundary_not_applied" => {
            p.push(name(label(a)));
            p.push(fix(format!(" on an old boundary {age}{ack}")));
        }
        // @S: the approval prompt for a call about a strip schedule.
        "decision_waiting" if schedule_prompt(a).is_some() => {
            let (strip, of, opens) = schedule_prompt(a).unwrap_or_default();
            let code = s(a, "code").unwrap_or("");
            let when = opens.map(|o| day_clock(o, t)).unwrap_or_default();
            p.push(name(herd(a)));
            if s(a, "action") == Some("HOLD") {
                p.push(fix(format!(": hold today's strip? Strip {strip} of {of} is due {when}. Reply Y or N. Code {code}")));
            } else {
                p.push(fix(format!(": strip {strip} of {of} opens {when}. Reply Y to keep, N to hold. Code {code}")));
            }
        }
        "decision_waiting" => {
            let code = s(a, "code").unwrap_or("");
            p.push(name(herd(a)));
            if s(a, "action") == Some("STAY") {
                match paddock {
                    Some(pd) => {
                        p.push(fix(": stay in "));
                        p.push(name(pd));
                        p.push(fix("?"));
                    }
                    None => p.push(fix(": stay?")),
                }
            } else {
                p.push(fix(": move to "));
                match paddock {
                    Some(pd) => p.push(name(pd)),
                    None => p.push(fix("the new boundary")),
                }
                let mut facts = Vec::new();
                if let Some(ha) = n(a, "area_ha") {
                    facts.push(t.fmt.area(ha));
                }
                if let Some(d) = n(a, "days") {
                    facts.push(format!("{} d", days(d)));
                }
                p.push(fix(if facts.is_empty() { "?".to_owned() } else { format!(" ({})?", facts.join(", ")) }));
            }
            p.push(fix(format!(" Reply Y or N. Code {code}")));
        }
        "move_stalled" => {
            p.push(name(herd(a)));
            match paddock {
                Some(pd) => {
                    p.push(fix(": move to "));
                    p.push(name(pd));
                }
                None => p.push(fix(": move")),
            }
            if a.data.get("staged").and_then(Value::as_bool) == Some(true) {
                p.push(fix(format!(" waiting on a staged boundary {age}")));
            } else {
                p.push(fix(format!(" stalled {age}. Check the herd")));
            }
        }
        "stragglers" => {
            let labels: Vec<String> =
                a.data.get("labels").and_then(Value::as_array).map(|v| v.iter().filter_map(|x| x.as_str().map(str::to_owned)).collect()).unwrap_or_default();
            if labels.len() == 1 {
                p.push(name(&labels[0]));
                p.push(fix(" left behind by the move"));
            } else {
                return list_text(&format!("{}: {} left behind by the move:", cut(&gsm7(&herd(a)), 40), labels.len()), &labels);
            }
        }
        "drop_off" if is_rollup(a) => {
            p.push(name(herd(a)));
            p.push(fix(format!(": {} collars not moving {age}. They may be off", count(a))));
        }
        "drop_off" => {
            p.push(name(label(a)));
            p.push(fix(format!(" not moving {age}")));
            if let Some(pl) = place {
                p.push(fix(", "));
                p.push(name(pl));
            }
            p.push(fix(". Collar may be off"));
        }
        "gps_degraded" if is_rollup(a) => {
            p.push(name(herd(a)));
            p.push(fix(format!(": GPS weak on {} collars", count(a))));
        }
        "gps_degraded" => {
            p.push(name(label(a)));
            match (n(a, "accuracy_m"), s(a, "no_fix_since").and_then(|x| op_core::time::from_db(x).ok())) {
                (Some(m), _) => p.push(fix(format!(" GPS weak, {} accuracy", t.fmt.len(m)))),
                (None, Some(since)) => p.push(fix(format!(" no GPS fix {}", age_of(since, t.now)))),
                (None, None) => p.push(fix(" GPS weak")),
            }
        }
        // @S
        "schedule_not_stored" => {
            let strip = n(a, "strip").map_or(String::new(), |k| format!(" {}", k as i64));
            let when = s(a, "opens_at").and_then(|x| op_core::time::from_db(x).ok()).map(|o| format!(" (opens {})", day_clock(o, t))).unwrap_or_default();
            p.push(name(herd(a)));
            let missing = n(a, "missing").unwrap_or(1.0) as i64;
            let labels: Vec<&str> = a.data.get("labels").and_then(Value::as_array).map(|v| v.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
            if missing == 1
                && let Some(l) = labels.first()
            {
                p.push(fix(": "));
                p.push(name(l));
                p.push(fix(format!(" missing strip{strip}{when}. Check coverage")));
            } else {
                let total = n(a, "total").map_or(String::new(), |t| format!(" of {}", t as i64));
                p.push(fix(format!(": {missing}{total} collars missing strip{strip}{when}. Check coverage")));
            }
        }
        // @H
        _ => p.push(name(&a.title)),
    }
    compose(p, MAX_ALERT)
}

fn age_of(since: DateTime<Utc>, now: DateTime<Utc>) -> String {
    age(since, now)
}

// @S
/// `(strip, of, opens_at)` of the schedule a decision was about.
fn schedule_prompt(a: &Alert) -> Option<(u64, u64, Option<DateTime<Utc>>)> {
    let sc = a.data.get("schedule").filter(|v| v.is_object())?;
    let strip = sc.get("strip").and_then(Value::as_u64)?;
    let of = sc.get("of").and_then(Value::as_u64)?;
    let opens = sc.get("opens_at").and_then(Value::as_str).and_then(|t| op_core::time::from_db(t).ok());
    Some((strip, of, opens))
}

/// "07:00" today in farm time, "Wed 07:00" another day.
fn day_clock(at: DateTime<Utc>, t: &TextCtx) -> String {
    let local = at.with_timezone(&t.tz);
    if local.date_naive() == t.now.with_timezone(&t.tz).date_naive() { local.format("%H:%M").to_string() } else { local.format("%a %H:%M").to_string() }
}

/// "3 outside P3: 214 031 118", fitted: labels that don't fit become "+2".
fn list_text(head: &str, labels: &[String]) -> String {
    let head = cut(head, MAX_ALERT - 6);
    let mut out = head.clone();
    for (i, l) in labels.iter().enumerate() {
        let l = gsm7(l);
        let rest = labels.len() - i - 1;
        let tail = if rest > 0 { format!(" +{rest}") } else { String::new() };
        if septets(&out) + 1 + septets(&l) + septets(&tail) > MAX_ALERT {
            if i == 0 {
                // Not even one whole: cut it.
                let room = MAX_ALERT.saturating_sub(septets(&out) + 1 + format!(" +{}", labels.len() - 1).len());
                if room > 0 {
                    out.push(' ');
                    out.push_str(&cut(&l, room));
                    if labels.len() > 1 {
                        out.push_str(&format!(" +{}", labels.len() - 1));
                    }
                }
                return out;
            }
            out.push_str(&format!(" +{}", labels.len() - i));
            return out;
        }
        out.push(' ');
        out.push_str(&l);
    }
    out
}

/// One text for several alerts of one kind in one herd, opened in one
/// grouping window: "3 outside P3: 214 031 118".
pub fn group_text(alerts: &[Alert], place: Option<&str>, t: &TextCtx) -> String {
    match alerts {
        [] => String::new(),
        [one] => alert_text(one, place, t),
        _ => {
            let a = &alerts[0];
            let total: usize = alerts.iter().map(count).sum();
            let labels: Vec<String> = alerts
                .iter()
                .flat_map(|x| {
                    if is_rollup(x) {
                        x.data
                            .get("members")
                            .and_then(Value::as_array)
                            .map(|m| m.iter().filter_map(|y| y["label"].as_str().map(str::to_owned)).collect())
                            .unwrap_or_default()
                    } else {
                        vec![label(x)]
                    }
                })
                .collect();
            let phrase = match a.kind.as_str() {
                "escaped" | "outside" => match s(a, "paddock") {
                    Some(p) => format!("outside {}", cut(&gsm7(p), 40)),
                    None => "outside the boundary".to_owned(),
                },
                "silent" => "silent".into(),
                "low_battery" => "low battery".into(),
                "boundary_not_applied" => "on an old boundary".into(),
                "drop_off" => "not moving".into(),
                "gps_degraded" => "GPS weak".into(),
                "decision_waiting" | "move_stalled" | "stragglers" | "herd_silent" => return alert_text(a, place, t),
                // @S
                "schedule_not_stored" => return alert_text(a, place, t),
                // @H
                "fit_check_due" => "fit check due".into(),
                other => other.replace('_', " "),
            };
            list_text(&format!("{total} {phrase}:"), &labels)
        }
    }
}

/// An email's subject line.
pub fn subject(alerts: &[Alert]) -> String {
    match alerts {
        [one] => one.title.clone(),
        [first, ..] => format!("{} and {} more", first.title, alerts.len() - 1),
        [] => "openpasture".into(),
    }
}
