//! The farm's grazing history as reports read it: where each herd was, how
//! many head it had, and each paddock's area, from the `herd_history` and
//! `paddock_geometry_history` tables the triggers keep. Also the farm's clock
//! (report days are farm-local) and the pieces every report's header shares.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Context;
use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use chrono_tz::Tz;
use op_core::time::{from_db, now};
use op_core::units::Fmt;
use op_core::{Ctx, Paddock, Polygon};
use serde_json::{Value, json};
use sqlx::Row;

use crate::settings::{Counted, Inputs};
use crate::{ReportDoc, ReportParams};

pub use op_engine::calc::round;

/// One row of `herd_history`: the herd's state from `at` on.
#[derive(Debug, Clone, PartialEq)]
pub struct HerdState {
    pub at: DateTime<Utc>,
    pub count: u32,
    pub paddock_id: Option<String>,
    pub name: String,
    pub species: String,
    pub source: String,
}

/// A herd's time in one paddock, whole (not cut to any report's dates).
/// `parts` split it wherever the head count changed.
#[derive(Debug, Clone, PartialEq)]
pub struct Stay {
    pub herd_id: String,
    pub paddock_id: String,
    pub start: DateTime<Utc>,
    /// None: still there.
    pub end: Option<DateTime<Utc>>,
    pub parts: Vec<Part>,
    /// Some of it comes from records made before herd history was kept.
    pub backfilled: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Part {
    pub start: DateTime<Utc>,
    pub end: Option<DateTime<Utc>>,
    pub count: u32,
}

/// A stay cut to a report's window.
#[derive(Debug, Clone, PartialEq)]
pub struct Cut {
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    /// The herd is still there now (the window ends now and the stay hasn't).
    pub open: bool,
    /// The window cut the stay short at either end (not counting `open`).
    pub cut: bool,
    /// The stay went on past the window: `end` is the window's end (the
    /// midnight after the report's last day), not a day the herd left.
    pub cut_end: bool,
    /// Head on the first day of the cut (a stay always has head: [`Farm::stays`]).
    pub head: u32,
    pub days: f64,
    pub head_days: f64,
    /// (head, days) of each stretch at one count, in time order.
    pub counts: Vec<(u32, f64)>,
    /// The head count changed inside the cut.
    pub recounted: bool,
}

#[derive(Debug, Clone, PartialEq)]
struct PaddockVersion {
    at: DateTime<Utc>,
    area_ha: f64,
    name: String,
}

/// Everything a report reads, loaded once.
pub struct Farm {
    pub name: String,
    pub tz: Tz,
    pub fmt: Fmt,
    pub now: DateTime<Utc>,
    pub inputs: Inputs,
    pub paddocks: BTreeMap<String, Paddock>,
    /// Herd id → its states in time order.
    pub herds: BTreeMap<String, Vec<HerdState>>,
    /// Current herd names, for herds that still exist.
    current_herds: BTreeMap<String, String>,
    /// Herds whose count follows their animal rows (K-animals).
    by_animals: BTreeSet<String>,
    versions: BTreeMap<String, Vec<PaddockVersion>>,
}

impl Farm {
    pub async fn load(ctx: &Ctx) -> anyhow::Result<Self> {
        let farm = ctx.store().get_farm().await?;
        let tz = farm.as_ref().and_then(|f| f.timezone.parse::<Tz>().ok()).unwrap_or(Tz::UTC);
        let fmt = Fmt::of(ctx).await?;
        let inputs = Inputs::load(ctx).await?;
        let paddocks = ctx.store().list_paddocks().await?.into_iter().map(|p| (p.id.clone(), p)).collect();
        let current_herds = ctx.store().list_herds().await?.into_iter().map(|h| (h.id, h.name)).collect();
        let by_animals = sqlx::query_scalar::<_, String>("SELECT DISTINCT herd_id FROM animals").fetch_all(ctx.db()).await?.into_iter().collect();

        let mut herds: BTreeMap<String, Vec<HerdState>> = BTreeMap::new();
        let rows =
            sqlx::query("SELECT herd_id, at, count, paddock_id, name, species, source FROM herd_history ORDER BY herd_id, at, id").fetch_all(ctx.db()).await?;
        for r in rows {
            let herd_id: String = r.get("herd_id");
            let at: String = r.get("at");
            herds.entry(herd_id).or_default().push(HerdState {
                at: from_db(&at).with_context(|| format!("herd_history at {at:?}"))?,
                count: r.get::<i64, _>("count").max(0) as u32,
                paddock_id: r.get("paddock_id"),
                name: r.get("name"),
                species: r.get("species"),
                source: r.get("source"),
            });
        }

        let mut versions: BTreeMap<String, Vec<PaddockVersion>> = BTreeMap::new();
        let rows =
            sqlx::query("SELECT paddock_id, at, geometry, area_ha, name FROM paddock_geometry_history ORDER BY paddock_id, at, id").fetch_all(ctx.db()).await?;
        for r in rows {
            let at: String = r.get("at");
            let geometry: String = r.get("geometry");
            // Areas stored before op-geo measured every winding correctly can
            // be wrong; the geometry is the truth (to the stored precision).
            let area_ha = serde_json::from_str::<Polygon>(&geometry).map(|g| (g.area_ha() * 1000.0).round() / 1000.0).unwrap_or_else(|_| r.get("area_ha"));
            versions.entry(r.get("paddock_id")).or_default().push(PaddockVersion { at: from_db(&at)?, area_ha, name: r.get("name") });
        }

        Ok(Self { name: farm.map(|f| f.name).unwrap_or_default(), tz, fmt, now: now(), inputs, paddocks, herds, current_herds, by_animals, versions })
    }

    /// Farm-local midnight starting `day`, in UTC.
    pub fn midnight(&self, day: NaiveDate) -> DateTime<Utc> {
        let naive = day.and_hms_opt(0, 0, 0).expect("midnight");
        match self.tz.from_local_datetime(&naive) {
            chrono::LocalResult::Single(t) => t.with_timezone(&Utc),
            chrono::LocalResult::Ambiguous(t, _) => t.with_timezone(&Utc),
            // Midnight skipped by a clock change: the day starts an hour later.
            chrono::LocalResult::None => {
                self.tz.from_local_datetime(&(naive + Duration::hours(1))).earliest().map(|t| t.with_timezone(&Utc)).unwrap_or(naive.and_utc())
            }
        }
    }

    /// The report's window: farm-local `from` 00:00 to the end of `to`, and
    /// never past now.
    pub fn window(&self, p: &ReportParams) -> (DateTime<Utc>, DateTime<Utc>) {
        let w0 = self.midnight(p.from);
        let w1 = self.midnight(p.to + Duration::days(1)).min(self.now);
        (w0, w1.max(w0))
    }

    pub fn today(&self) -> NaiveDate {
        self.now.with_timezone(&self.tz).date_naive()
    }

    pub fn local_date(&self, t: DateTime<Utc>) -> NaiveDate {
        t.with_timezone(&self.tz).date_naive()
    }

    /// "2026-09-24 07:40", farm time.
    pub fn local_time(&self, t: DateTime<Utc>) -> String {
        t.with_timezone(&self.tz).format("%Y-%m-%d %H:%M").to_string()
    }

    /// Paddock name: the current one, else the last it had before it was deleted.
    pub fn paddock_name(&self, id: &str) -> String {
        if let Some(p) = self.paddocks.get(id) {
            return p.name.clone();
        }
        self.versions.get(id).and_then(|v| v.last()).map(|v| v.name.clone()).unwrap_or_else(|| id.to_owned())
    }

    /// The paddock's area at `t`: the shape it had then (its first known one
    /// for earlier times).
    pub fn area_at(&self, id: &str, t: DateTime<Utc>) -> Option<f64> {
        let v = self.versions.get(id)?;
        v.iter().rev().find(|x| x.at <= t).or(v.first()).map(|x| x.area_ha)
    }

    /// The paddock's area now (or when it was deleted).
    pub fn area_now(&self, id: &str) -> Option<f64> {
        self.versions.get(id).and_then(|v| v.last()).map(|v| v.area_ha)
    }

    pub fn paddock_prop(&self, id: &str, key: &str) -> Option<String> {
        let v = self.paddocks.get(id)?.props.get(key)?;
        match v {
            Value::String(s) if !s.trim().is_empty() => Some(s.trim().to_owned()),
            Value::Number(n) => Some(n.to_string()),
            _ => None,
        }
    }

    pub fn herd_name(&self, id: &str) -> String {
        if let Some(n) = self.current_herds.get(id) {
            return n.clone();
        }
        self.herds.get(id).and_then(|s| s.last()).map(|s| s.name.clone()).unwrap_or_else(|| id.to_owned())
    }

    pub fn species(&self, herd_id: &str) -> String {
        self.herds.get(herd_id).and_then(|s| s.last()).map(|s| s.species.clone()).unwrap_or_else(|| "cattle".into())
    }

    /// How the herd's count is kept: by its animal rows once it has any.
    pub fn counted(&self, herd_id: &str) -> Counted {
        if self.by_animals.contains(herd_id) { Counted::ByAnimals } else { Counted::ByHand }
    }

    /// Animal units of the herd at `count` head.
    pub fn animal_units(&self, herd_id: &str, count: u32) -> f64 {
        self.inputs.animal_units(herd_id, &self.species(herd_id), count, self.counted(herd_id))
    }

    /// Cow-calf pairs in the herd at `count` head, when its mix says so.
    pub fn pairs(&self, herd_id: &str, count: u32) -> Option<f64> {
        self.inputs.pairs(herd_id, &self.species(herd_id), count, self.counted(herd_id))
    }

    /// Herds the report covers: the one asked for, else every herd with history.
    pub fn herd_ids(&self, p: &ReportParams) -> Vec<String> {
        match &p.herd_id {
            Some(h) => vec![h.clone()],
            None => self.herds.keys().cloned().collect(),
        }
    }

    /// When herd history began being kept (the migration's install rows),
    /// if any rows were backfilled from older records.
    pub fn history_since(&self) -> Option<DateTime<Utc>> {
        self.herds.values().flatten().filter(|s| s.source == "install").map(|s| s.at).min()
    }

    /// Every stay of every herd, in herd then time order: its time in a
    /// paddock with head ([`with_head`]).
    pub fn stays(&self) -> Vec<Stay> {
        let since = self.history_since();
        let mut out = Vec::new();
        for (herd_id, states) in &self.herds {
            let mut cur: Option<Stay> = None;
            for (i, s) in states.iter().enumerate() {
                let next = states.get(i + 1).map(|n| n.at);
                let same = cur.as_ref().is_some_and(|c| Some(&c.paddock_id) == s.paddock_id.as_ref());
                if !same {
                    if let Some(mut c) = cur.take() {
                        c.end = Some(s.at);
                        out.push(c);
                    }
                    if let Some(p) = &s.paddock_id {
                        cur = Some(Stay { herd_id: herd_id.clone(), paddock_id: p.clone(), start: s.at, end: None, parts: vec![], backfilled: false });
                    }
                }
                if let Some(c) = cur.as_mut() {
                    // A zero-length state (two rows in one instant) adds nothing.
                    if next == Some(s.at) {
                        continue;
                    }
                    if let Some(last) = c.parts.last_mut()
                        && last.count == s.count
                    {
                        last.end = next;
                    } else {
                        if let Some(last) = c.parts.last_mut() {
                            last.end = Some(s.at);
                        }
                        c.parts.push(Part { start: s.at, end: next, count: s.count });
                    }
                    if since.is_some_and(|t| s.at < t) {
                        c.backfilled = true;
                    }
                }
            }
            if let Some(c) = cur.take() {
                out.push(c);
            }
        }
        with_head(out)
    }

    /// Days since any herd last left `paddock_id` before `t`; None with no
    /// earlier stay on record. A paddock another herd is still in rests 0.
    pub fn rest_before(&self, stays: &[Stay], paddock_id: &str, t: DateTime<Utc>) -> Option<f64> {
        let mut last: Option<DateTime<Utc>> = None;
        for s in stays.iter().filter(|s| s.paddock_id == paddock_id && s.start < t) {
            let end = s.end.map_or(t, |e| e.min(t));
            last = Some(last.map_or(end, |l| l.max(end)));
        }
        last.map(|l| days(t - l))
    }
}

/// Stays as a herd's time in a paddock with head. A stretch at 0 head isn't
/// grazing: a herd made empty and filled by its animals a moment later goes
/// in when they came, one emptied into another herd and left where it was
/// (the Training herd after training) leaves when its last animal did, and
/// one that never had any is no stay at all, so the paddock rests through it.
fn with_head(stays: Vec<Stay>) -> Vec<Stay> {
    let mut out = Vec::with_capacity(stays.len());
    for s in stays {
        let mut run: Vec<Part> = Vec::new();
        for p in s.parts.iter().copied().chain([Part { start: DateTime::<Utc>::MAX_UTC, end: None, count: 0 }]) {
            if p.count > 0 {
                run.push(p);
            } else if let (Some(first), Some(last)) = (run.first().copied(), run.last().copied()) {
                out.push(Stay { start: first.start, end: last.end, parts: std::mem::take(&mut run), ..s.clone() });
            }
        }
    }
    out
}

/// A span in days.
pub fn days(d: Duration) -> f64 {
    d.num_milliseconds().max(0) as f64 / 86_400_000.0
}

impl Cut {
    /// The farm day the cut ends on: the day the herd left, or for a stay
    /// that went on past the window, the window's last day.
    pub fn last_day(&self, farm: &Farm) -> NaiveDate {
        if self.cut_end { farm.local_date(self.end - Duration::milliseconds(1)) } else { farm.local_date(self.end) }
    }
}

impl Cut {
    /// Animal-unit days of `herd_id` in the cut, each stretch at its count's animal units.
    pub fn au_days(&self, farm: &Farm, herd_id: &str) -> f64 {
        self.counts.iter().map(|(c, d)| farm.animal_units(herd_id, *c) * d).sum()
    }

    /// Pair-days of `herd_id` in the cut, when its mix names pairs.
    pub fn pair_days(&self, farm: &Farm, herd_id: &str) -> Option<f64> {
        self.counts.iter().map(|(c, d)| farm.pairs(herd_id, *c).map(|p| p * d)).sum()
    }
}

impl Stay {
    /// The part of the stay inside `[w0, w1)`; `now` tells an open stay.
    pub fn cut(&self, w0: DateTime<Utc>, w1: DateTime<Utc>, now: DateTime<Utc>) -> Option<Cut> {
        let end = self.end.unwrap_or(DateTime::<Utc>::MAX_UTC);
        let (start, stop) = (self.start.max(w0), end.min(w1));
        if stop <= start {
            return None;
        }
        let mut head_days = 0.0;
        let mut head = None;
        let mut counts = Vec::new();
        for p in &self.parts {
            let (a, b) = (p.start.max(start), p.end.unwrap_or(DateTime::<Utc>::MAX_UTC).min(stop));
            if b > a {
                head.get_or_insert(p.count);
                counts.push((p.count, days(b - a)));
                head_days += p.count as f64 * days(b - a);
            }
        }
        let recounted = counts.iter().map(|(c, _)| *c).collect::<BTreeSet<_>>().len() > 1;
        let open = self.end.is_none() && w1 >= now;
        let cut_end = stop < end && !open;
        Some(Cut {
            start,
            end: stop,
            open,
            cut: start > self.start || cut_end,
            cut_end,
            head: head.unwrap_or(0),
            days: days(stop - start),
            head_days,
            counts,
            recounted,
        })
    }
}

/// Head-days of `herd_id` in `[w0, w1)`, wherever it was (or wasn't).
pub fn herd_days(states: &[HerdState], w0: DateTime<Utc>, w1: DateTime<Utc>) -> f64 {
    let mut total = 0.0;
    for (i, s) in states.iter().enumerate() {
        let end = states.get(i + 1).map_or(w1, |n| n.at);
        let (a, b) = (s.at.max(w0), end.min(w1));
        if b > a {
            total += s.count as f64 * days(b - a);
        }
    }
    total
}

/// Farm-local days in `[w0, w1)` on which the herd was in a paddock at any time.
pub fn days_in_paddocks(farm: &Farm, stays: &[&Stay], w0: DateTime<Utc>, w1: DateTime<Utc>) -> usize {
    let mut seen = BTreeSet::new();
    for s in stays {
        let Some(c) = s.cut(w0, w1, farm.now) else { continue };
        let mut day = farm.local_date(c.start);
        // The last instant inside the cut.
        let last = farm.local_date(c.end - Duration::milliseconds(1));
        while day <= last {
            seen.insert(day);
            day += Duration::days(1);
        }
    }
    seen.len()
}

/// Days collars place each herd in each paddock (UTC dates): (herd,
/// paddock) → dates. A day counts when the paddock held a real share of the
/// herd's tracked day ([`op_engine::signals::GRAZING_DAY_SHARE`], an hour's
/// worth: op-analytics' pasture rule and the engine's last grazed), from the
/// rolled-up and imported collar days (`paddock_day_dwell`). A few fixes
/// across a fence from the paddock the herd is really in don't make one.
pub async fn collar_days(ctx: &Ctx, w0: DateTime<Utc>, w1: DateTime<Utc>) -> anyhow::Result<BTreeMap<(String, String), BTreeSet<NaiveDate>>> {
    let rows = sqlx::query(
        "SELECT d.herd_id, d.paddock_id, d.date FROM (
             SELECT herd_id, date, paddock_id, SUM(dwell_s) AS dwell FROM paddock_day_dwell
             WHERE date >= ?1 AND date <= ?2 AND herd_id != '' GROUP BY herd_id, date, paddock_id) d
         JOIN (SELECT herd_id, date, SUM(dwell_s) AS total FROM paddock_day_dwell
               WHERE date >= ?1 AND date <= ?2 AND herd_id != '' GROUP BY herd_id, date) t ON t.herd_id = d.herd_id AND t.date = d.date
         WHERE d.paddock_id != '' AND d.dwell > 0 AND d.dwell >= ?3 * t.total",
    )
    .bind(w0.date_naive().to_string())
    .bind(w1.date_naive().to_string())
    .bind(op_engine::signals::GRAZING_DAY_SHARE)
    .fetch_all(ctx.db())
    .await?;
    let mut out: BTreeMap<(String, String), BTreeSet<NaiveDate>> = BTreeMap::new();
    for r in rows {
        let date: String = r.get("date");
        if let Ok(d) = date.parse::<NaiveDate>() {
            out.entry((r.get("herd_id"), r.get("paddock_id"))).or_default().insert(d);
        }
    }
    Ok(out)
}

/// Collar days inside a cut: UTC dates from the cut's first to its last instant.
pub fn collar_days_in(dates: Option<&BTreeSet<NaiveDate>>, c: &Cut) -> usize {
    let Some(dates) = dates else { return 0 };
    let (a, b) = (c.start.date_naive(), (c.end - Duration::milliseconds(1)).date_naive());
    dates.range(a..=b).count()
}

/// A number rounded to `d` places, as a JSON value. Adding `0.0` turns a
/// negative zero into zero: an empty `f64` sum is `-0.0`, and so is a small
/// negative rounded away, and neither should print as "-0.0" in a report.
pub fn n(x: f64, d: usize) -> Value {
    json!(round(x, d) + 0.0)
}

/// "Cattle".
pub fn species_label(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().chain(c).collect()).unwrap_or_default()
}

/// A method line for animal units, covering the herds in the report.
pub fn au_note(farm: &Farm, herd_ids: &[String]) -> Option<String> {
    if herd_ids.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    let mut species = BTreeSet::new();
    let mut mixed = Vec::new();
    for h in herd_ids {
        let sp = farm.species(h);
        match farm.inputs.mix(h, &sp) {
            Some(m) => {
                let mut what = Vec::new();
                if m.cows > 0 {
                    what.push(format!("{} {}", m.cows, if m.pairs { "pairs" } else { "cows" }));
                }
                if m.bulls > 0 {
                    what.push(format!("{} bulls", m.bulls));
                }
                if m.calves > 0 && !m.pairs {
                    what.push(format!("{} weaned calves", m.calves));
                }
                // The head counts the mix's animal units stand at (other counts scale them).
                let counted = farm.counted(h);
                let (low, high) = (m.head_at(0, counted), m.head_at(u32::MAX, counted));
                let mut at = format!("{} AU at {low}", fmt_au(m.animal_units(&farm.inputs.settings.au)));
                if high > low {
                    at += &format!(" to {high} head, calves at side counted or not");
                } else {
                    at += " head";
                }
                mixed.push(format!("{} {}: {at}", farm.herd_name(h), what.join(", ")));
            }
            None => {
                species.insert(sp);
            }
        }
    }
    for sp in &species {
        parts.push(format!("{} {}", sp, fmt_factor(op_engine::calc::animal_units(sp, 1, None))));
    }
    if !mixed.is_empty() {
        let f = farm.inputs.settings.au;
        parts.push(format!(
            "cow {}, bull {}, pair {}, weaned calf {} ({})",
            fmt_factor(f.cow),
            fmt_factor(f.bull),
            fmt_factor(f.pair),
            fmt_factor(f.weaned_calf),
            mixed.join("; ")
        ));
    }
    Some(format!("Animal units per head: {}.", parts.join("; ")))
}

/// "130.5", "143" (one decimal, dropped when zero).
fn fmt_au(v: f64) -> String {
    let s = format!("{:.1}", round(v, 1));
    s.strip_suffix(".0").map(str::to_owned).unwrap_or(s)
}

fn fmt_factor(v: f64) -> String {
    let s = format!("{:.2}", round(v, 2));
    let s = s.trim_end_matches('0');
    if s.ends_with('.') { format!("{s}0") } else { s.to_owned() }
}

/// The note for head counts recorded before herd history was kept.
pub fn backfill_note(farm: &Farm) -> Option<String> {
    farm.history_since().map(|t| format!("Head counts before {} are the count on that day; herd history starts then.", farm.local_date(t)))
}

/// A report's frame: header block (Farm, Operator, FSA farm, Dates, Herd;
/// only those known) and no sections yet.
pub fn doc(farm: &Farm, id: &str, title: &str, p: &ReportParams, fsa_farm: Option<String>) -> ReportDoc {
    let mut header = Vec::new();
    if !farm.name.is_empty() {
        header.push(("Farm".to_owned(), farm.name.clone()));
    }
    if let Some(o) = &farm.inputs.settings.operator {
        header.push(("Operator".into(), o.clone()));
    }
    if let Some(f) = farm.inputs.settings.fsa_farm.clone().or(fsa_farm) {
        header.push(("FSA farm".into(), f));
    }
    header.push(("Dates".into(), format!("{} – {}", p.from, p.to)));
    if let Some(h) = &p.herd_id {
        header.push(("Herd".into(), farm.herd_name(h)));
    }
    ReportDoc {
        id: id.into(),
        title: title.into(),
        farm: farm.name.clone(),
        from: p.from,
        to: p.to,
        herd_id: p.herd_id.clone(),
        generated_at: farm.now,
        header,
        sections: vec![],
        notes: vec![],
        signatures: vec![],
    }
}

/// The FSA farm number every listed paddock shares, if they all carry one.
pub fn shared_fsa_farm(farm: &Farm, paddock_ids: &BTreeSet<String>) -> Option<String> {
    let vals: BTreeSet<Option<String>> = paddock_ids.iter().map(|p| farm.paddock_prop(p, "fsa_farm")).collect();
    match vals.into_iter().collect::<Vec<_>>().as_slice() {
        [Some(v)] => Some(v.clone()),
        _ => None,
    }
}
