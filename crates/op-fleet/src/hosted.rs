//! Stable factory-demo collars for the hosted mobile staging server.

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use op_core::Ctx;
use op_core::time::{now, to_db};
use op_protocol::SigningKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub const KEY_FILE: &str = "mobile-sim-device-keys.json";

#[derive(Debug, Clone, Serialize)]
pub struct DemoCard {
    pub serial: String,
    pub model: &'static str,
    pub claim_code: String,
    pub card: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct WelfareState {
    pub boundary_version: u32,
    pub armed: bool,
    pub seen_outside: bool,
    pub escaping: bool,
    pub outside_since_ms: i64,
    pub outside_cues: u8,
    pub pulse_cutoff: bool,
    pub crossed_out: bool,
    pub escape_cycles: u8,
    pub escape_window_start_ms: i64,
    pub enforcement_off: bool,
    pub cueing: bool,
    pub active_since_ms: i64,
    pub rest_until_ms: i64,
}

impl WelfareState {
    /// Firmware `cue_rearm`: a new fence clears only the current crossing.
    pub fn rearm(&mut self, boundary_version: u32) {
        self.boundary_version = boundary_version;
        self.armed = false;
        self.seen_outside = false;
        self.cueing = false;
        self.clear_crossing();
    }

    /// Firmware `cue_clear_shutoff`: the explicit caretaker/server clear.
    pub fn clear_shutoff(&mut self) {
        self.clear_crossing();
        self.escape_cycles = 0;
        self.escape_window_start_ms = 0;
        self.enforcement_off = false;
    }

    fn clear_crossing(&mut self) {
        self.escaping = false;
        self.outside_since_ms = 0;
        self.outside_cues = 0;
        self.pulse_cutoff = false;
        self.crossed_out = false;
    }
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    seeds: BTreeMap<String, String>,
    bearers: BTreeMap<String, String>,
    #[serde(default)]
    welfare: BTreeMap<String, WelfareState>,
}

const CODES: [&str; 16] = [
    "R00K1001GRAZE001",
    "R00K1002GRAZE002",
    "R00K1003GRAZE003",
    "R00K1004GRAZE004",
    "R00K1005GRAZE005",
    "R00K1006GRAZE006",
    "R00K1007GRAZE007",
    "R00K1008GRAZE008",
    "STAR2001GRAZE001",
    "STAR2002GRAZE002",
    "STAR2003GRAZE003",
    "STAR2004GRAZE004",
    "STAR2005GRAZE005",
    "STAR2006GRAZE006",
    "STAR2007GRAZE007",
    "STAR2008GRAZE008",
];

pub fn cards() -> Vec<DemoCard> {
    CODES
        .iter()
        .enumerate()
        .map(|(i, code)| {
            let rook = i < 8;
            let serial = if rook { format!("OPR-ROOK-{:04}", i + 1) } else { format!("OPS-STARLING-{:04}", i - 7) };
            let seed = seed(&serial);
            let public = SigningKey::from_bytes(&seed).verifying_key();
            let pubkey = URL_SAFE_NO_PAD.encode(public.as_bytes());
            DemoCard {
                serial: serial.clone(),
                model: if rook { "Rook (LTE-M + GPS)" } else { "Starling (LoRa mesh)" },
                claim_code: grouped(code),
                card: format!("OC2:{serial}:{pubkey}:{}", grouped(code)),
            }
        })
        .collect()
}

/// Idempotently factory-provision the demo inventory and persist device seeds.
pub async fn seed_inventory(ctx: &Ctx) -> anyhow::Result<()> {
    let mut state = read_state(ctx)?;
    let at = to_db(&now());
    for (i, card) in cards().iter().enumerate() {
        let raw_code = CODES[i];
        let seed = seed(&card.serial);
        state.seeds.entry(card.serial.clone()).or_insert_with(|| URL_SAFE_NO_PAD.encode(seed));
        let public = SigningKey::from_bytes(&seed).verifying_key();
        let pubkey = op_protocol::encode_public_key(&public);
        let kid = op_protocol::v2::crypto::kid(public.as_bytes());
        let class = if i < 8 { "direct" } else { "node" };
        let radios = if i < 8 {
            serde_json::json!({"imei": format!("3597620800{:05}", i + 1)})
        } else {
            serde_json::json!({"lora_eui": format!("70B3D57ED000{:04X}", i - 7)})
        };
        sqlx::query(
            "INSERT OR IGNORE INTO fleet_units
             (serial, device_class, hw_rev, fw_version, device_pubkey, device_kid, radios, caps, state, origin,
              claim_code, claim_checked, provisioned_at, created_at, updated_at)
             VALUES (?, ?, 'demo-pod-1', '0.2-sim', ?, ?, ?, '[\"gps\",\"pod\"]', 'provisioned', 'station', ?, 1, ?, ?, ?)",
        )
        .bind(&card.serial)
        .bind(class)
        .bind(pubkey)
        .bind(hex(&kid))
        .bind(radios.to_string())
        .bind(raw_code)
        .bind(&at)
        .bind(&at)
        .bind(&at)
        .execute(ctx.db())
        .await?;
        sqlx::query("INSERT OR IGNORE INTO fleet_unit_keys (serial, device_pubkey, device_kid, from_at) VALUES (?, ?, ?, ?)")
            .bind(&card.serial)
            .bind(op_protocol::encode_public_key(&public))
            .bind(hex(&kid))
            .bind(&at)
            .execute(ctx.db())
            .await?;
    }
    write_state(ctx, &state)?;
    tracing::info!(count = CODES.len(), "hosted demo collar inventory ready");
    Ok(())
}

pub fn remember_bearer(ctx: &Ctx, serial: &str, bearer: &str) -> anyhow::Result<()> {
    if !is_demo(serial) {
        return Ok(());
    }
    let mut state = read_state(ctx)?;
    state.bearers.insert(serial.to_owned(), bearer.to_owned());
    write_state(ctx, &state)
}

pub fn bearers(ctx: &Ctx) -> anyhow::Result<BTreeMap<String, String>> {
    Ok(read_state(ctx)?.bearers)
}

pub fn welfare(ctx: &Ctx, serial: &str) -> anyhow::Result<WelfareState> {
    Ok(read_state(ctx)?.welfare.get(serial).cloned().unwrap_or_default())
}

pub fn save_welfare(ctx: &Ctx, serial: &str, welfare: WelfareState) -> anyhow::Result<()> {
    let mut state = read_state(ctx)?;
    state.welfare.insert(serial.to_owned(), welfare);
    write_state(ctx, &state)
}

/// Explicit caretaker/server clear for a hosted simulated collar.
pub fn clear_welfare(ctx: &Ctx, serial: &str) -> anyhow::Result<()> {
    let mut state = read_state(ctx)?;
    state.welfare.entry(serial.to_owned()).or_default().clear_shutoff();
    write_state(ctx, &state)
}

pub fn is_demo(serial: &str) -> bool {
    serial.starts_with("OPR-ROOK-") || serial.starts_with("OPS-STARLING-")
}

fn seed(serial: &str) -> [u8; 32] {
    Sha256::digest(format!("openpasture-mobile-demo-v1:{serial}")).into()
}

fn grouped(code: &str) -> String {
    code.as_bytes().chunks(4).map(|c| String::from_utf8_lossy(c).into_owned()).collect::<Vec<_>>().join("-")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn read_state(ctx: &Ctx) -> anyhow::Result<State> {
    let path = ctx.data_dir().join(KEY_FILE);
    match std::fs::read_to_string(&path) {
        Ok(value) => Ok(serde_json::from_str(&value)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error.into()),
    }
}

fn write_state(ctx: &Ctx, state: &State) -> anyhow::Result<()> {
    op_core::secrets::write_private(&ctx.data_dir().join(KEY_FILE), serde_json::to_vec_pretty(state)?.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards_are_stable_and_parse() {
        let cards = cards();
        for card in &cards {
            println!("{}|{}|{}|{}", card.serial, card.model, card.claim_code, card.card);
        }
        assert_eq!(cards.len(), 16);
        assert_eq!(cards[0].serial, "OPR-ROOK-0001");
        assert!(cards.iter().all(|c| op_protocol::v2::identity::Card::parse(&c.card).is_some()));
    }

    #[test]
    fn boundary_rearm_keeps_welfare_but_caretaker_clear_resets_it() {
        let mut state = WelfareState {
            armed: true,
            outside_cues: 3,
            pulse_cutoff: true,
            crossed_out: true,
            escape_cycles: 3,
            escape_window_start_ms: 42,
            enforcement_off: true,
            ..Default::default()
        };
        state.rearm(7);
        assert_eq!(state.boundary_version, 7);
        assert_eq!(state.escape_cycles, 3);
        assert_eq!(state.escape_window_start_ms, 42);
        assert!(state.enforcement_off);
        assert_eq!(state.outside_cues, 0);
        assert!(!state.pulse_cutoff);

        state.clear_shutoff();
        assert_eq!(state.escape_cycles, 0);
        assert_eq!(state.escape_window_start_ms, 0);
        assert!(!state.enforcement_off);
    }
}
