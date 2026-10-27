//! Farm rules (docs/API.md "Farm rules"): plain-language rules the owner and
//! managers give the farm agent, e.g. "never put the heifers in the creek
//! paddock before July" or "always leave 30 m from the neighbour's bull
//! pasture". Each keeps who wrote, changed and removed it, and when.
//!
//! - Every decision prompt carries them (`farm_rules` in the decision
//!   context, beside the no-go zones), MCP agents read them
//!   (`list_farm_rules`, and in every write tool's description), and a
//!   decision says how it follows them (`rules` in the brain's answer).
//! - A rule that plainly keeps ground out (it names a paddock or a map
//!   feature and says never, keep out, stay N m from) offers to become a
//!   no-go zone, or an exclusion when it has dates ([`Suggestion`]). Then the
//!   gate enforces it for every sender.
//! - [`review`]: before a decision goes to the collars on its own (timer,
//!   auto), the safety gate's findings and the rules are checked. A rule read
//!   as ground and dates is checked on the map; any other goes to a second
//!   brain pass as reviewer. A hard finding, a broken rule or any doubt (the
//!   reviewer unsure or unable, no statement from an LLM brain) and the
//!   decision waits for a person, the reasons on it. Rule text never lets
//!   anything past the gate: it can only hold a decision back.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, RwLock};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::routing::{get, post};
use axum::{Json, Router};
use chrono::{DateTime, Datelike, NaiveDate, TimeZone, Utc};
use geo::{Area, BooleanOps, Coord, LineString, MultiPolygon};
use op_core::features::{FeatureGeometry, FeatureKind, MapFeature, NewFeature};
use op_core::time::{from_db, now, to_db};
use op_core::tools::{ToolCall, ToolSpec};
use op_core::{
    Actor, ApiError, ApiJson, ApiResult, BrainId, Ctx, DbEnum, Decision, DecisionAction, DecisionSource, Event, Herd, Identity, LonLat, Paddock, Polygon, Role,
    id,
};
use op_geo::Projection;
use op_ingest::gate::{self, Sender, SenderKind};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;

/// `rul_…`
pub const RULE: &str = "rul";
/// Longest rule, in characters.
pub const MAX_TEXT: usize = 500;
/// Most rules a farm keeps at once.
pub const MAX_RULES: usize = 100;
/// Overlaps smaller than this (m²) are rounding.
const OVERLAP_M2: f64 = 1.0;
/// Sides of the circle around a point.
const DISC_SIDES: usize = 24;

pub fn router() -> Router<Ctx> {
    Router::new()
        .route("/api/rules", get(list_route).post(create_route))
        .route("/api/rules/{id}", axum::routing::patch(update_route).delete(remove_route))
        .route("/api/rules/{id}/convert", post(convert_route))
}

// ---- records --------------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FarmRule {
    pub id: String,
    pub text: String,
    /// The no-go zone or exclusion it became.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub feature_id: Option<String>,
    pub created_by: Actor,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_by: Option<Actor>,
    pub updated_at: DateTime<Utc>,
    /// What it could become on the map; none once it is a feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<Suggestion>,
}

/// A rule as a map feature: a no-go zone, or an exclusion for its dates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Suggestion {
    /// `no_go` or `exclusion`.
    pub kind: FeatureKind,
    pub name: String,
    pub geometry: Polygon,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paddock_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_from: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_until: Option<DateTime<Utc>>,
    /// "Make Creek a no-go zone"
    pub text: String,
}

fn rule_from_row(r: &sqlx::sqlite::SqliteRow) -> anyhow::Result<FarmRule> {
    let updated_by: Option<String> = r.try_get("updated_by")?;
    Ok(FarmRule {
        id: r.try_get("id")?,
        text: r.try_get("text")?,
        feature_id: r.try_get("feature_id")?,
        created_by: serde_json::from_str(&r.try_get::<String, _>("created_by")?)?,
        created_at: from_db(&r.try_get::<String, _>("created_at")?)?,
        updated_by: updated_by.map(|a| serde_json::from_str(&a)).transpose()?,
        updated_at: from_db(&r.try_get::<String, _>("updated_at")?)?,
        suggestion: None,
    })
}

/// The farm's rules in force, oldest first, without suggestions.
pub async fn active(ctx: &Ctx) -> anyhow::Result<Vec<FarmRule>> {
    let rows = sqlx::query("SELECT * FROM farm_rules WHERE removed_at IS NULL ORDER BY created_at, rowid").fetch_all(ctx.db()).await?;
    rows.iter().map(rule_from_row).collect()
}

/// [`active`], each with what it could become on the map.
pub async fn list(ctx: &Ctx) -> anyhow::Result<Vec<FarmRule>> {
    let mut rules = active(ctx).await?;
    let world = World::load(ctx).await?;
    for r in rules.iter_mut() {
        let linked = match &r.feature_id {
            Some(f) => op_core::features::get_feature(ctx, f).await?.is_some(),
            None => false,
        };
        if !linked {
            r.feature_id = None;
            r.suggestion = suggest(&read(&r.text, &world), &world);
        }
    }
    Ok(rules)
}

fn clean_text(text: &str) -> ApiResult<String> {
    let t = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if t.is_empty() {
        return Err(ApiError::bad_request("Write the rule."));
    }
    if t.chars().count() > MAX_TEXT {
        return Err(ApiError::bad_request(format!("A rule is at most {MAX_TEXT} characters.")));
    }
    Ok(t)
}

pub async fn create(ctx: &Ctx, text: &str, by: &Actor) -> ApiResult<FarmRule> {
    let text = clean_text(text)?;
    if active(ctx).await?.len() >= MAX_RULES {
        return Err(ApiError::conflict(format!("A farm keeps at most {MAX_RULES} rules. Remove one first.")));
    }
    let at = now();
    let rule =
        FarmRule { id: id::new_id(RULE), text, feature_id: None, created_by: by.clone(), created_at: at, updated_by: None, updated_at: at, suggestion: None };
    sqlx::query("INSERT INTO farm_rules (id, text, created_by, created_at, updated_at) VALUES (?, ?, ?, ?, ?)")
        .bind(&rule.id)
        .bind(&rule.text)
        .bind(serde_json::to_string(by).map_err(anyhow::Error::from)?)
        .bind(to_db(&at))
        .bind(to_db(&at))
        .execute(ctx.db())
        .await?;
    changed(ctx).await;
    tracing::info!(rule = %rule.id, "farm rule added");
    Ok(rule)
}

pub async fn update(ctx: &Ctx, rule_id: &str, text: &str, by: &Actor) -> ApiResult<FarmRule> {
    let text = clean_text(text)?;
    let row = sqlx::query("UPDATE farm_rules SET text = ?, updated_by = ?, updated_at = ? WHERE id = ? AND removed_at IS NULL RETURNING *")
        .bind(&text)
        .bind(serde_json::to_string(by).map_err(anyhow::Error::from)?)
        .bind(to_db(&now()))
        .bind(rule_id)
        .fetch_optional(ctx.db())
        .await?;
    let rule = rule_from_row(&row.ok_or_else(|| ApiError::not_found("No such rule."))?)?;
    changed(ctx).await;
    Ok(rule)
}

/// Removed rules stay, with who removed them and when; the feature a rule
/// became stays on the map.
pub async fn remove(ctx: &Ctx, rule_id: &str, by: &Actor) -> ApiResult<()> {
    let n = sqlx::query("UPDATE farm_rules SET removed_by = ?, removed_at = ? WHERE id = ? AND removed_at IS NULL")
        .bind(serde_json::to_string(by).map_err(anyhow::Error::from)?)
        .bind(to_db(&now()))
        .bind(rule_id)
        .execute(ctx.db())
        .await?
        .rows_affected();
    if n == 0 {
        return Err(ApiError::not_found("No such rule."));
    }
    changed(ctx).await;
    Ok(())
}

/// Make a rule the map feature it suggests: a no-go zone or a dated exclusion.
pub async fn convert(ctx: &Ctx, rule_id: &str, by: &Actor) -> ApiResult<(FarmRule, MapFeature)> {
    let rule = list(ctx).await?.into_iter().find(|r| r.id == rule_id).ok_or_else(|| ApiError::not_found("No such rule."))?;
    let s = rule.suggestion.clone().ok_or_else(|| ApiError::conflict("This rule doesn't name ground to keep out. It stays a rule the agent follows."))?;
    let feature = op_core::features::insert_feature(
        ctx,
        NewFeature {
            kind: s.kind,
            name: Some(s.name.clone()),
            geometry: FeatureGeometry::Polygon(s.geometry.coordinates.clone()),
            paddock_id: None,
            notes: Some(format!("From the farm rule \"{}\"", rule.text)),
            props: json!({ "rule_id": rule.id }),
            active_from: s.active_from,
            active_until: s.active_until,
        },
    )
    .await?;
    ctx.publish(Event::Feature { feature: feature.clone(), deleted: false });
    let row = sqlx::query("UPDATE farm_rules SET feature_id = ?, updated_by = ?, updated_at = ? WHERE id = ? RETURNING *")
        .bind(&feature.id)
        .bind(serde_json::to_string(by).map_err(anyhow::Error::from)?)
        .bind(to_db(&now()))
        .bind(rule_id)
        .fetch_one(ctx.db())
        .await?;
    changed(ctx).await;
    tracing::info!(rule = %rule_id, feature = %feature.id, kind = %feature.kind.as_db(), "farm rule made a map feature");
    Ok((rule_from_row(&row)?, feature))
}

// ---- what MCP agents read -------------------------------------------------------------------

/// Each farm's rule texts (by data dir), kept for the MCP tool descriptions,
/// which are built without a database read.
static CACHED: LazyLock<RwLock<HashMap<PathBuf, Vec<String>>>> = LazyLock::new(Default::default);

async fn changed(ctx: &Ctx) {
    if let Err(e) = refresh(ctx).await {
        tracing::warn!("farm rules: {e:#}");
    }
}

/// Reload the rule texts MCP descriptions carry.
pub async fn refresh(ctx: &Ctx) -> anyhow::Result<()> {
    let texts = active(ctx).await?.into_iter().map(|r| r.text).collect();
    CACHED.write().unwrap_or_else(|e| e.into_inner()).insert(ctx.data_dir().to_path_buf(), texts);
    Ok(())
}

/// What a write tool's description adds: the rules, numbered.
pub fn description_tail(ctx: &Ctx) -> String {
    let cached = CACHED.read().unwrap_or_else(|e| e.into_inner());
    let rules = cached.get(ctx.data_dir()).map(Vec::as_slice).unwrap_or_default();
    let mut out = String::from(
        " Every proposal must follow the farm's rules and stay out of its no-go zones; the server's safety gate refuses any boundary that crosses a road or the farm boundary or takes in a no-go zone or a hard hazard, and a rule review holds anything doubtful for the farmer.",
    );
    if !rules.is_empty() {
        out.push_str(" Farm rules:");
        for (i, r) in rules.iter().enumerate() {
            out.push_str(&format!(" {}. {r}", i + 1));
        }
    }
    out
}

pub fn tool() -> ToolSpec {
    ToolSpec {
        name: "list_farm_rules",
        description: "The farm's own rules for the agent, in the farmer's words: every proposal must follow them. Each has an id, the text, who wrote it and when, and the no-go zone or exclusion it became on the map, if any.",
        input_schema: json!({ "type": "object", "properties": {}, "required": [], "additionalProperties": false }),
        read: true,
        brain: false,
        min_role: Role::Viewer,
        run: ToolSpec::run_fn(|c: ToolCall| async move {
            let rules: Vec<Value> = active(&c.ctx)
                .await?
                .into_iter()
                .map(|r| json!({ "id": r.id, "text": r.text, "feature_id": r.feature_id, "by": r.created_by.name, "at": r.created_at }))
                .collect();
            Ok(json!({ "rules": rules }))
        }),
    }
}

/// The rules and the no-go zones, for the decision context.
pub async fn for_context(ctx: &Ctx) -> anyhow::Result<Value> {
    let rules: Vec<Value> = active(ctx).await?.into_iter().map(|r| json!({ "id": r.id, "text": r.text })).collect();
    let zones: Vec<Value> = op_core::features::list_features(ctx, Some(FeatureKind::NoGo), None, None)
        .await?
        .into_iter()
        .map(|f| json!({ "id": f.id, "name": f.name, "geometry": f.geometry }))
        .collect();
    Ok(json!({ "rules": rules, "no_go_zones": zones }))
}

// ---- reading a rule -------------------------------------------------------------------------

/// What the farm has that rules name.
pub struct World {
    pub paddocks: Vec<Paddock>,
    pub features: Vec<MapFeature>,
    pub herds: Vec<Herd>,
    pub tz: chrono_tz::Tz,
    pub now: DateTime<Utc>,
}

impl World {
    pub async fn load(ctx: &Ctx) -> anyhow::Result<Self> {
        let store = ctx.store();
        let tz = store.get_farm().await?.and_then(|f| f.timezone.parse().ok()).unwrap_or(chrono_tz::UTC);
        Ok(Self {
            paddocks: store.list_paddocks().await?,
            features: op_core::features::list_features(ctx, None, None, None).await?,
            herds: store.list_herds().await?,
            tz,
            now: now(),
        })
    }
}

/// Ground a rule names.
#[derive(Debug, Clone, PartialEq)]
pub struct Ground {
    pub name: String,
    pub shape: Shape,
    pub paddock_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Area(Polygon),
    Line(Vec<LonLat>),
    Point(LonLat),
}

/// Month and day, for windows that come round every year.
pub type Day = (u32, u32);

/// What a rule says, as far as words can be read without a model.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Reading {
    pub ground: Option<Ground>,
    /// It keeps animals out of (or away from) the ground.
    pub keep_out: bool,
    /// How far from the ground (m).
    pub distance_m: f64,
    /// In force from this day each year (inclusive)…
    pub from: Option<Day>,
    /// …until this day (exclusive).
    pub until: Option<Day>,
    /// Herds it names; none: every herd.
    pub herds: Vec<String>,
    /// It names animals that aren't a herd ("the heifers" on a farm with no herd of that name).
    pub vague: bool,
}

/// Lowercase words, apostrophes dropped ("neighbour's" → "neighbours").
fn words(s: &str) -> Vec<String> {
    let s: String = s.to_lowercase().chars().filter(|c| *c != '\'' && *c != '’').map(|c| if c.is_alphanumeric() || c == '.' { c } else { ' ' }).collect();
    s.split_whitespace().map(|w| w.trim_end_matches('.').to_owned()).filter(|w| !w.is_empty()).collect()
}

/// Whether `name`'s words appear in a row in `text`.
fn mentions(text: &[String], name: &str) -> bool {
    let n = words(name);
    !n.is_empty() && text.windows(n.len()).any(|w| w == n.as_slice())
}

const KEEP_OUT: [&str; 6] = ["never", "no", "not", "dont", "avoid", "without"];
const KEEP_OUT_PAIRS: [[&str; 2]; 8] =
    [["keep", "out"], ["keep", "off"], ["stay", "out"], ["stay", "off"], ["stay", "away"], ["away", "from"], ["clear", "of"], ["out", "of"]];
/// Words for every herd.
const ALL: [&str; 9] = ["cattle", "cows", "cow", "stock", "livestock", "animals", "herd", "herds", "sheep"];
/// Words for some animals, which only a herd's name can pin down.
const CLASSES: [&str; 22] = [
    "heifer",
    "heifers",
    "bull",
    "bulls",
    "calf",
    "calves",
    "steer",
    "steers",
    "ewe",
    "ewes",
    "lamb",
    "lambs",
    "ram",
    "rams",
    "wether",
    "wethers",
    "weaner",
    "weaners",
    "yearling",
    "yearlings",
    "kids",
    "bucks",
];
const MONTHS: [&str; 12] = ["jan", "feb", "mar", "apr", "may", "jun", "jul", "aug", "sep", "oct", "nov", "dec"];

fn month(w: &str) -> Option<u32> {
    let w = w.trim_end_matches('.');
    if w.len() < 3 {
        return None;
    }
    MONTHS.iter().position(|m| w.starts_with(m) && (w.len() == 3 || full_month(w))).map(|i| i as u32 + 1)
}

fn full_month(w: &str) -> bool {
    ["january", "february", "march", "april", "may", "june", "july", "august", "september", "sept", "october", "november", "december"].contains(&w)
}

fn day_number(w: &str) -> Option<u32> {
    let d = w.trim_end_matches(|c: char| c.is_alphabetic());
    d.parse::<u32>().ok().filter(|d| (1..=31).contains(d)).filter(|_| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
}

/// A month and day starting at `i` ("july", "july 1st", "1 july"), and the words it took.
fn date_at(ws: &[String], i: usize) -> Option<(Day, usize)> {
    let w = ws.get(i)?;
    if let Some(m) = month(w) {
        let d = ws.get(i + 1).and_then(|n| day_number(n));
        return Some(((m, d.unwrap_or(1)), if d.is_some() { 2 } else { 1 }));
    }
    let d = day_number(w)?;
    let next = if ws.get(i + 1).is_some_and(|x| x == "of") { i + 2 } else { i + 1 };
    let m = month(ws.get(next)?)?;
    Some(((m, d), next + 1 - i))
}

/// A distance at `i` ("30 m", "30m", "100 feet"), in metres.
fn distance_at(ws: &[String], i: usize) -> Option<f64> {
    let w = &ws[i];
    let (num, unit) = match w.find(|c: char| c.is_alphabetic()) {
        Some(k) if k > 0 => (&w[..k], w[k..].to_owned()),
        _ => (w.as_str(), ws.get(i + 1)?.clone()),
    };
    let n: f64 = num.parse().ok().filter(|n: &f64| n.is_finite() && *n > 0.0 && *n <= 5000.0)?;
    let per = match unit.as_str() {
        "m" | "metre" | "metres" | "meter" | "meters" => 1.0,
        "km" => 1000.0,
        "ft" | "foot" | "feet" => 0.3048,
        "yd" | "yard" | "yards" => 0.9144,
        _ => return None,
    };
    Some(n * per)
}

/// Read a rule (see [`Reading`]).
pub fn read(text: &str, world: &World) -> Reading {
    let ws = words(text);
    let mut r = Reading::default();
    r.keep_out = ws.iter().any(|w| KEEP_OUT.contains(&w.as_str()))
        || ws.windows(2).any(|p| KEEP_OUT_PAIRS.iter().any(|k| p[0] == k[0] && p[1] == k[1]))
        || ws.iter().any(|w| w == "leave") && (0..ws.len()).any(|i| distance_at(&ws, i).is_some());
    // The longest name wins; a paddock over a feature of the same length.
    let mut best: Option<(usize, Ground)> = None;
    let mut consider = |name: &str, g: Ground, bonus: usize| {
        if mentions(&ws, name) {
            let score = words(name).len() * 2 + bonus;
            if best.as_ref().is_none_or(|(s, _)| score > *s) {
                best = Some((score, g));
            }
        }
    };
    for p in &world.paddocks {
        consider(&p.name, Ground { name: p.name.clone(), shape: Shape::Area(p.geometry.clone()), paddock_id: Some(p.id.clone()) }, 1);
    }
    for f in world.features.iter().filter(|f| f.kind != FeatureKind::NoGo) {
        let Some(name) = f.name.as_deref() else { continue };
        let shape = match &f.geometry {
            FeatureGeometry::Polygon(rings) if !rings.is_empty() => Shape::Area(Polygon::from_rings(rings[0].clone(), rings[1..].iter().cloned())),
            FeatureGeometry::Polygon(_) => continue,
            FeatureGeometry::LineString(pts) => Shape::Line(pts.clone()),
            FeatureGeometry::Point(p) => Shape::Point(*p),
        };
        consider(name, Ground { name: name.to_owned(), shape, paddock_id: f.paddock_id.clone() }, 0);
    }
    r.ground = best.map(|(_, g)| g);
    r.distance_m = (0..ws.len()).find_map(|i| distance_at(&ws, i)).unwrap_or(0.0);
    for i in 0..ws.len() {
        match ws[i].as_str() {
            "before" | "until" | "till" | "til" | "through" => {
                if let Some((d, _)) = date_at(&ws, i + 1) {
                    r.until.get_or_insert(d);
                }
            }
            "after" | "from" | "since" => {
                if let Some(((m, d), n)) = date_at(&ws, i + 1) {
                    // "after July" is from August; "after July 15" from the 16th.
                    let day = match (ws[i].as_str(), n) {
                        ("after", 1) => (m % 12 + 1, 1),
                        ("after", _) => next_day((m, d)),
                        _ => (m, d),
                    };
                    r.from.get_or_insert(day);
                }
            }
            "between" => {
                if let Some((a, n)) = date_at(&ws, i + 1)
                    && ws.get(i + 1 + n).is_some_and(|w| w == "and")
                    && let Some((b, _)) = date_at(&ws, i + 2 + n)
                {
                    r.from = Some(a);
                    r.until = Some(b);
                }
            }
            _ => {}
        }
    }
    for h in &world.herds {
        if mentions(&ws, &h.name) {
            r.herds.push(h.id.clone());
        }
    }
    // Words of the ground's own name ("bull pasture") don't name animals.
    let named: Vec<String> = r.ground.as_ref().map(|g| words(&g.name)).unwrap_or_default();
    let herd_words: Vec<String> = world.herds.iter().flat_map(|h| words(&h.name)).collect();
    let animal = |w: &String| CLASSES.contains(&w.as_str()) && !herd_words.contains(w) && !named.contains(w);
    r.vague = r.herds.is_empty() && ws.iter().any(animal) && !ws.iter().any(|w| ALL.contains(&w.as_str()));
    r
}

fn next_day((m, d): Day) -> Day {
    let len = NaiveDate::from_ymd_opt(2024, m % 12 + 1, 1).and_then(|n| n.pred_opt()).map_or(31, |n| n.day());
    if d >= len { (m % 12 + 1, 1) } else { (m, d + 1) }
}

/// Whether a yearly window is in force on `day`.
fn in_force(r: &Reading, day: Day) -> bool {
    match (r.from, r.until) {
        (None, None) => true,
        (Some(f), None) => day >= f,
        (None, Some(u)) => day < u,
        (Some(f), Some(u)) if f <= u => day >= f && day < u,
        (Some(f), Some(u)) => day >= f || day < u,
    }
}

fn today(world: &World) -> Day {
    let local = world.now.with_timezone(&world.tz);
    (local.month(), local.day())
}

/// The farm-time midnight that starts `day` in `year`, in UTC.
fn midnight(world: &World, year: i32, (m, d): Day) -> Option<DateTime<Utc>> {
    let date = NaiveDate::from_ymd_opt(year, m, d).or_else(|| NaiveDate::from_ymd_opt(year, m, 28))?;
    world.tz.from_local_datetime(&date.and_hms_opt(0, 0, 0)?).earliest().map(|t| t.with_timezone(&Utc))
}

/// The window a dated rule keeps the ground out for, as concrete times: the
/// one in force now, else the next one.
fn next_window(r: &Reading, world: &World) -> (Option<DateTime<Utc>>, Option<DateTime<Utc>>) {
    let year = world.now.with_timezone(&world.tz).year();
    let t = today(world);
    let after = |d: Day| if d > t { year } else { year + 1 };
    match (r.from, r.until) {
        (None, Some(u)) if in_force(r, t) => (None, midnight(world, year, u)),
        (None, Some(u)) => (midnight(world, year + 1, (1, 1)), midnight(world, year + 1, u)),
        (Some(_), None) if in_force(r, t) => (None, midnight(world, year + 1, (1, 1))),
        (Some(f), None) => (midnight(world, year, f), midnight(world, year + 1, (1, 1))),
        (Some(_), Some(u)) if in_force(r, t) => (None, midnight(world, after(u), u)),
        (Some(f), Some(u)) => {
            let fy = after(f);
            (midnight(world, fy, f), midnight(world, if u > f { fy } else { fy + 1 }, u))
        }
        (None, None) => (None, None),
    }
}

/// What a rule could become on the map: only a plain keep-out of named
/// ground, for every herd, with no animals it can't place.
pub fn suggest(r: &Reading, world: &World) -> Option<Suggestion> {
    let g = r.ground.as_ref()?;
    if !r.keep_out || !r.herds.is_empty() || r.vague {
        return None;
    }
    let geometry = buffered(g, r.distance_m)?;
    let dated = r.from.is_some() || r.until.is_some();
    let (active_from, active_until) = if dated { next_window(r, world) } else { (None, None) };
    let away = if r.distance_m > 0.0 { format!(" and {:.0} m around it", r.distance_m) } else { String::new() };
    let (kind, text) = if dated {
        let until = active_until.map(|t| t.with_timezone(&world.tz).format("%b %-d %Y").to_string()).unwrap_or_default();
        (FeatureKind::Exclusion, format!("Keep {}{away} out until {until}", g.name))
    } else {
        (FeatureKind::NoGo, format!("Make {}{away} a no-go zone", g.name))
    };
    Some(Suggestion { kind, name: g.name.clone(), geometry, paddock_id: g.paddock_id.clone(), active_from, active_until, text })
}

// ---- geometry -------------------------------------------------------------------------------

struct Local(Projection);

impl Local {
    fn ring(&self, r: &[LonLat]) -> LineString<f64> {
        let mut pts: Vec<Coord<f64>> = op_geo::clean_ring(r).iter().map(|p| self.0.forward(*p)).map(|[x, y]| Coord { x, y }).collect();
        if let Some(f) = pts.first().copied() {
            pts.push(f);
        }
        LineString(pts)
    }

    fn poly(&self, p: &Polygon) -> geo::Polygon<f64> {
        let mut it = p.coordinates.iter();
        let outer = it.next().map(|r| self.ring(r)).unwrap_or_else(|| LineString(vec![]));
        geo::Polygon::new(outer, it.map(|r| self.ring(r)).collect())
    }

    fn disc(&self, c: [f64; 2], r: f64) -> geo::Polygon<f64> {
        let mut pts: Vec<Coord<f64>> = (0..DISC_SIDES)
            .map(|i| {
                let a = std::f64::consts::TAU * i as f64 / DISC_SIDES as f64;
                Coord { x: c[0] + r * a.cos(), y: c[1] + r * a.sin() }
            })
            .collect();
        pts.push(pts[0]);
        geo::Polygon::new(LineString(pts), vec![])
    }

    /// A band `r` wide either side of a segment, with round ends.
    fn band(&self, a: [f64; 2], b: [f64; 2], r: f64) -> MultiPolygon<f64> {
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let len = dx.hypot(dy);
        let mut out = MultiPolygon(vec![self.disc(a, r)]);
        if len > 0.0 {
            let (nx, ny) = (-dy / len * r, dx / len * r);
            let quad = vec![
                Coord { x: a[0] + nx, y: a[1] + ny },
                Coord { x: b[0] + nx, y: b[1] + ny },
                Coord { x: b[0] - nx, y: b[1] - ny },
                Coord { x: a[0] - nx, y: a[1] - ny },
                Coord { x: a[0] + nx, y: a[1] + ny },
            ];
            out = out.union(&MultiPolygon(vec![geo::Polygon::new(LineString(quad), vec![]), self.disc(b, r)]));
        }
        out
    }

    /// `g` grown by `r` metres (the ground itself when `r` is 0; nothing for a line or point then).
    fn grown(&self, g: &Shape, r: f64) -> Option<MultiPolygon<f64>> {
        let pts: Vec<[f64; 2]> = match g {
            Shape::Area(p) if r <= 0.0 => return Some(MultiPolygon(vec![self.poly(p)])),
            Shape::Area(p) => p.coordinates.iter().flat_map(|ring| op_geo::clean_ring(ring)).map(|c| self.0.forward(c)).collect(),
            Shape::Line(l) => l.iter().map(|c| self.0.forward(*c)).collect(),
            Shape::Point(p) => vec![self.0.forward(*p)],
        };
        if r <= 0.0 {
            return None;
        }
        let mut out = match g {
            Shape::Area(p) => MultiPolygon(vec![self.poly(p)]),
            _ => MultiPolygon(vec![]),
        };
        if pts.len() == 1 {
            return Some(out.union(&MultiPolygon(vec![self.disc(pts[0], r)])));
        }
        let closed = matches!(g, Shape::Area(_));
        for w in pts.windows(2) {
            out = out.union(&self.band(w[0], w[1], r));
        }
        if closed && let (Some(f), Some(l)) = (pts.first(), pts.last()) {
            out = out.union(&self.band(*l, *f, r));
        }
        Some(out)
    }

    fn back(&self, mp: &MultiPolygon<f64>) -> Option<Polygon> {
        let biggest = mp.0.iter().max_by(|a, b| a.unsigned_area().total_cmp(&b.unsigned_area()))?;
        let ring = |ls: &LineString<f64>| -> Vec<LonLat> {
            ls.coords().map(|c| self.0.inverse([c.x, c.y])).map(|[x, y]| [op_geo::projection::round7(x), op_geo::projection::round7(y)]).collect()
        };
        Polygon::from_ring(ring(biggest.exterior())).validated().ok()
    }
}

fn anchor(g: &Shape) -> Option<LonLat> {
    match g {
        Shape::Area(p) => p.outer_ring().first().copied(),
        Shape::Line(l) => l.first().copied(),
        Shape::Point(p) => Some(*p),
    }
}

/// The ground as a polygon, grown by `distance_m` (one ring: what a map feature holds).
pub fn buffered(g: &Ground, distance_m: f64) -> Option<Polygon> {
    let loc = Local(Projection::new(anchor(&g.shape)?));
    let grown = loc.grown(&g.shape, distance_m)?;
    let p = loc.back(&grown)?;
    // Simplified to a ring a feature takes: at most 256 corners.
    Some(op_geo::shape::fit(&p, &op_geo::CollarLimits { outer: 256, holes: 0, hole_vertices: 0, total: 256, ..op_geo::CollarLimits::V0 }))
}

/// Whether `shape` comes onto the ground or within `distance_m` of it.
fn touches(shape: &Polygon, g: &Ground, distance_m: f64) -> bool {
    let Some(o) = anchor(&g.shape) else { return false };
    let loc = Local(Projection::new(o));
    let Some(zone) = loc.grown(&g.shape, distance_m) else { return false };
    MultiPolygon(vec![loc.poly(shape)]).intersection(&zone).unsigned_area() >= OVERLAP_M2
}

// ---- checking a decision --------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Complies,
    Violates,
    Unsure,
}

/// One rule against one decision.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RuleCheck {
    pub rule_id: String,
    pub verdict: Verdict,
    /// `map` (read as ground and dates), `gate` (it is a map feature now) or `reviewer`.
    pub by: String,
    pub why: String,
}

/// A rule checked on the map where its words allow (see [`read`]); `None`
/// when only a reviewer can tell.
pub fn check_on_map(r: &Reading, world: &World, herd_id: &str, geometry: Option<&Polygon>) -> Option<(Verdict, String)> {
    let g = r.ground.as_ref().filter(|_| r.keep_out)?;
    if !in_force(r, today(world)) {
        return Some((Verdict::Complies, "not in force today".into()));
    }
    let Some(shape) = geometry else { return Some((Verdict::Complies, "no new ground".into())) };
    if !touches(shape, g, r.distance_m) {
        let near = if r.distance_m > 0.0 { format!("{:.0} m clear of {}", r.distance_m, g.name) } else { format!("clear of {}", g.name) };
        return Some((Verdict::Complies, near));
    }
    if !r.herds.is_empty() && !r.herds.iter().any(|h| h == herd_id) {
        return Some((Verdict::Complies, "about another herd".into()));
    }
    if r.vague {
        return None;
    }
    let what = if r.distance_m > 0.0 { format!("comes within {:.0} m of {}", r.distance_m, g.name) } else { format!("takes in {}", g.name) };
    Some((Verdict::Violates, what))
}

/// The review of a decision before it goes anywhere (see the module docs).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Review {
    /// Nothing holds it back: it may go on its own.
    pub clean: bool,
    /// Why not, one sentence each.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reasons: Vec<String>,
    /// The gate's hard findings.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub gate: Vec<op_core::check::Finding>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RuleCheck>,
    pub at: DateTime<Utc>,
}

fn quoted(rules: &[FarmRule], id: &str) -> String {
    rules.iter().position(|r| r.id == id).map(|i| format!("rule {} (\"{}\")", i + 1, rules[i].text)).unwrap_or_else(|| "a rule".into())
}

/// Who a decision is from, for the gate.
pub fn proposer(d: &Decision) -> SenderKind {
    if d.inputs["via"] == "mcp" { SenderKind::Mcp } else { SenderKind::Brain }
}

/// Review a decision that would change what the collars get. `automatic`:
/// it would go without a person (timer, auto), so rules the map can't settle
/// go to the reviewer, and the brain's statement on the rules is required.
pub async fn review(ctx: &Ctx, d: &Decision, herd: &Herd, automatic: bool) -> anyhow::Result<Review> {
    let mut out = Review { clean: true, reasons: vec![], gate: vec![], rules: vec![], at: now() };
    // The gate, as it will judge the send: the proposer's, a machine.
    if let Some(g) = &d.geometry {
        match op_ingest::prepare(ctx, &herd.id, g, &op_ingest::SendOpts::default()).await {
            Ok(p) => {
                let v = gate::judge(&p.findings, &Sender::of(proposer(d)));
                if !v.allowed {
                    out.reasons.extend(v.hard.iter().map(|f| f.text.clone()));
                    out.gate = v.hard;
                }
            }
            Err(e) => out.reasons.push(format!("It can't go to the collars: {}", e.message)),
        }
    }
    let rules = active(ctx).await?;
    if !rules.is_empty() {
        let world = World::load(ctx).await?;
        let mut open: Vec<&FarmRule> = Vec::new();
        for r in &rules {
            if let Some(f) = &r.feature_id
                && op_core::features::get_feature(ctx, f).await?.is_some()
            {
                out.rules.push(RuleCheck { rule_id: r.id.clone(), verdict: Verdict::Complies, by: "gate".into(), why: "kept on the map".into() });
                continue;
            }
            match check_on_map(&read(&r.text, &world), &world, &herd.id, d.geometry.as_ref()) {
                Some((verdict, why)) => out.rules.push(RuleCheck { rule_id: r.id.clone(), verdict, by: "map".into(), why }),
                None => open.push(r),
            }
        }
        let llm = d.brain.is_some_and(|b| b != BrainId::Heuristic) || d.inputs["via"] == "mcp";
        let said = d.inputs["rules"].as_str().is_some_and(|s| !s.trim().is_empty());
        if automatic && llm && !said {
            out.reasons.push("It didn't say how it follows the farm's rules.".into());
        }
        let broken = out.rules.iter().any(|c| c.verdict == Verdict::Violates);
        if !open.is_empty() {
            if automatic && out.gate.is_empty() && !broken {
                out.rules.extend(reviewer(ctx, d, herd, &rules, &open, &world).await);
            } else {
                out.rules.extend(open.iter().map(|r| RuleCheck {
                    rule_id: r.id.clone(),
                    verdict: Verdict::Unsure,
                    by: "none".into(),
                    why: "for the farmer to judge".into(),
                }));
            }
        }
        for c in &out.rules {
            match c.verdict {
                Verdict::Violates => out.reasons.push(format!("Breaks {}: {}.", quoted(&rules, &c.rule_id), c.why)),
                Verdict::Unsure if automatic => out.reasons.push(format!("Not sure it follows {}: {}.", quoted(&rules, &c.rule_id), c.why)),
                _ => {}
            }
        }
        if !automatic && out.rules.iter().any(|c| c.verdict == Verdict::Unsure) {
            out.clean = false;
        }
    }
    if !out.reasons.is_empty() {
        out.clean = false;
    }
    Ok(out)
}

/// The second brain pass: the farm's brain asked whether the decision
/// follows the rules the map couldn't settle. Anything but a clear
/// COMPLIES is a doubt, and so is a brain that can't answer.
async fn reviewer(ctx: &Ctx, d: &Decision, herd: &Herd, all: &[FarmRule], open: &[&FarmRule], world: &World) -> Vec<RuleCheck> {
    let unsure = |why: &str| {
        open.iter().map(|r| RuleCheck { rule_id: r.id.clone(), verdict: Verdict::Unsure, by: "reviewer".into(), why: why.to_owned() }).collect::<Vec<_>>()
    };
    let brain = match op_brain::resolve(ctx).await {
        Ok(b) => b,
        Err(e) => return unsure(&format!("no reviewer ({e})")),
    };
    let numbered: Vec<String> = open.iter().map(|r| format!("{}: {}", quoted(all, &r.id), r.text)).collect();
    let covers: Vec<Value> = match &d.geometry {
        Some(g) => world
            .paddocks
            .iter()
            .filter_map(|p| {
                let share = op_ingest::prepare::share_inside(g, &p.geometry);
                (share > 0.01).then(|| json!({ "paddock": p.name, "share_of_boundary": (share * 100.0).round() / 100.0 }))
            })
            .collect(),
        None => vec![],
    };
    let to = d.to_paddock_id.as_deref().and_then(|p| world.paddocks.iter().find(|x| x.id == p)).map(|p| p.name.clone());
    let context = json!({
        "today": world.now.with_timezone(&world.tz).format("%Y-%m-%d").to_string(),
        "herd": { "name": herd.name, "species": herd.species, "count": herd.count },
        "decision": { "action": d.action, "to_paddock": to, "boundary_covers": covers, "reasoning": d.reasoning, "how_it_follows_the_rules": d.inputs["rules"] },
        "rules_to_check": numbered,
    });
    let question = "You check one grazing decision against the farm's own rules before it goes to the collars without a person \
looking. Read the decision and the rules in the context. Answer with COMPLIES, VIOLATES or UNSURE as the first word, then one short \
sentence naming the rule and why. Say UNSURE when anything is unclear: a person will look."
        .to_owned();
    // No tools: it judges from what it is given (anonymous lists none).
    let req = op_brain::AskRequest { question, context, tools: ctx.tool_runner(Identity::anonymous(), &[]), max_chars: 400, log: None };
    let answer = match tokio::time::timeout(std::time::Duration::from_secs(60), brain.ask(req)).await {
        Ok(Ok(a)) => a,
        Ok(Err(e)) => return unsure(&format!("the reviewer couldn't answer ({e})")),
        Err(_) => return unsure("the reviewer took too long"),
    };
    let first = answer.split_whitespace().next().unwrap_or_default().trim_matches(|c: char| !c.is_alphabetic()).to_ascii_uppercase();
    let why = answer.split_whitespace().skip(1).collect::<Vec<_>>().join(" ");
    let why = if why.is_empty() { "no reason given".to_owned() } else { why };
    let verdict = match first.as_str() {
        "COMPLIES" => Verdict::Complies,
        "VIOLATES" => Verdict::Violates,
        _ => Verdict::Unsure,
    };
    tracing::info!(decision = %d.id, verdict = ?verdict, "rule review");
    open.iter().map(|r| RuleCheck { rule_id: r.id.clone(), verdict, by: "reviewer".into(), why: why.clone() }).collect()
}

/// A decision's action changes what the collars get.
pub fn acts(d: &Decision) -> bool {
    matches!(d.action, Some(DecisionAction::Move | DecisionAction::Hold))
}

/// Brains and agents are machines; a farmer's own decision isn't reviewed.
pub fn reviewed(d: &Decision) -> bool {
    d.source != DecisionSource::Farmer
}

// ---- routes ---------------------------------------------------------------------------------

#[derive(Deserialize)]
struct RuleBody {
    text: String,
}

async fn list_route(State(ctx): State<Ctx>) -> ApiResult<Json<Vec<FarmRule>>> {
    Ok(Json(list(&ctx).await?))
}

async fn create_route(State(ctx): State<Ctx>, identity: Identity, ApiJson(b): ApiJson<RuleBody>) -> ApiResult<(StatusCode, Json<FarmRule>)> {
    let rule = create(&ctx, &b.text, &identity.actor()).await?;
    let rule = list(&ctx).await?.into_iter().find(|r| r.id == rule.id).unwrap_or(rule);
    Ok((StatusCode::CREATED, Json(rule)))
}

async fn update_route(State(ctx): State<Ctx>, identity: Identity, Path(rule_id): Path<String>, ApiJson(b): ApiJson<RuleBody>) -> ApiResult<Json<FarmRule>> {
    let rule = update(&ctx, &rule_id, &b.text, &identity.actor()).await?;
    Ok(Json(list(&ctx).await?.into_iter().find(|r| r.id == rule.id).unwrap_or(rule)))
}

async fn remove_route(State(ctx): State<Ctx>, identity: Identity, Path(rule_id): Path<String>) -> ApiResult<StatusCode> {
    remove(&ctx, &rule_id, &identity.actor()).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Serialize)]
struct Converted {
    rule: FarmRule,
    feature: MapFeature,
}

async fn convert_route(State(ctx): State<Ctx>, identity: Identity, Path(rule_id): Path<String>) -> ApiResult<(StatusCode, Json<Converted>)> {
    let (rule, feature) = convert(&ctx, &rule_id, &identity.actor()).await?;
    Ok((StatusCode::CREATED, Json(Converted { rule, feature })))
}

#[cfg(test)]
mod tests {
    use super::*;

    const O: LonLat = [-93.6225, 42.0318];

    fn at(x: f64, y: f64) -> LonLat {
        Projection::new(O).inverse([x, y])
    }
    fn rect(x0: f64, y0: f64, x1: f64, y1: f64) -> Polygon {
        Polygon::from_ring(vec![at(x0, y0), at(x1, y0), at(x1, y1), at(x0, y1), at(x0, y0)])
    }
    fn paddock(id: &str, name: &str, g: Polygon) -> Paddock {
        serde_json::from_value(json!({ "id": id, "name": name, "geometry": g, "area_ha": 1.0, "created_at": now() })).unwrap()
    }
    fn herd(id: &str, name: &str) -> Herd {
        serde_json::from_value(json!({ "id": id, "name": name, "species": "cattle", "count": 20, "timer_minutes": 60, "created_at": now() })).unwrap()
    }
    fn line_feature(name: &str, pts: Vec<LonLat>) -> MapFeature {
        serde_json::from_value(json!({ "id": "fea_1", "kind": "neighbour_line", "name": name, "geometry": { "type": "LineString", "coordinates": pts }, "props": {}, "created_at": now(), "updated_at": now() })).unwrap()
    }
    fn world(month: u32, day: u32) -> World {
        World {
            paddocks: vec![paddock("pad_creek", "Creek", rect(0.0, 0.0, 100.0, 100.0)), paddock("pad_north", "North", rect(0.0, 200.0, 100.0, 300.0))],
            features: vec![line_feature("Neighbour's bull pasture", vec![at(-50.0, 400.0), at(200.0, 400.0)])],
            herds: vec![herd("herd_h", "Heifers"), herd("herd_c", "Cows")],
            tz: chrono_tz::America::Chicago,
            now: chrono_tz::America::Chicago.with_ymd_and_hms(2026, month, day, 12, 0, 0).unwrap().with_timezone(&Utc),
        }
    }

    #[test]
    fn a_seasonal_paddock_rule_for_one_herd_reads_as_ground_dates_and_herd() {
        let w = world(5, 10);
        let r = read("Never put the heifers in the creek paddock before July", &w);
        assert!(r.keep_out);
        assert_eq!(r.ground.as_ref().unwrap().paddock_id.as_deref(), Some("pad_creek"));
        assert_eq!(r.until, Some((7, 1)));
        assert_eq!(r.herds, ["herd_h"]);
        assert!(!r.vague);
        // It names a herd: not a farm-wide feature.
        assert!(suggest(&r, &w).is_none());
        // On the map: the heifers into Creek in May break it; the cows don't; North is clear; August is past it.
        assert_eq!(check_on_map(&r, &w, "herd_h", Some(&rect(10.0, 10.0, 90.0, 90.0))).unwrap().0, Verdict::Violates);
        assert_eq!(check_on_map(&r, &w, "herd_c", Some(&rect(10.0, 10.0, 90.0, 90.0))).unwrap().0, Verdict::Complies);
        assert_eq!(check_on_map(&r, &w, "herd_h", Some(&rect(10.0, 210.0, 90.0, 290.0))).unwrap().0, Verdict::Complies);
        assert_eq!(check_on_map(&r, &world(8, 1), "herd_h", Some(&rect(10.0, 10.0, 90.0, 90.0))).unwrap().0, Verdict::Complies);
    }

    #[test]
    fn a_distance_from_a_line_becomes_a_no_go_band() {
        let w = world(5, 10);
        let r = read("Always leave 30 m from the neighbour's bull pasture", &w);
        assert!(r.keep_out);
        assert_eq!(r.distance_m, 30.0);
        assert_eq!(r.ground.as_ref().unwrap().name, "Neighbour's bull pasture");
        let s = suggest(&r, &w).unwrap();
        assert_eq!(s.kind, FeatureKind::NoGo);
        assert_eq!(s.text, "Make Neighbour's bull pasture and 30 m around it a no-go zone");
        // The band reaches 30 m either side of the line.
        assert!(s.geometry.contains(at(50.0, 425.0)));
        assert!(s.geometry.contains(at(50.0, 375.0)));
        assert!(!s.geometry.contains(at(50.0, 340.0)));
        assert_eq!(check_on_map(&r, &w, "herd_c", Some(&rect(0.0, 300.0, 100.0, 380.0))).unwrap().0, Verdict::Violates);
        assert_eq!(check_on_map(&r, &w, "herd_c", Some(&rect(0.0, 300.0, 100.0, 360.0))).unwrap().0, Verdict::Complies);
    }

    #[test]
    fn a_dated_keep_out_offers_an_exclusion_and_vague_animals_go_to_the_reviewer() {
        let w = world(5, 10);
        let r = read("Keep everything out of North until 15 June", &w);
        assert_eq!(r.until, Some((6, 15)));
        let s = suggest(&r, &w).unwrap();
        assert_eq!(s.kind, FeatureKind::Exclusion);
        assert_eq!(s.active_from, None);
        assert_eq!(s.active_until.unwrap().with_timezone(&w.tz).format("%Y-%m-%d").to_string(), "2026-06-15");
        // Bulls aren't a herd here: only a reviewer can tell, unless the decision is clear of North anyway.
        let bulls = read("No bulls in North", &w);
        assert!(bulls.vague);
        assert!(check_on_map(&bulls, &w, "herd_c", Some(&rect(10.0, 210.0, 90.0, 290.0))).is_none());
        assert_eq!(check_on_map(&bulls, &w, "herd_c", Some(&rect(10.0, 10.0, 90.0, 90.0))).unwrap().0, Verdict::Complies);
        // Words without ground: the reviewer's.
        assert!(check_on_map(&read("Move them early on hot days", &w), &w, "herd_c", None).is_none());
        assert!(suggest(&read("Move them early on hot days", &w), &w).is_none());
    }

    #[test]
    fn windows_come_round_every_year() {
        let w = world(10, 26);
        let r = read("Stay out of Creek between November 1 and March 1", &w);
        assert_eq!((r.from, r.until), (Some((11, 1)), Some((3, 1))));
        assert!(!in_force(&r, (10, 26)));
        assert!(in_force(&r, (12, 25)) && in_force(&r, (1, 5)));
        let (from, until) = next_window(&r, &w);
        assert_eq!(from.unwrap().with_timezone(&w.tz).format("%Y-%m-%d").to_string(), "2026-11-01");
        assert_eq!(until.unwrap().with_timezone(&w.tz).format("%Y-%m-%d").to_string(), "2027-03-01");
        assert_eq!(read("never in Creek after July", &w).from, Some((8, 1)));
    }
}
