//! A unit's label: 50 × 25 mm, for a label printer at the station. Serial,
//! class and hardware revision, the claim code in groups of four, and a QR
//! code holding the card text (protocol v2 `identity.md` §3,
//! `OC2:<serial>:<device key>:<claim code>`), so any server can claim it.

use axum::Router;
use axum::extract::{Path, State};
use axum::http::header;
use axum::response::IntoResponse;
use axum::routing::get;
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

/// The text column right of the QR code, in mm.
const TEXT_X: f64 = 25.0;
const TEXT_W: f64 = 24.0;
/// A monospace glyph's advance as a share of the font size: Menlo and
/// DejaVu Sans Mono are 0.602, Courier 0.600. Every line is also given this
/// length (`textLength`), so whatever font the printer has, it fits.
const ADVANCE: f64 = 0.602;
/// Below this a line wraps rather than shrinks (about 6 pt).
const MIN_SIZE: f64 = 2.1;

/// One line of text, sized to fit the column: `size` mm, or smaller.
fn line(text: &str, y: f64, size: f64, bold: bool) -> String {
    let n = text.chars().count().max(1) as f64;
    // Printed to 0.01 mm, rounded down so the printed size still fits.
    let size = (size.min(TEXT_W / (n * ADVANCE)) * 100.0).floor() / 100.0;
    let width = n * ADVANCE * size;
    format!(
        r#"<text x="{TEXT_X}" y="{y:.2}" font-size="{size:.2}"{} textLength="{width:.2}" lengthAdjust="spacingAndGlyphs">{}</text>"#,
        if bold { r#" font-weight="700""# } else { "" },
        esc(text)
    )
}

/// Text that doesn't fit one line at `MIN_SIZE`, in two: split at the `-`
/// nearest the middle, else in the middle.
fn split(text: &str, size: f64) -> Vec<String> {
    let n = text.chars().count();
    if TEXT_W / (n.max(1) as f64 * ADVANCE) >= size.min(MIN_SIZE) {
        return vec![text.to_owned()];
    }
    let chars: Vec<char> = text.chars().collect();
    let mid = n / 2;
    let cut = (0..n).filter(|&i| chars[i] == '-').min_by_key(|&i| i.abs_diff(mid)).filter(|&i| i.abs_diff(mid) <= n / 4).map(|i| i + 1).unwrap_or(mid);
    vec![chars[..cut].iter().collect(), chars[cut..].iter().collect()]
}

/// The label as SVG, sized in millimetres. Serial, class and board, and the
/// claim code (two lines of two groups) fit the column at their longest:
/// a 32-character serial, a 32-character class and a 16-character board.
pub fn svg(unit: &Unit, claim_code: &str) -> anyhow::Result<String> {
    let qr = qr_path(&payload(unit, claim_code), 1.0, 1.0, 23.0)?;
    let mut text = String::new();
    let serial = split(&unit.serial, 3.4);
    let top = if serial.len() == 1 { 6.0 } else { 4.6 };
    for (i, l) in serial.iter().enumerate() {
        text.push_str(&line(l, top + i as f64 * 3.0, 3.4, true));
    }
    text.push_str(&line(&format!("{} {}", unit.device_class, unit.hw_rev), 11.2, 2.4, false));
    text.push_str(&line("claim", 15.2, 2.0, false));
    let grouped = crate::provision::grouped(claim_code);
    let (a, b) = grouped.split_at(grouped.len().min(9));
    text.push_str(&line(a, 19.0, 3.2, true));
    text.push_str(&line(b.trim_start_matches('-'), 22.8, 3.2, true));
    Ok(format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="50mm" height="25mm" viewBox="0 0 50 25"><rect width="50" height="25" fill="#fff"/>{qr}<g font-family="ui-monospace, Menlo, 'DejaVu Sans Mono', monospace" fill="#000">{text}</g></svg>"##
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
            generation: 0,
            generation_at: None,
            origin: Origin::Station,
            station: None,
            claim_code: Some("7Q4K9M2XH8D3TV5W".into()),
            claim_checked: true,
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
        assert!(s.contains(">7Q4K-9M2X<") && s.contains(">H8D3-TV5W<"), "grouped, two lines");
        let card = op_protocol::v2::identity::Card::parse(&payload(&unit(), "7Q4K9M2XH8D3TV5W")).unwrap();
        assert_eq!(card.serial, "OCN-2611-00123");
        assert_eq!(card.claim_code, "7Q4K9M2XH8D3TV5W");
        assert!(esc("<&>").contains("&lt;&amp;&gt;"));
    }

    /// Every text line: (x, font size, textLength, text).
    fn lines(svg: &str) -> Vec<(f64, f64, f64, String)> {
        let attr = |t: &str, name: &str| -> f64 {
            let at = t.find(&format!(" {name}=\"")).unwrap() + name.len() + 3;
            t[at..t[at..].find('"').unwrap() + at].parse().unwrap()
        };
        svg.split("<text")
            .skip(1)
            .map(|t| {
                let body = &t[t.find('>').unwrap() + 1..t.find("</text>").unwrap()];
                (attr(t, "x"), attr(t, "font-size"), attr(t, "textLength"), body.to_owned())
            })
            .collect()
    }

    /// Review P2-05: the grouped code (19 glyphs) and the example serial ran
    /// past the 50 mm edge. At the longest values everything ends inside the
    /// label, is given exactly that length, and stays readable.
    #[test]
    fn the_longest_serial_and_code_fit_the_label() {
        for (serial, class, hw) in
            [("OCN-2611-00123", "node", "N1.0"), ("OCN-2611-0012345-ABCDEFGHIJ-WWWW", "anchor-with-a-long-class-name-xy", "N1.0-REV-B-12345")]
        {
            assert!(op_protocol::v2::identity::serial_ok(serial) && serial.len() <= 32);
            let u = Unit { serial: serial.into(), device_class: class.into(), hw_rev: hw.into(), ..unit() };
            let s = svg(&u, "WWWWMMMMWWWWMMMM").unwrap();
            let all = lines(&s);
            for (x, size, len, text) in &all {
                let est = text.chars().count() as f64 * ADVANCE * size;
                assert!((len - est).abs() < 0.02, "{text}: textLength {len} vs {est}");
                assert!(x + len <= 49.01, "{text} ends at {} mm", x + len);
            }
            let code: Vec<_> = all.iter().filter(|l| l.3.starts_with("WWWW") || l.3.starts_with("MMMM")).collect();
            assert_eq!(code.len(), 2);
            assert!(code.iter().all(|l| l.1 >= 3.0), "the code reads at 3 mm: {code:?}");
            let serial_lines: Vec<_> = all.iter().take_while(|l| l.3 != format!("{class} {hw}")).collect();
            assert_eq!(serial_lines.iter().map(|l| l.3.as_str()).collect::<String>(), serial, "all of the serial");
            assert!(serial_lines.iter().all(|l| l.1 >= MIN_SIZE), "{serial_lines:?}");
        }
    }
}
