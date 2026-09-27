//! Welfare record (field-ready H): what the collars played to each animal and
//! how it learned the boundary.
//!
//! - The cue ledger: every cue (time, kind, level, tone length, margin, ring,
//!   boundary version) with how its episode ended. Kinds are `warn` and
//!   `outside` only; the collars are audio only.
//! - Episodes: those collars report (firmware 0.2), and those the server
//!   rebuilds from cues and fixes for collars that don't (firmware 0.1,
//!   [`derive`]), marked `derived`.
//! - Learning status: trained after `trained_after` turned-back episodes in a
//!   row with no crossing, learning once it has any episode.
//! - Per farm day: cues by kind, seconds of tone, episodes and the longest.
//! - Fit checks and times the collar lay still (`drop_off` alerts).
//! - Training mode per herd ([`training`]): a wider warning zone for boundaries
//!   sent while it is on (op-ingest's `default_margins` reads it).
//!
//! Routes: `GET /api/welfare/animals?herd_id=`,
//! `GET /api/welfare/animals/{animal_id}/cues?from=&to=`,
//! `GET /api/welfare/cues/points?herd_id=&from=&to=`,
//! `GET/PUT /api/welfare/training/{herd_id}`. MCP `get_welfare`.

mod derive;
mod record;
pub mod training;

use std::collections::{BTreeSet, HashMap};
use std::time::Duration as StdDuration;

use axum::Json;
use axum::extract::{Path, Query, State};
use axum::routing::get;
use chrono::{DateTime, Duration, Utc};
use op_core::tools::{ToolCall, ToolSpec};
use op_core::{ApiError, ApiJson, ApiResult, Ctx, Role};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub use derive::{Derived, LEGACY_BEEP_MS, LegacyCue, Pass, Run, Step, StepCue, derive, episodes as track_episodes, steps};
pub use record::{
    Cue, Day, Ep, Episode, FitCheck, Learning, LedgerRow, Outcome, Outcomes, Status, StillSpell, TICK_M, Whose, by_collar, cues, days, episode_of, episodes,
    farm_tz, fit_checks, history, learning, ledger, local_date, midnight, spells_for, still_spells, still_spells_all, ticks,
};
pub use training::{TRAINING_KEY, Training, TrainingPatch};

use crate::range::TimeRange;

/// Episode ids (`epi_…`, as op-ingest's).
pub const EPISODE: &str = "epi";
/// Ledger rows one animal's record returns; older ones are counted in `truncated`.
pub const MAX_LEDGER: usize = 2000;
/// Map ticks returned; the most cued cells first.
pub const MAX_TICKS: usize = 5000;

// ---------------------------------------------------------------- task

const FIRST_AFTER: StdDuration = StdDuration::from_secs(20);
const EVERY: StdDuration = StdDuration::from_secs(30);

/// The background task that rebuilds firmware 0.1 episodes, every 30 s.
pub fn spawn(ctx: Ctx) {
    tokio::spawn(async move {
        let mut wait = FIRST_AFTER;
        loop {
            tokio::select! {
                _ = ctx.on_shutdown() => break,
                _ = tokio::time::sleep(wait) => {}
            }
            match derive(&ctx, op_core::time::now().timestamp_millis()).await {
                Ok(p) if p.episodes > 0 => tracing::debug!(collars = p.collars, episodes = p.episodes, waiting = p.waiting, "welfare: episodes rebuilt"),
                Ok(_) => {}
                Err(e) => tracing::warn!("welfare: {e:#}"),
            }
            wait = EVERY;
        }
    });
}

// ---------------------------------------------------------------- a herd

/// One animal's standing.
#[derive(Debug, Clone, Serialize)]
pub struct AnimalRow {
    pub animal_id: String,
    pub tag: String,
    pub herd_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
    #[serde(flatten)]
    pub learning: Learning,
}

#[derive(Debug, Clone, Serialize)]
pub struct HerdWelfare {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub herd_id: Option<String>,
    /// The herd's training mode (only when one herd is asked for).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub training: Option<Training>,
    /// Animals in the herd (not removed).
    pub head: u32,
    pub trained: u32,
    pub learning: u32,
    pub animals: Vec<AnimalRow>,
}

/// Learning status of every animal in a herd (every herd when `None`), now.
pub async fn herd(ctx: &Ctx, herd_id: Option<&str>) -> ApiResult<HerdWelfare> {
    if let Some(h) = herd_id {
        if ctx.store().get_herd(h).await?.is_none() {
            return Err(ApiError::not_found("No such herd."));
        }
    }
    let animals: Vec<op_core::Animal> = ctx.store().list_animals(herd_id).await?.into_iter().filter(|a| a.removed_at.is_none()).collect();
    let trainings = training::all(ctx).await?;
    let ids: Vec<String> = animals.iter().map(|a| a.id.clone()).collect();
    let mut hist = history(ctx, &ids).await?;
    let mut rows = Vec::with_capacity(animals.len());
    let (mut trained, mut learning_n) = (0, 0);
    for a in animals {
        let n = trainings.get(&a.herd_id).copied().unwrap_or_default().trained_after;
        let l = learning(&hist.remove(&a.id).unwrap_or_default(), n, None);
        match l.status {
            Some(Status::Trained) => trained += 1,
            Some(Status::Learning) => learning_n += 1,
            None => {}
        }
        rows.push(AnimalRow { animal_id: a.id, tag: a.tag, herd_id: a.herd_id, collar_id: a.collar_id, learning: l });
    }
    rows.sort_by(|a, b| tag_order(&a.tag, &b.tag));
    let training = herd_id.map(|h| trainings.get(h).copied().unwrap_or_default());
    Ok(HerdWelfare { herd_id: herd_id.map(str::to_owned), training, head: rows.len() as u32, trained, learning: learning_n, animals: rows })
}

/// Tags in the order people read them: numbers by value, then the rest.
pub fn tag_order(a: &str, b: &str) -> std::cmp::Ordering {
    match (a.trim().parse::<u64>(), b.trim().parse::<u64>()) {
        (Ok(x), Ok(y)) => x.cmp(&y).then_with(|| a.cmp(b)),
        (Ok(_), Err(_)) => std::cmp::Ordering::Less,
        (Err(_), Ok(_)) => std::cmp::Ordering::Greater,
        _ => a.cmp(b),
    }
}

// ---------------------------------------------------------------- one animal

#[derive(Debug, Clone, Serialize)]
pub struct AnimalWelfare {
    pub animal_id: String,
    pub tag: String,
    pub herd_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
    /// Turned-back episodes in a row that make an animal of its herd trained.
    pub trained_after: u32,
    /// Where it stands now, from all its episodes.
    pub learning: Learning,
    /// Farm days in the range, oldest first.
    pub days: Vec<Day>,
    /// Episodes in the range, newest first.
    pub episodes: Vec<Episode>,
    /// The cue ledger in the range, newest first.
    pub cues: Vec<LedgerRow>,
    /// Older cues left out of `cues`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<usize>,
    /// Fit checks of the collars it wore, newest first.
    pub fit_checks: Vec<FitCheck>,
    /// Times its collar lay still long enough to raise a `drop_off` alert.
    pub drop_offs: Vec<StillSpell>,
}

/// One animal's record over a range.
pub async fn animal(ctx: &Ctx, animal_id: &str, range: TimeRange, max_cues: usize) -> ApiResult<AnimalWelfare> {
    let Some(a) = ctx.store().get_animal(animal_id).await? else {
        return Err(ApiError::not_found("No such animal."));
    };
    let (from, to) = (range.from_ms(), range.to_ms());
    let tz = farm_tz(ctx).await?;
    let cue_rows = cues(ctx, Whose::Animal(animal_id), from, to).await?;
    let eps = episodes(ctx, Whose::Animal(animal_id), from, to).await?;
    let n = training::of(ctx, &a.herd_id).await?.trained_after;
    let hist = history(ctx, std::slice::from_ref(&a.id)).await?;
    let l = learning(hist.get(&a.id).map(Vec::as_slice).unwrap_or_default(), n, None);
    let day_rows = days(&tz, from, to, &cue_rows, &eps);
    let mut collars: BTreeSet<String> = cue_rows.iter().map(|c| c.collar_id.clone()).chain(eps.iter().map(|e| e.collar_id.clone())).collect();
    collars.extend(a.collar_id.clone());
    let collars: Vec<String> = collars.into_iter().collect();
    let checks = fit_checks(ctx, &collars, None, None).await?;
    let drops = still_spells(ctx, &a.id, &collars, range.from, range.to).await?;
    let mut ledger_rows = ledger(&cue_rows, &eps);
    let truncated = (ledger_rows.len() > max_cues).then(|| ledger_rows.len() - max_cues);
    ledger_rows.truncate(max_cues);
    let mut eps = eps;
    eps.reverse();
    Ok(AnimalWelfare {
        animal_id: a.id,
        tag: a.tag,
        herd_id: a.herd_id,
        collar_id: a.collar_id,
        from: range.from,
        to: range.to,
        trained_after: n,
        learning: l,
        days: day_rows,
        episodes: eps,
        cues: ledger_rows,
        truncated,
        fit_checks: checks,
        drop_offs: drops,
    })
}

// ---------------------------------------------------------------- map ticks

#[derive(Debug, Clone, Serialize)]
pub struct CuePoints {
    pub cell_m: f64,
    /// Degrees of longitude and latitude a cell spans (absent with no ticks).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<[f64; 2]>,
    /// `[lon, lat, warn, outside]` at each cell's centre, most cued first.
    pub ticks: Vec<[f64; 4]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub truncated: Option<usize>,
}

/// Where cues fired: a herd's (every herd's when `None`) cues with a
/// position over a range, binned into 2 m cells.
pub async fn points(ctx: &Ctx, herd_id: Option<&str>, range: TimeRange) -> ApiResult<CuePoints> {
    let whose = match herd_id {
        Some(h) => Whose::Herd(h),
        None => Whose::All,
    };
    let rows = cues(ctx, whose, range.from_ms(), range.to_ms()).await?;
    let origin = match ctx.store().get_farm().await?.map(|f| f.center) {
        Some(c) => Some(c),
        None => rows.iter().find_map(|c| c.point),
    };
    let Some(origin) = origin else {
        return Ok(CuePoints { cell_m: TICK_M, size: None, ticks: Vec::new(), truncated: None });
    };
    let (mut t, size) = ticks(&rows, origin);
    let truncated = (t.len() > MAX_TICKS).then(|| t.len() - MAX_TICKS);
    t.truncate(MAX_TICKS);
    Ok(CuePoints { cell_m: TICK_M, size: (!t.is_empty()).then_some(size), ticks: t, truncated })
}

// ---------------------------------------------------------------- HTTP

pub fn router() -> axum::Router<Ctx> {
    axum::Router::new()
        .route("/api/welfare/animals", get(get_animals))
        .route("/api/welfare/animals/{animal_id}/cues", get(get_animal))
        .route("/api/welfare/cues/points", get(get_points))
        .route("/api/welfare/training/{herd_id}", get(get_training).put(put_training))
}

#[derive(Debug, Default, Deserialize)]
pub struct Params {
    pub herd_id: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
}

fn herd_param(p: &Params) -> Option<&str> {
    p.herd_id.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

async fn get_animals(State(ctx): State<Ctx>, Query(p): Query<Params>) -> ApiResult<Json<HerdWelfare>> {
    Ok(Json(herd(&ctx, herd_param(&p)).await?))
}

async fn get_animal(State(ctx): State<Ctx>, Path(animal_id): Path<String>, Query(p): Query<Params>) -> ApiResult<Json<AnimalWelfare>> {
    let range = TimeRange::parse(p.from.as_deref(), p.to.as_deref(), Duration::days(14), op_core::time::now())?;
    Ok(Json(animal(&ctx, &animal_id, range, MAX_LEDGER).await?))
}

async fn get_points(State(ctx): State<Ctx>, Query(p): Query<Params>) -> ApiResult<Json<CuePoints>> {
    let range = TimeRange::parse(p.from.as_deref(), p.to.as_deref(), Duration::days(7), op_core::time::now())?;
    if let Some(h) = herd_param(&p) {
        if ctx.store().get_herd(h).await?.is_none() {
            return Err(ApiError::not_found("No such herd."));
        }
    }
    Ok(Json(points(&ctx, herd_param(&p), range).await?))
}

/// A herd's training mode, with its id.
#[derive(Debug, Clone, Serialize)]
pub struct HerdTraining {
    pub herd_id: String,
    #[serde(flatten)]
    pub training: Training,
}

async fn get_training(State(ctx): State<Ctx>, Path(herd_id): Path<String>) -> ApiResult<Json<HerdTraining>> {
    if ctx.store().get_herd(&herd_id).await?.is_none() {
        return Err(ApiError::not_found("No such herd."));
    }
    let training = training::of(&ctx, &herd_id).await?;
    Ok(Json(HerdTraining { herd_id, training }))
}

async fn put_training(State(ctx): State<Ctx>, Path(herd_id): Path<String>, ApiJson(p): ApiJson<TrainingPatch>) -> ApiResult<Json<HerdTraining>> {
    let training = training::update(&ctx, &herd_id, &p).await?;
    Ok(Json(HerdTraining { herd_id, training }))
}

// ---------------------------------------------------------------- MCP

/// Rows of each list the tool returns for one animal.
const TOOL_ROWS: usize = 50;

pub fn tool() -> ToolSpec {
    ToolSpec {
        name: "get_welfare",
        description: "Welfare record from the collars, which are audio only. Cue kinds are warn (the warning tone near the boundary, level 1-4) and outside (a tone for up to 10 s after the animal crossed). An episode is a run of warning tones ending turned_back, crossed, rest (tones stopped after 20 s) or boundary_changed; derived episodes were rebuilt from fixes for firmware 0.1 collars. An animal is trained after trained_after turned_back episodes in a row with no crossing (5 unless its herd's training mode says otherwise), learning once it has any episode. Without animal_id: each animal's status of a herd (or every herd), with counts. With animal_id: that animal's status, farm days in the range (cues by kind, tone seconds, episodes and the longest), its newest 50 episodes and cues, fit checks and times its collar lay still. from/to are RFC 3339, dates or relative (-14d); default the last 14 days.",
        input_schema: json!({
            "type": "object",
            "properties": {
                "herd_id": { "type": "string", "description": "Only this herd's animals." },
                "animal_id": { "type": "string", "description": "One animal's record." },
                "from": { "type": "string", "description": "Start: RFC 3339, a date, or relative like -14d." },
                "to": { "type": "string", "description": "End (exclusive): RFC 3339, a date, or now." }
            },
            "required": [],
            "additionalProperties": false
        }),
        read: true,
        brain: false,
        min_role: Role::Viewer,
        run: ToolSpec::run_fn(|c: ToolCall| async move { tool_run(&c.ctx, &c.args).await }),
    }
}

async fn tool_run(ctx: &Ctx, args: &Value) -> ApiResult<Value> {
    if let Some(m) = args.as_object() {
        if let Some(k) = m.keys().find(|k| !["herd_id", "animal_id", "from", "to"].contains(&k.as_str())) {
            return Err(ApiError::bad_request(format!("Unknown argument `{k}`.")));
        }
    }
    let s = |k: &str| args.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
    let out = match s("animal_id") {
        Some(a) => {
            let range = TimeRange::parse(s("from"), s("to"), Duration::days(14), op_core::time::now())?;
            let mut w = animal(ctx, a, range, TOOL_ROWS).await?;
            let more = w.episodes.len().saturating_sub(TOOL_ROWS);
            w.episodes.truncate(TOOL_ROWS);
            let mut v = serde_json::to_value(&w).map_err(anyhow::Error::from)?;
            if more > 0 {
                v["episodes_truncated"] = json!(more);
            }
            v
        }
        None => serde_json::to_value(herd(ctx, s("herd_id")).await?).map_err(anyhow::Error::from)?,
    };
    Ok(out)
}

/// Collars named in cues or episodes, by animal: which collars an animal wore.
pub fn collars_by_animal(cues: &[Cue], eps: &[Episode]) -> HashMap<String, BTreeSet<String>> {
    let mut m: HashMap<String, BTreeSet<String>> = HashMap::new();
    for c in cues {
        if let Some(a) = &c.animal_id {
            m.entry(a.clone()).or_default().insert(c.collar_id.clone());
        }
    }
    for e in eps {
        if let Some(a) = &e.animal_id {
            m.entry(a.clone()).or_default().insert(e.collar_id.clone());
        }
    }
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_sort_as_people_read_them() {
        let mut v = vec!["118", "31", "A7", "214", "031", "B2"];
        v.sort_by(|a, b| tag_order(a, b));
        assert_eq!(v, ["031", "31", "118", "214", "A7", "B2"]);
    }
}
