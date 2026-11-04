//! Portable cross-farm transfer token: `OPT1:serial:device_key:collar_id:code:exp`.
//! Produced by a releasing farm; claimed on the receiving farm via
//! `POST /api/fleet/claim` with `transfer_token` (or the card).

use chrono::{DateTime, Utc};
use op_protocol::v2::identity::normalize_claim_code;

/// `OPT1:<serial>:<device_key_b64url>:<collar_id>:<code>:<exp_unix>`
pub fn format(serial: &str, device_pubkey: &str, collar_id: &str, code: &str, exp: DateTime<Utc>) -> String {
    format!("OPT1:{serial}:{device_pubkey}:{collar_id}:{code}:{}", exp.timestamp())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferToken {
    pub serial: String,
    pub device_pubkey: String,
    pub collar_id: String,
    pub transfer_code: String,
    pub expires_at: DateTime<Utc>,
}

pub fn parse(s: &str) -> Option<TransferToken> {
    let mut parts = s.trim().split(':');
    if parts.next()? != "OPT1" {
        return None;
    }
    let serial = parts.next()?.to_owned();
    let device_pubkey = parts.next()?.to_owned();
    let collar_id = parts.next()?.to_owned();
    let transfer_code = normalize_claim_code(parts.next()?)?;
    let exp: i64 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let expires_at = DateTime::from_timestamp(exp, 0)?;
    Some(TransferToken { serial, device_pubkey, collar_id, transfer_code, expires_at })
}

#[cfg(test)]
mod tests {
    use super::*;
    use op_protocol::v2::identity::new_claim_code;

    #[test]
    fn round_trip() {
        let code = new_claim_code();
        let exp = DateTime::from_timestamp(1_900_000_000, 0).unwrap();
        let t = format("OCN-1", "pk", "col_1", &code, exp);
        let p = parse(&t).unwrap();
        assert_eq!(p.serial, "OCN-1");
        assert_eq!(p.collar_id, "col_1");
        assert_eq!(p.transfer_code, code);
        assert_eq!(p.expires_at, exp);
    }
}
