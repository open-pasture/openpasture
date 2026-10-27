//! A unit's label: 50 × 25 mm, for a label printer at the station. Serial,
//! class and hardware revision, the claim code in groups of four, and a QR
//! code holding the card text (protocol v2 `identity.md` §3,
//! `OC2:<serial>:<device key>:<claim code>`), so any server can claim it.

use axum::extract::{Path, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::Router;
use op_core::{ApiError, ApiResult, Ctx};
use qrcode::{Color, EcLevel, QrCode};

use crate::units::{self, Unit};

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/fleet/units/{serial}/label.svg", get(label))
}

/// The QR payload: the card text.
pub fn payload(unit: &Unit, claim_code: &str) -> String {
    crate::record::card(unit, claim_code).unwrap_or_default()
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

/// The QR code as one path in a `size` mm square at (x, y), quiet zone included.
fn qr_path(data: &str, x: f64, y: f64, size: f64) -> anyhow::Result<String> {
    let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::M)?;
    let w = code.width();
    let quiet = 2;
    let m = size / (w + 2 * quiet) as f64;
    let mut d = String::new();
    for (i, c) in code.to_colors().iter().enumerate() {
        if *c == Color::Dark {
            let (cx, cy) = (i % w + quiet, i / w + quiet);
            d.push_str(&format!("M{:.3} {:.3}h{m:.3}v{m:.3}h-{m:.3}z", x + cx as f64 * m, y + cy as f64 * m));
        }
    }
    Ok(format!(r##"<path d="{d}" fill="#000"/>"##))
}

/// The label as SVG, sized in millimetres.
pub fn svg(unit: &Unit, claim_code: &str) -> anyhow::Result<String> {
    let qr = qr_path(&payload(unit, claim_code), 1.0, 1.0, 23.0)?;
    Ok(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="50mm" height="25mm" viewBox="0 0 50 25"><rect width="50" height="25" fill="#fff"/>{qr}<g font-family="ui-monospace, Menlo, monospace" fill="#000"><text x="25" y="6.5" font-size="3.4" font-weight="700">{serial}</text><text x="25" y="10.5" font-size="2.4">{class} {hw}</text><text x="25" y="17" font-size="2">claim</text><text x="25" y="20.5" font-size="3" font-weight="700">{code}</text></g></svg>"##,
        serial = esc(&unit.serial),
        class = esc(&unit.device_class),
        hw = esc(&unit.hw_rev),
        code = esc(&crate::provision::grouped(claim_code)),
    ))
}

async fn label(State(ctx): State<Ctx>, Path(serial): Path<String>) -> ApiResult<impl IntoResponse> {
    let unit = units::find(&ctx, &serial).await?;
    let code = unit.claim_code.clone().ok_or_else(|| ApiError::conflict(format!("{serial} has no claim code until it is provisioned.")))?;
    Ok(([(header::CONTENT_TYPE, "image/svg+xml"), (header::CACHE_CONTROL, "no-store")], svg(&unit, &code)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::units::{Origin, Radios, UnitState};

    fn unit() -> Unit {
        let key = op_protocol::SigningKey::from_bytes(&[5u8; 32]).verifying_key();
        Unit {
            serial: "OCN-2611-00123".into(),
            device_class: "node".into(),
            hw_rev: "N1.0".into(),
            fw_version: "1.0.0".into(),
            device_pubkey: op_protocol::encode_public_key(&key),
            device_kid: "k".into(),
            device_id: "k".into(),
            radios: Radios::default(),
            image_keys: vec![],
            caps: vec![],
            state: UnitState::Provisioned,
            origin: Origin::Station,
            station: None,
            claim_code: Some("7Q4K9M2XH8D3TV5W".into()),
            collar_id: None,
            herd_id: None,
            last_test: None,
            maker: None,
            maker_kid: None,
            maker_verified: false,
            made_at: None,
            provisioned_at: None,
            claimed_at: None,
            created_at: op_core::time::now(),
            updated_at: op_core::time::now(),
        }
    }

    #[test]
    fn the_label_carries_serial_code_and_the_card() {
        let s = svg(&unit(), "7Q4K9M2XH8D3TV5W").unwrap();
        assert!(s.starts_with("<svg") && s.contains(r#"width="50mm""#));
        assert!(s.contains("OCN-2611-00123"));
        assert!(s.contains("7Q4K-9M2X-H8D3-TV5W"), "grouped");
        let card = op_protocol::v2::identity::Card::parse(&payload(&unit(), "7Q4K9M2XH8D3TV5W")).unwrap();
        assert_eq!(card.serial, "OCN-2611-00123");
        assert_eq!(card.claim_code, "7Q4K9M2XH8D3TV5W");
        assert!(esc("<&>").contains("&lt;&amp;&gt;"));
    }
}
