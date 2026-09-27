//! Provisioning cards: one QR code per collar holding everything the collar
//! needs to join this farm, `{"v":1,"c":collar,"h":herd,"k":key,"e":endpoint,"s":server key}`
//! (compact JSON, error correction M). A USB scanner types it into the
//! collar's serial console after `provision `.
//!
//! Keys travel only in request bodies and exist only in the link or rekey
//! response, so a card can be printed only from that response; printing again
//! means a new key. The endpoint is baked into the collar, so cards need the
//! server's public https URL (a LAN or tunnel address would strand it).

use axum::extract::{DefaultBodyLimit, State};
use axum::routing::post;
use axum::{Json, Router};
use op_core::{ApiError, ApiJson, ApiResult, Ctx};
use qrcode::{EcLevel, QrCode};
use serde::{Deserialize, Serialize};

const MAX_ITEMS: usize = 2000;

pub fn router() -> Router<Ctx> {
    Router::new().route("/api/cards", post(cards).layer(DefaultBodyLimit::max(2 * 1024 * 1024)))
}

/// What a collar's card holds, in this order.
#[derive(Serialize)]
struct Provision<'a> {
    v: u8,
    c: &'a str,
    h: &'a str,
    k: &'a str,
    e: &'a str,
    s: &'a str,
}

/// The card payload: compact JSON, keys in the order `v c h k e s`.
pub fn payload(collar_id: &str, herd_id: &str, key: &str, endpoint: &str, server_key: &str) -> String {
    serde_json::to_string(&Provision { v: 1, c: collar_id, h: herd_id, k: key, e: endpoint, s: server_key }).expect("plain strings")
}

/// The payload as an SVG QR code (error correction M, one unit per module,
/// quiet zone included), ready to inline in a page.
pub fn qr_svg(payload: &str) -> anyhow::Result<String> {
    let code = QrCode::with_error_correction_level(payload.as_bytes(), EcLevel::M)?;
    let svg = code.render::<qrcode::render::svg::Color>().module_dimensions(1, 1).quiet_zone(true).build();
    // Inline in HTML: no XML declaration.
    Ok(svg.find("<svg").map_or(svg.clone(), |i| svg[i..].to_owned()))
}

/// The endpoint that goes on cards: the public https URL, or why there is none.
pub async fn card_endpoint(ctx: &Ctx) -> ApiResult<String> {
    match ctx.settings().await?.server.public_url {
        Some(u) if u.starts_with("https://") => Ok(format!("{}/collar/v1", u.trim_end_matches('/'))),
        Some(_) => Err(ApiError::conflict("Cards need an https public URL: collars only talk to https.")),
        None => Err(ApiError::conflict("Cards need the server's public URL (Settings, Server): it goes into every collar.")),
    }
}

#[derive(Deserialize)]
struct CardsBody {
    items: Vec<CardItem>,
}

#[derive(Deserialize)]
struct CardItem {
    collar_id: String,
    key: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Card {
    pub collar_id: String,
    pub qr_svg: String,
}

async fn cards(State(ctx): State<Ctx>, ApiJson(body): ApiJson<CardsBody>) -> ApiResult<Json<Vec<Card>>> {
    let endpoint = card_endpoint(&ctx).await?;
    if body.items.len() > MAX_ITEMS {
        return Err(ApiError::bad_request(format!("At most {MAX_ITEMS} cards at once.")));
    }
    let server_key = ctx.public_key_b64();
    let mut out = Vec::with_capacity(body.items.len());
    for it in body.items {
        let row = sqlx::query_as::<_, (String, String)>("SELECT herd_id, name FROM collars WHERE id = ?").bind(&it.collar_id).fetch_optional(ctx.db()).await?;
        let (herd_id, name) = row.ok_or_else(|| ApiError::not_found("No such collar."))?;
        if !op_ingest::collar_key_matches(&ctx, &it.collar_id, &it.key).await? {
            return Err(ApiError::bad_request(format!("That isn't {name}'s key now. Give it a new key to print its card.")));
        }
        let qr_svg = qr_svg(&payload(&it.collar_id, &herd_id, &it.key, &endpoint, &server_key))?;
        out.push(Card { collar_id: it.collar_id, qr_svg });
    }
    Ok(Json(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payload_is_compact_and_in_order() {
        assert_eq!(
            payload("col_1", "herd_1", "ab", "https://farm.example.com/collar/v1", "S="),
            r#"{"v":1,"c":"col_1","h":"herd_1","k":"ab","e":"https://farm.example.com/collar/v1","s":"S="}"#
        );
    }

    #[test]
    fn svg_is_inline_ready() {
        let s = qr_svg("hello").unwrap();
        assert!(s.starts_with("<svg"), "{s}");
        assert!(s.ends_with("</svg>"));
    }
}
