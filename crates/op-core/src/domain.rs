//! Domain types. Serde shapes match the TypeScript in `docs/API.md` exactly:
//! snake_case fields, lowercase enums, optional fields omitted when absent.

use chrono::{DateTime, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use op_geo::{GeofenceState as FenceState, LonLat, Polygon};
pub use op_protocol::AckStatus;

/// String conversions for the enums, matching their serde names. Used for
/// database columns.
pub trait DbEnum: Sized + Serialize + serde::de::DeserializeOwned {
    fn as_db(&self) -> String {
        match serde_json::to_value(self) {
            Ok(Value::String(s)) => s,
            _ => unreachable!("unit enum"),
        }
    }
    fn from_db(s: &str) -> anyhow::Result<Self> {
        serde_json::from_value(Value::String(s.to_owned())).map_err(|_| anyhow::anyhow!("bad enum value {s:?}"))
    }
}

macro_rules! db_enum {
    ($($t:ty),*) => { $(impl DbEnum for $t {})* };
}
db_enum!(
    PaddockStatus,
    Species,
    Autonomy,
    FenceState,
    AckStatus,
    BrainId,
    DecisionSource,
    DecisionStatus,
    DecisionAction,
    Units,
    MoveStatus,
    EscapeStatus,
    ParkReason,
    Sex,
    RemovedReason
);

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Farm {
    pub id: String,
    pub name: String,
    pub timezone: String,
    pub center: LonLat,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PaddockStatus {
    #[default]
    Resting,
    Grazing,
    Planned,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Paddock {
    pub id: String,
    pub name: String,
    pub geometry: Polygon,
    pub area_ha: f64,
    #[serde(default)]
    pub status: PaddockStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grazed_until: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// Extra facts, e.g. FSA numbers under `fsa_farm`, `fsa_tract`, `fsa_field`.
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub props: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Species {
    Cattle,
    Sheep,
    Goats,
}

/// `propose` waits for approval, `timer` applies after `timer_minutes` unless
/// the farmer stops it, `auto` applies at once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Autonomy {
    #[default]
    Propose,
    Timer,
    Auto,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Herd {
    pub id: String,
    pub name: String,
    pub species: Species,
    pub count: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub paddock_id: Option<String>,
    #[serde(default)]
    pub autonomy: Autonomy,
    pub timer_minutes: u32,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Animal {
    pub id: String,
    pub tag: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub herd_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
    /// 15-digit electronic ID (ISO 11784).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub eid: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub breed: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sex: Option<Sex>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub born: Option<NaiveDate>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    /// Sold, died, culled or moved off: kept for the record, off the map.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_reason: Option<RemovedReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Sex {
    Female,
    Male,
    Castrated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RemovedReason {
    Sold,
    Died,
    Culled,
    MovedOff,
}

/// Why a collar is off duty. A parked collar raises no alerts, is not drawn
/// and is not counted; its reports only update battery and health.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ParkReason {
    Charging,
    Shelf,
    Repair,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Fix {
    pub at: DateTime<Utc>,
    pub point: LonLat,
    pub accuracy_m: f64,
    pub sats: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cn0: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttf_s: Option<f64>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Collar {
    pub id: String,
    pub name: String,
    pub herd_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animal_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_seen: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub battery: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_version: Option<u32>,
    #[serde(default)]
    pub state: FenceState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fix: Option<Fix>,
    /// Firmware version the collar reports.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fw: Option<String>,
    /// Protocol capabilities the collar reports (`holes`, `slots`, …).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub caps: Vec<String>,
    /// Since when its fixes have been outside the boundary; cleared once back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outside_since: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parked_reason: Option<ParkReason>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub collar_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub animal_id: Option<String>,
    pub fix: Fix,
    pub state: FenceState,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Boundary {
    pub id: String,
    pub herd_id: String,
    pub version: u32,
    pub geometry: Polygon,
    pub warn_m: f64,
    pub hysteresis_m: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_at: Option<DateTime<Utc>>,
    pub decision_id: String,
    pub created_at: DateTime<Utc>,
    /// Set when the boundary is one collar's own (an escape), not the herd's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub collar_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoundaryAck {
    pub collar_id: String,
    pub version: u32,
    pub status: AckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProposedBoundary {
    pub decision_id: String,
    pub geometry: Polygon,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BoundaryStatus {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<Boundary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending: Option<Boundary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proposed: Option<ProposedBoundary>,
    #[serde(default)]
    pub acks: Vec<BoundaryAck>,
    /// The running move, or the last one for 10 minutes after it ends.
    #[serde(default, rename = "move", skip_serializing_if = "Option::is_none")]
    pub r#move: Option<Move>,
    /// Animals out on their own boundary, and those back or let go in the
    /// last 10 minutes.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub escapes: Vec<Escape>,
    /// Every alive staged boundary, in version order (`pending` is the last).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub staged: Vec<Boundary>,
    /// Per version held on collars: how many applied, stored, rejected.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub slots: Vec<SlotCount>,
}

/// How far one boundary version has reached the herd's collars. `collars`
/// counts herd collars not on an escape and not parked; the UI line
/// "Wed 07:00 248/250 stored" is `applied + stored` of `collars`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SlotCount {
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_at: Option<DateTime<Utc>>,
    pub applied: u32,
    pub stored: u32,
    pub rejected: u32,
    pub collars: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MoveStatus {
    Sweeping,
    Done,
    Stopped,
}

/// A target and the sweep that brings the herd into it (API.md "Moves").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Move {
    pub id: String,
    pub herd_id: String,
    pub decision_id: String,
    pub target: Polygon,
    pub status: MoveStatus,
    /// Boundaries sent so far for this move.
    pub step: u32,
    /// From the back line to the target, metres.
    pub remaining_m: f64,
    /// Collar ids dropped from the sweep.
    pub stragglers: Vec<String>,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EscapeStatus {
    Returning,
    Back,
    Stopped,
}

/// An animal that stayed outside its herd's boundary (API.md "Escapes"). Its
/// collar holds a boundary of its own, the herd's joined to a pen around it,
/// which closes in behind it until it is back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Escape {
    pub id: String,
    pub herd_id: String,
    pub collar_id: String,
    pub status: EscapeStatus,
    /// The collar's own boundary now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<Polygon>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    /// Boundaries sent to the collar so far.
    pub step: u32,
    /// From the pen's back line to the herd's boundary, metres.
    pub remaining_m: f64,
    pub started_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BrainId {
    Codex,
    Claude,
    Anthropic,
    Openai,
    Compatible,
    Hosted,
    #[default]
    Heuristic,
}

impl BrainId {
    pub const ALL: [BrainId; 7] = [Self::Codex, Self::Claude, Self::Anthropic, Self::Openai, Self::Compatible, Self::Hosted, Self::Heuristic];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionSource {
    Brain,
    Farmer,
    Heuristic,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DecisionStatus {
    Running,
    Proposed,
    Approved,
    Applied,
    Rejected,
    Failed,
    Superseded,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum DecisionAction {
    Stay,
    Move,
    NeedsInfo,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Decision {
    pub id: String,
    pub herd_id: String,
    pub source: DecisionSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub brain: Option<BrainId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    pub status: DecisionStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<DecisionAction>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to_paddock_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<Polygon>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub confidence: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub need: Option<String>,
    #[serde(default)]
    pub inputs: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub apply_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boundary_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub responded_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<Value>,
}

/// `Brain` in API.md: what `/api/brains` lists.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrainInfo {
    pub id: BrainId,
    pub name: String,
    pub available: bool,
    pub signed_in: bool,
    /// Secret names this brain needs.
    pub needs: Vec<String>,
    pub models: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Units {
    #[default]
    Metric,
    Imperial,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct BrainSetting {
    pub id: BrainId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ServerSettings {
    pub bind: String,
    pub port: u16,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub public_url: Option<String>,
    pub app_token: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub brain: BrainSetting,
    /// Local time of the daily decision, `HH:MM`.
    pub decision_time: String,
    pub server: ServerSettings,
    #[serde(default)]
    pub units: Units,
}

/// `GET /api/state`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppState {
    pub farm: Option<Farm>,
    pub herds: Vec<Herd>,
    pub paddocks: Vec<Paddock>,
    pub settings: Settings,
}

/// An entry in the append-only activity log.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActivityEvent {
    pub id: String,
    /// Dotted kind, e.g. `paddock.created`, `decision.applied`.
    pub kind: String,
    /// `farmer`, `brain`, `collar` or `system`.
    pub source: String,
    pub occurred_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default)]
    pub payload: Value,
    /// `(target_type, target_id)`: farm, paddock, herd, animal, collar, decision.
    #[serde(default)]
    pub targets: Vec<(String, String)>,
}
