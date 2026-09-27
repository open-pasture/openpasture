//! Reads behind the welfare record: cues (SQLite and Parquet day files),
//! episodes (reported and derived), learning status, farm days, the ledger,
//! map ticks, fit checks and times a collar lay still.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Duration, NaiveDate, TimeZone, Utc};
use datafusion::arrow::array::timezone::Tz;
use datafusion::arrow::datatypes::{DataType, Field, Schema, SchemaRef};
use datafusion::arrow::record_batch::RecordBatch;
use op_core::{Actor, Ctx, LonLat};
use op_geo::Projection;
use parquet::arrow::ProjectionMask;
use parquet::arrow::arrow_reader::ParquetRecordBatchReaderBuilder;
use serde::Serialize;
use sqlx::Row;

use super::derive::LEGACY_BEEP_MS;
use crate::range::date_of;
use crate::schema::{Cols, get_f64, get_i64, get_str, normalize};
use crate::telemetry::{BATCH_ROWS, list_days};

// ---------------------------------------------------------------- cues

/// One cue as the welfare record reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Cue {
    pub id: i64,
    pub collar_id: String,
    pub herd_id: Option<String>,
    pub animal_id: Option<String>,
    pub t: i64,
    pub level: u8,
    pub margin_m: f64,
    /// `warn` or `outside`: as the collar said, else from the margin
    /// (firmware 0.1 sends no kind).
    pub kind: &'static str,
    /// Whether the collar reported the kind (firmware 0.2 and later).
    pub reported: bool,
    pub ring: Option<u32>,
    pub dur_ms: Option<u32>,
    pub boundary_version: Option<u32>,
    pub point: Option<LonLat>,
}

impl Cue {
    /// Tone length counted: what the collar reported, else firmware 0.1's beep.
    pub fn tone_ms(&self) -> u32 {
        self.dur_ms.unwrap_or(LEGACY_BEEP_MS)
    }
}

fn kind_of(kind: Option<&str>, margin_m: f64) -> (&'static str, bool) {
    match kind {
        Some("outside") => ("outside", true),
        Some(_) => ("warn", true),
        None if margin_m < 0.0 => ("outside", false),
        None => ("warn", false),
    }
}

/// Whose cues or episodes to read.
#[derive(Debug, Clone, Copy)]
pub enum Whose<'a> {
    Animal(&'a str),
    /// Those recorded for this herd (the herd the collar was in then).
    Herd(&'a str),
    Animals(&'a [String]),
    All,
}

impl Whose<'_> {
    fn owned(&self) -> Owned {
        match self {
            Whose::Animal(a) => Owned::Animal((*a).to_owned()),
            Whose::Herd(h) => Owned::Herd((*h).to_owned()),
            Whose::Animals(a) => Owned::Animals(a.iter().cloned().collect()),
            Whose::All => Owned::All,
        }
    }
}

/// [`Whose`] to carry into a blocking read.
enum Owned {
    Animal(String),
    Herd(String),
    Animals(HashSet<String>),
    All,
}

impl Owned {
    fn takes(&self, animal: Option<&str>, herd: Option<&str>) -> bool {
        match self {
            Owned::Animal(a) => animal == Some(a.as_str()),
            Owned::Herd(h) => herd == Some(h.as_str()),
            Owned::Animals(s) => animal.is_some_and(|a| s.contains(a)),
            Owned::All => true,
        }
    }
}

const CUE_COLS: &str = "id, collar_id, herd_id, animal_id, t, level, margin_m, kind, ring, dur_ms, boundary_version, lon, lat";

fn cue_schema() -> SchemaRef {
    let f = |n: &str, t: DataType| Field::new(n, t, true);
    Arc::new(Schema::new(vec![
        f("id", DataType::Int64),
        f("collar_id", DataType::Utf8),
        f("herd_id", DataType::Utf8),
        f("animal_id", DataType::Utf8),
        f("t", DataType::Int64),
        f("level", DataType::Int64),
        f("margin_m", DataType::Float64),
        f("kind", DataType::Utf8),
        f("ring", DataType::Int64),
        f("dur_ms", DataType::Int64),
        f("boundary_version", DataType::Int64),
        f("lon", DataType::Float64),
        f("lat", DataType::Float64),
    ]))
}

fn u32_of(v: Option<i64>) -> Option<u32> {
    v.and_then(|x| u32::try_from(x).ok())
}

/// A day file's columns that `target` names (the others read as null).
fn read_projected(path: &Path, target: &SchemaRef, mut f: impl FnMut(&RecordBatch)) -> anyhow::Result<()> {
    let builder = ParquetRecordBatchReaderBuilder::try_new(File::open(path)?)?;
    let have = builder.schema().clone();
    let roots: Vec<usize> = target.fields().iter().filter_map(|t| have.index_of(t.name()).ok()).collect();
    let mask = ProjectionMask::roots(builder.parquet_schema(), roots);
    for b in builder.with_projection(mask).with_batch_size(BATCH_ROWS).build()? {
        f(&normalize(&b?, target)?);
    }
    Ok(())
}

fn cold_cues(path: &Path, whose: &Owned, from: i64, to: i64, out: &mut Vec<Cue>) -> anyhow::Result<()> {
    read_projected(path, &cue_schema(), |b| {
        let c = Cols::new(b);
        let (id, collar, herd, animal, t, level, margin) =
            (c.i64("id"), c.str("collar_id"), c.str("herd_id"), c.str("animal_id"), c.i64("t"), c.i64("level"), c.f64("margin_m"));
        let (kind, ring, dur, bv, lon, lat) = (c.str("kind"), c.i64("ring"), c.i64("dur_ms"), c.i64("boundary_version"), c.f64("lon"), c.f64("lat"));
        for i in 0..b.num_rows() {
            let (Some(id), Some(collar_id), Some(t)) = (get_i64(id, i), get_str(collar, i), get_i64(t, i)) else { continue };
            if t < from || t >= to || !whose.takes(get_str(animal, i), get_str(herd, i)) {
                continue;
            }
            let margin_m = get_f64(margin, i).unwrap_or(0.0);
            let (k, reported) = kind_of(get_str(kind, i), margin_m);
            out.push(Cue {
                id,
                collar_id: collar_id.to_owned(),
                herd_id: get_str(herd, i).map(str::to_owned),
                animal_id: get_str(animal, i).map(str::to_owned),
                t,
                level: get_i64(level, i).unwrap_or(0).clamp(0, 255) as u8,
                margin_m,
                kind: k,
                reported,
                ring: u32_of(get_i64(ring, i)),
                dur_ms: u32_of(get_i64(dur, i)),
                boundary_version: u32_of(get_i64(bv, i)),
                point: get_f64(lon, i).zip(get_f64(lat, i)).map(|(x, y)| [x, y]),
            });
        }
    })
}

fn cue_from_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Cue> {
    let margin_m: f64 = r.try_get("margin_m")?;
    let kind: Option<String> = r.try_get("kind")?;
    let (k, reported) = kind_of(kind.as_deref(), margin_m);
    let lon: Option<f64> = r.try_get("lon")?;
    let lat: Option<f64> = r.try_get("lat")?;
    Ok(Cue {
        id: r.try_get("id")?,
        collar_id: r.try_get("collar_id")?,
        herd_id: r.try_get("herd_id")?,
        animal_id: r.try_get("animal_id")?,
        t: r.try_get("t")?,
        level: r.try_get::<i64, _>("level")?.clamp(0, 255) as u8,
        margin_m,
        kind: k,
        reported,
        ring: u32_of(r.try_get("ring")?),
        dur_ms: u32_of(r.try_get("dur_ms")?),
        boundary_version: u32_of(r.try_get("boundary_version")?),
        point: lon.zip(lat).map(|(x, y)| [x, y]),
    })
}

/// Cues in `[from, to)` ms, from the day files and SQLite (a row in both
/// while a rollup deletes counts once), in time order.
pub async fn cues(ctx: &Ctx, whose: Whose<'_>, from: i64, to: i64) -> anyhow::Result<Vec<Cue>> {
    let mut out = Vec::new();
    if from >= to || matches!(whose, Whose::Animals(a) if a.is_empty()) {
        return Ok(out);
    }
    let (first, last) = (date_of(from), date_of(to - 1));
    let owned = Arc::new(whose.owned());
    for (date, path) in list_days(ctx.data_dir(), "cues") {
        if date < first || date > last {
            continue;
        }
        let w = owned.clone();
        let mut got = tokio::task::spawn_blocking(move || -> anyhow::Result<Vec<Cue>> {
            let mut v = Vec::new();
            cold_cues(&path, &w, from, to, &mut v)?;
            Ok(v)
        })
        .await??;
        out.append(&mut got);
    }
    let seen: HashSet<i64> = out.iter().map(|c| c.id).collect();
    let base = format!("SELECT {CUE_COLS} FROM cues WHERE t >= ? AND t < ?");
    let rows = match whose {
        Whose::Animal(a) => sqlx::query(&format!("{base} AND animal_id = ?")).bind(from).bind(to).bind(a).fetch_all(ctx.db()).await?,
        Whose::Herd(h) => sqlx::query(&format!("{base} AND herd_id = ?")).bind(from).bind(to).bind(h).fetch_all(ctx.db()).await?,
        Whose::All => sqlx::query(&base).bind(from).bind(to).fetch_all(ctx.db()).await?,
        Whose::Animals(a) => {
            let mut rows = Vec::new();
            for chunk in a.chunks(400) {
                let sql = format!("{base} AND animal_id IN ({})", vec!["?"; chunk.len()].join(","));
                let mut q = sqlx::query(&sql).bind(from).bind(to);
                for id in chunk {
                    q = q.bind(id);
                }
                rows.extend(q.fetch_all(ctx.db()).await?);
            }
            rows
        }
    };
    for r in &rows {
        let c = cue_from_row(r)?;
        if !seen.contains(&c.id) {
            out.push(c);
        }
    }
    out.sort_by_key(|c| (c.t, c.id));
    Ok(out)
}

// ---------------------------------------------------------------- episodes

/// An episode, reported by the collar or derived by the server.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Episode {
    pub id: String,
    pub collar_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub animal_id: Option<String>,
    #[serde(with = "ms_time")]
    pub start: i64,
    #[serde(with = "ms_time")]
    pub end: i64,
    pub ring: u32,
    pub cues: u32,
    pub max_level: u8,
    pub min_margin_m: f64,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary_version: Option<u32>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub derived: bool,
}

impl Episode {
    pub fn secs(&self) -> f64 {
        (self.end - self.start).max(0) as f64 / 1000.0
    }
}

/// Unix ms as an RFC 3339 time, like every other time in the API.
mod ms_time {
    use serde::Serialize;

    pub fn serialize<S: serde::Serializer>(ms: &i64, s: S) -> Result<S::Ok, S::Error> {
        op_core::time::from_unix_ms(*ms).serialize(s)
    }
}

const EP_COLS: &str = "id, collar_id, herd_id, animal_id, start_t, end_t, ring, cues, max_level, min_margin_m, outcome, boundary_version, derived";

fn episode_from_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<Episode> {
    Ok(Episode {
        id: r.try_get("id")?,
        collar_id: r.try_get("collar_id")?,
        herd_id: r.try_get("herd_id")?,
        animal_id: r.try_get("animal_id")?,
        start: r.try_get("start_t")?,
        end: r.try_get("end_t")?,
        ring: r.try_get::<i64, _>("ring")?.max(0) as u32,
        cues: r.try_get::<i64, _>("cues")?.max(0) as u32,
        max_level: r.try_get::<i64, _>("max_level")?.clamp(0, 255) as u8,
        min_margin_m: r.try_get("min_margin_m")?,
        outcome: r.try_get("outcome")?,
        boundary_version: u32_of(r.try_get("boundary_version")?),
        derived: r.try_get::<i64, _>("derived")? != 0,
    })
}

/// Episodes that overlap `[from, to)` ms, by start.
pub async fn episodes(ctx: &Ctx, whose: Whose<'_>, from: i64, to: i64) -> anyhow::Result<Vec<Episode>> {
    let base = format!("SELECT {EP_COLS} FROM episodes WHERE start_t < ? AND end_t >= ?");
    let rows = match whose {
        Whose::Animal(a) => sqlx::query(&format!("{base} AND animal_id = ?")).bind(to).bind(from).bind(a).fetch_all(ctx.db()).await?,
        Whose::Herd(h) => sqlx::query(&format!("{base} AND herd_id = ?")).bind(to).bind(from).bind(h).fetch_all(ctx.db()).await?,
        Whose::All => sqlx::query(&base).bind(to).bind(from).fetch_all(ctx.db()).await?,
        Whose::Animals(a) => {
            let mut rows = Vec::new();
            for chunk in a.chunks(400) {
                let sql = format!("{base} AND animal_id IN ({})", vec!["?"; chunk.len()].join(","));
                let mut q = sqlx::query(&sql).bind(to).bind(from);
                for id in chunk {
                    q = q.bind(id);
                }
                rows.extend(q.fetch_all(ctx.db()).await?);
            }
            rows
        }
    };
    let mut out = rows.iter().map(episode_from_row).collect::<anyhow::Result<Vec<_>>>()?;
    out.sort_by(|a, b| (a.start, &a.id).cmp(&(b.start, &b.id)));
    Ok(out)
}

// ---------------------------------------------------------------- learning

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Trained,
    Learning,
}

impl Status {
    pub fn word(self) -> &'static str {
        match self {
            Self::Trained => "trained",
            Self::Learning => "learning",
        }
    }
}

/// Episodes by how they ended.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct Outcomes {
    pub turned_back: u32,
    pub crossed: u32,
    pub rest: u32,
    pub boundary_changed: u32,
}

impl Outcomes {
    pub fn add(&mut self, outcome: &str) {
        match outcome {
            "turned_back" => self.turned_back += 1,
            "crossed" => self.crossed += 1,
            "rest" => self.rest += 1,
            _ => self.boundary_changed += 1,
        }
    }

    pub fn total(&self) -> u32 {
        self.turned_back + self.crossed + self.rest + self.boundary_changed
    }
}

/// One episode as learning status needs it.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ep {
    pub start: i64,
    pub end: i64,
    pub crossed: bool,
    pub turned_back: bool,
    pub outcome: Outcome,
    pub derived: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    TurnedBack,
    Crossed,
    Rest,
    BoundaryChanged,
}

impl Outcome {
    pub fn parse(s: &str) -> Self {
        match s {
            "turned_back" => Self::TurnedBack,
            "crossed" => Self::Crossed,
            "rest" => Self::Rest,
            _ => Self::BoundaryChanged,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::TurnedBack => "turned_back",
            Self::Crossed => "crossed",
            Self::Rest => "rest",
            Self::BoundaryChanged => "boundary_changed",
        }
    }
}

impl Ep {
    pub fn new(start: i64, end: i64, outcome: &str, derived: bool) -> Self {
        let o = Outcome::parse(outcome);
        Self { start, end, crossed: o == Outcome::Crossed, turned_back: o == Outcome::TurnedBack, outcome: o, derived }
    }
}

/// Every episode of these animals, in time order: what learning status is
/// computed from (read from the `episodes_animal` index alone).
pub async fn history(ctx: &Ctx, animals: &[String]) -> anyhow::Result<HashMap<String, Vec<Ep>>> {
    let mut out: HashMap<String, Vec<Ep>> = HashMap::new();
    for chunk in animals.chunks(400) {
        let sql = format!(
            "SELECT animal_id, start_t, end_t, outcome, derived FROM episodes WHERE animal_id IN ({}) ORDER BY animal_id, start_t",
            vec!["?"; chunk.len()].join(",")
        );
        let mut q = sqlx::query(&sql);
        for id in chunk {
            q = q.bind(id);
        }
        for r in q.fetch_all(ctx.db()).await? {
            let a: String = r.try_get(0)?;
            let outcome: String = r.try_get(3)?;
            out.entry(a).or_default().push(Ep::new(r.try_get(1)?, r.try_get(2)?, &outcome, r.try_get::<i64, _>(4)? != 0));
        }
    }
    Ok(out)
}

/// Where an animal stands: trained after `trained_after` turned-back
/// episodes in a row with no crossing (rest and boundary changes neither
/// count nor break the run), learning once it has any episode, nothing
/// without episodes.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Learning {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<Status>,
    /// When the status began: the episode that made it trained; for
    /// learning, the crossing that ended being trained, else its first episode.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub since: Option<DateTime<Utc>>,
    /// Turned-back episodes since the last crossing.
    pub streak: u32,
    pub outcomes: Outcomes,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_episode_at: Option<DateTime<Utc>>,
    /// Some episodes were rebuilt from fixes (firmware 0.1).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub derived: bool,
}

/// Learning status from episodes in time order, counting those that began
/// before `until` (all of them when `None`).
pub fn learning(eps: &[Ep], trained_after: u32, until: Option<i64>) -> Learning {
    let n = trained_after.max(1) as usize;
    let mut l = Learning::default();
    let mut run: Vec<i64> = Vec::new();
    let mut learning_since: Option<i64> = None;
    for e in eps.iter().filter(|e| until.is_none_or(|u| e.start < u)) {
        l.outcomes.add(e.outcome.as_str());
        l.derived |= e.derived;
        learning_since.get_or_insert(e.start);
        l.last_episode_at = Some(op_core::time::from_unix_ms(e.end.max(e.start)));
        if e.crossed {
            if run.len() >= n {
                learning_since = Some(e.start);
            }
            run.clear();
        } else if e.turned_back {
            run.push(e.end);
        }
    }
    l.streak = run.len() as u32;
    if run.len() >= n {
        l.status = Some(Status::Trained);
        l.since = Some(op_core::time::from_unix_ms(run[n - 1]));
    } else if let Some(s) = learning_since {
        l.status = Some(Status::Learning);
        l.since = Some(op_core::time::from_unix_ms(s));
    }
    l
}

// ---------------------------------------------------------------- farm days

/// The farm's time zone (UTC when it has none).
pub async fn farm_tz(ctx: &Ctx) -> anyhow::Result<Tz> {
    let tz = ctx.store().get_farm().await?.map(|f| f.timezone).unwrap_or_default();
    Ok(tz.parse::<Tz>().unwrap_or_else(|_| "+00:00".parse().expect("utc")))
}

pub fn local_date(tz: &Tz, ms: i64) -> NaiveDate {
    op_core::time::from_unix_ms(ms).with_timezone(tz).date_naive()
}

/// Farm-local midnight starting `day`, in UTC.
pub fn midnight(tz: &Tz, day: NaiveDate) -> DateTime<Utc> {
    let naive = day.and_hms_opt(0, 0, 0).expect("midnight");
    match tz.from_local_datetime(&naive) {
        chrono::LocalResult::Single(t) | chrono::LocalResult::Ambiguous(t, _) => t.with_timezone(&Utc),
        chrono::LocalResult::None => tz.from_local_datetime(&(naive + Duration::hours(1))).earliest().map(|t| t.with_timezone(&Utc)).unwrap_or(naive.and_utc()),
    }
}

/// One farm day of one animal (or a herd).
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Day {
    pub date: NaiveDate,
    pub warn: u32,
    pub outside: u32,
    /// Seconds of tone played.
    pub tone_s: f64,
    /// Episodes that began that day.
    pub episodes: u32,
    /// The longest of them, seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub longest_s: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_level: Option<u8>,
}

impl Day {
    fn add_cue(&mut self, c: &Cue) {
        if c.kind == "outside" {
            self.outside += 1;
        } else {
            self.warn += 1;
        }
        self.tone_s += c.tone_ms() as f64 / 1000.0;
        self.max_level = Some(self.max_level.map_or(c.level, |m| m.max(c.level)));
    }

    fn add_episode(&mut self, e: &Episode) {
        self.episodes += 1;
        let s = e.secs();
        self.longest_s = Some(self.longest_s.map_or(s, |m| m.max(s)));
    }

    /// Seconds rounded to tenths.
    pub fn round(mut self) -> Self {
        self.tone_s = (self.tone_s * 10.0).round() / 10.0;
        self.longest_s = self.longest_s.map(|s| (s * 10.0).round() / 10.0);
        self
    }
}

/// Every farm day that `[from, to)` touches, oldest first, with the cues and
/// the episodes that began in it.
pub fn days(tz: &Tz, from: i64, to: i64, cues: &[Cue], eps: &[Episode]) -> Vec<Day> {
    let (first, last) = (local_date(tz, from), local_date(tz, (to - 1).max(from)));
    let mut by: BTreeMap<NaiveDate, Day> = BTreeMap::new();
    let mut d = first;
    while d <= last {
        by.insert(d, Day { date: d, ..Default::default() });
        d = d.succ_opt().unwrap_or(last + Duration::days(1));
    }
    for c in cues.iter().filter(|c| c.t >= from && c.t < to) {
        if let Some(day) = by.get_mut(&local_date(tz, c.t)) {
            day.add_cue(c);
        }
    }
    for e in eps.iter().filter(|e| e.start >= from && e.start < to) {
        if let Some(day) = by.get_mut(&local_date(tz, e.start)) {
            day.add_episode(e);
        }
    }
    by.into_values().map(Day::round).collect()
}

// ---------------------------------------------------------------- ledger

/// A cue in the ledger, with how its episode ended.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct LedgerRow {
    pub at: DateTime<Utc>,
    pub kind: &'static str,
    pub level: u8,
    /// Tone length counted, ms (firmware 0.1 doesn't report it: its beep).
    pub tone_ms: u32,
    /// The length the collar reported, when it did.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dur_ms: Option<u32>,
    pub margin_m: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ring: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub boundary_version: Option<u32>,
    pub collar_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub episode_id: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub derived: bool,
}

/// The outside tone plays for up to this long after a crossing.
const OUTSIDE_TONE_MS: i64 = 10_000;

/// The episode a cue belongs to: the one of its collar running at its time,
/// or, for the outside tone, the crossing it followed.
pub fn episode_of<'a>(c: &Cue, by_collar: &HashMap<&str, Vec<&'a Episode>>) -> Option<&'a Episode> {
    let eps = by_collar.get(c.collar_id.as_str())?;
    let i = eps.partition_point(|e| e.start <= c.t);
    let before = eps[..i].iter().rev().take(3);
    let mut hit = None;
    for e in before {
        if c.t <= e.end {
            return Some(e);
        }
        if hit.is_none() && c.kind == "outside" && e.outcome == "crossed" && c.t - e.end <= OUTSIDE_TONE_MS {
            hit = Some(*e);
        }
    }
    hit
}

pub fn by_collar(eps: &[Episode]) -> HashMap<&str, Vec<&Episode>> {
    let mut m: HashMap<&str, Vec<&Episode>> = HashMap::new();
    for e in eps {
        m.entry(e.collar_id.as_str()).or_default().push(e);
    }
    for v in m.values_mut() {
        v.sort_by_key(|e| e.start);
    }
    m
}

/// Ledger rows, newest first.
pub fn ledger(cues: &[Cue], eps: &[Episode]) -> Vec<LedgerRow> {
    let idx = by_collar(eps);
    let mut rows: Vec<LedgerRow> = cues
        .iter()
        .map(|c| {
            let e = episode_of(c, &idx);
            LedgerRow {
                at: op_core::time::from_unix_ms(c.t),
                kind: c.kind,
                level: c.level,
                tone_ms: c.tone_ms(),
                dur_ms: c.dur_ms,
                margin_m: c.margin_m,
                ring: c.ring.or(if c.reported { None } else { Some(0) }),
                boundary_version: c.boundary_version,
                collar_id: c.collar_id.clone(),
                outcome: e.map(|e| e.outcome.clone()),
                episode_id: e.map(|e| e.id.clone()),
                derived: e.is_some_and(|e| e.derived),
            }
        })
        .collect();
    rows.reverse();
    rows
}

// ---------------------------------------------------------------- map ticks

/// Side of a tick's cell, metres.
pub const TICK_M: f64 = 2.0;

/// Cues with a position binned into 2 m cells: `[lon, lat, warn, outside]` at
/// each cell's centre, most cued first, and the degrees a cell spans.
pub fn ticks(cues: &[Cue], origin: LonLat) -> (Vec<[f64; 4]>, [f64; 2]) {
    let proj = Projection::new(origin);
    let mut cells: HashMap<(i64, i64), (u32, u32)> = HashMap::new();
    for c in cues {
        let Some(p) = c.point else { continue };
        let [x, y] = proj.forward(p);
        let e = cells.entry(((x / TICK_M).floor() as i64, (y / TICK_M).floor() as i64)).or_default();
        if c.kind == "outside" {
            e.1 += 1;
        } else {
            e.0 += 1;
        }
    }
    let r = |v: f64| (v * 1e7).round() / 1e7;
    let mut out: Vec<[f64; 4]> = cells
        .into_iter()
        .map(|((cx, cy), (w, o))| {
            let p = proj.inverse([(cx as f64 + 0.5) * TICK_M, (cy as f64 + 0.5) * TICK_M]);
            [r(p[0]), r(p[1]), w as f64, o as f64]
        })
        .collect();
    out.sort_by(|a, b| (b[2] + b[3]).total_cmp(&(a[2] + a[3])).then(a[0].total_cmp(&b[0])).then(a[1].total_cmp(&b[1])));
    let size = [TICK_M / proj.m_per_deg_lon, proj.offset(0.0, TICK_M)[1] - proj.lat0];
    (out, size)
}

// ---------------------------------------------------------------- collar care

/// A fit check of a collar the animal wore.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct FitCheck {
    pub collar_id: String,
    pub checked_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub by: Option<Actor>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// Fit checks of these collars in `[from, to)` (all when `None`), newest first.
pub async fn fit_checks(ctx: &Ctx, collars: &[String], from: Option<DateTime<Utc>>, to: Option<DateTime<Utc>>) -> anyhow::Result<Vec<FitCheck>> {
    let mut out = Vec::new();
    for c in collars {
        let rows = sqlx::query_as::<_, (String, Option<String>, Option<String>)>(
            "SELECT checked_at, \"by\", notes FROM collar_fit_checks WHERE collar_id = ? ORDER BY checked_at DESC, id DESC",
        )
        .bind(c)
        .fetch_all(ctx.db())
        .await?;
        for (at, by, notes) in rows {
            let checked_at = op_core::time::from_db(&at)?;
            if from.is_some_and(|f| checked_at < f) || to.is_some_and(|t| checked_at >= t) {
                continue;
            }
            out.push(FitCheck { collar_id: c.clone(), checked_at, by: by.and_then(|b| serde_json::from_str(&b).ok()), notes });
        }
    }
    out.sort_by(|a, b| b.checked_at.cmp(&a.checked_at));
    Ok(out)
}

/// A time the collar lay still long enough to raise a `drop_off` alert (it
/// may have come off). `to` is absent while it lasts.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StillSpell {
    pub from: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub to: Option<DateTime<Utc>>,
}

/// `drop_off` alerts on this animal or these collars overlapping
/// `[from, to)`, overlapping ones merged, oldest first.
pub async fn still_spells(ctx: &Ctx, animal_id: &str, collars: &[String], from: DateTime<Utc>, to: DateTime<Utc>) -> anyhow::Result<Vec<StillSpell>> {
    let spells = still_spells_all(ctx, from, to).await?;
    Ok(spells_for(&spells, animal_id, collars))
}

/// Every `drop_off` alert overlapping `[from, to)` with its targets.
pub async fn still_spells_all(ctx: &Ctx, from: DateTime<Utc>, to: DateTime<Utc>) -> anyhow::Result<Vec<(Vec<(String, String)>, StillSpell)>> {
    let rows = sqlx::query_as::<_, (String, String, Option<String>)>(
        "SELECT targets, opened_at, resolved_at FROM alerts WHERE kind = 'drop_off' AND opened_at < ? AND (resolved_at IS NULL OR resolved_at >= ?) ORDER BY opened_at",
    )
    .bind(op_core::time::to_db(&to))
    .bind(op_core::time::to_db(&from))
    .fetch_all(ctx.db())
    .await?;
    let mut out = Vec::with_capacity(rows.len());
    for (targets, opened, resolved) in rows {
        let t: Vec<(String, String)> = serde_json::from_str(&targets).unwrap_or_default();
        out.push((t, StillSpell { from: op_core::time::from_db(&opened)?, to: op_core::time::opt_from_db(resolved)? }));
    }
    Ok(out)
}

/// The spells that name this animal or one of these collars, merged.
pub fn spells_for(all: &[(Vec<(String, String)>, StillSpell)], animal_id: &str, collars: &[String]) -> Vec<StillSpell> {
    let mut mine: Vec<StillSpell> = all
        .iter()
        .filter(|(t, _)| t.iter().any(|(k, id)| (k == "animal" && id == animal_id) || (k == "collar" && collars.contains(id))))
        .map(|(_, s)| s.clone())
        .collect();
    mine.sort_by_key(|s| s.from);
    let mut out: Vec<StillSpell> = Vec::new();
    for s in mine {
        match out.last_mut() {
            Some(l) if l.to.is_none_or(|e| s.from <= e) => {
                l.to = match (l.to, s.to) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    _ => None,
                };
            }
            _ => out.push(s),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ep(start: i64, outcome: &str) -> Ep {
        Ep::new(start, start + 5000, outcome, false)
    }

    #[test]
    fn trained_after_n_turned_back_in_a_row_and_reset_by_a_crossing() {
        let mut eps: Vec<Ep> = (0..4).map(|i| ep(i * 60_000, "turned_back")).collect();
        let l = learning(&eps, 5, None);
        assert_eq!((l.status, l.streak), (Some(Status::Learning), 4));
        assert_eq!(l.since, Some(op_core::time::from_unix_ms(0)));
        // A rest neither counts nor breaks the run.
        eps.push(ep(300_000, "rest"));
        eps.push(ep(360_000, "turned_back"));
        let l = learning(&eps, 5, None);
        assert_eq!((l.status, l.streak), (Some(Status::Trained), 5));
        assert_eq!(l.since, Some(op_core::time::from_unix_ms(365_000)), "the fifth turned back made it");
        assert_eq!(l.outcomes, Outcomes { turned_back: 5, crossed: 0, rest: 1, boundary_changed: 0 });
        // A crossing takes it back to learning, from then.
        eps.push(ep(900_000, "crossed"));
        let l = learning(&eps, 5, None);
        assert_eq!((l.status, l.streak), (Some(Status::Learning), 0));
        assert_eq!(l.since, Some(op_core::time::from_unix_ms(900_000)));
        // As of before the crossing it was trained.
        assert_eq!(learning(&eps, 5, Some(900_000)).status, Some(Status::Trained));
        // Fewer needed.
        assert_eq!(learning(&eps[..3], 3, None).status, Some(Status::Trained));
        // Nothing without episodes.
        assert_eq!(learning(&[], 5, None), Learning::default());
    }

    #[test]
    fn a_crossing_before_being_trained_keeps_learning_since_the_first_episode() {
        let eps = [ep(0, "turned_back"), ep(60_000, "crossed"), ep(120_000, "turned_back")];
        let l = learning(&eps, 5, None);
        assert_eq!((l.status, l.streak), (Some(Status::Learning), 1));
        assert_eq!(l.since, Some(op_core::time::from_unix_ms(0)));
    }

    fn cue(id: i64, collar: &str, t: i64, kind: &'static str) -> Cue {
        Cue {
            id,
            collar_id: collar.into(),
            herd_id: None,
            animal_id: Some("a".into()),
            t,
            level: 2,
            margin_m: if kind == "outside" { -1.0 } else { 2.0 },
            kind,
            reported: true,
            ring: Some(0),
            dur_ms: Some(300),
            boundary_version: Some(3),
            point: None,
        }
    }

    fn episode(id: &str, collar: &str, start: i64, end: i64, outcome: &str) -> Episode {
        Episode {
            id: id.into(),
            collar_id: collar.into(),
            herd_id: None,
            animal_id: Some("a".into()),
            start,
            end,
            ring: 0,
            cues: 2,
            max_level: 3,
            min_margin_m: 0.5,
            outcome: outcome.into(),
            boundary_version: Some(3),
            derived: false,
        }
    }

    #[test]
    fn cues_join_the_episode_they_were_played_in() {
        let eps = [episode("e1", "c1", 1000, 3000, "turned_back"), episode("e2", "c1", 10_000, 12_000, "crossed")];
        let cues = [
            cue(1, "c1", 1000, "warn"),
            cue(2, "c1", 2000, "warn"),
            cue(3, "c1", 12_000, "outside"),
            cue(4, "c1", 15_000, "outside"),
            cue(5, "c1", 40_000, "warn"),
        ];
        let rows = ledger(&cues, &eps);
        let outcomes: Vec<Option<&str>> = rows.iter().rev().map(|r| r.outcome.as_deref()).collect();
        assert_eq!(outcomes, [Some("turned_back"), Some("turned_back"), Some("crossed"), Some("crossed"), None]);
        assert_eq!(rows[0].at, op_core::time::from_unix_ms(40_000), "newest first");
    }

    #[test]
    fn farm_days_count_cues_tone_and_the_longest_episode() {
        let tz: Tz = "America/Chicago".parse().unwrap();
        // 2026-09-27 00:00 Chicago = 05:00 UTC.
        let midnight_ms = midnight(&tz, NaiveDate::from_ymd_opt(2026, 9, 27).unwrap()).timestamp_millis();
        assert_eq!(op_core::time::to_db(&op_core::time::from_unix_ms(midnight_ms)), "2026-09-27T05:00:00.000Z");
        let mut legacy = cue(3, "c1", midnight_ms + 1000, "warn");
        legacy.dur_ms = None;
        let cues = [cue(1, "c1", midnight_ms - 1000, "warn"), cue(2, "c1", midnight_ms + 500, "outside"), legacy];
        let eps = [
            episode("e1", "c1", midnight_ms + 100, midnight_ms + 12_600, "crossed"),
            episode("e2", "c1", midnight_ms + 60_000, midnight_ms + 64_000, "turned_back"),
        ];
        let d = days(&tz, midnight_ms - 3_600_000, midnight_ms + 3_600_000, &cues, &eps);
        assert_eq!(d.len(), 2);
        assert_eq!((d[0].warn, d[0].outside, d[0].tone_s, d[0].episodes), (1, 0, 0.3, 0));
        assert_eq!((d[1].warn, d[1].outside, d[1].tone_s, d[1].episodes, d[1].longest_s), (1, 1, 0.6, 2, Some(12.5)));
    }

    #[test]
    fn spells_merge_when_they_overlap() {
        let t = |m: i64| op_core::time::from_unix_ms(m * 60_000);
        let a = |k: &str, id: &str| vec![(k.to_owned(), id.to_owned())];
        let all = vec![
            (a("collar", "c1"), StillSpell { from: t(0), to: Some(t(30)) }),
            (a("animal", "x"), StillSpell { from: t(20), to: Some(t(50)) }),
            (a("collar", "c2"), StillSpell { from: t(5), to: None }),
            (a("collar", "c1"), StillSpell { from: t(100), to: None }),
        ];
        let got = spells_for(&all, "x", &["c1".to_owned()]);
        assert_eq!(got, vec![StillSpell { from: t(0), to: Some(t(50)) }, StillSpell { from: t(100), to: None }]);
    }
}
