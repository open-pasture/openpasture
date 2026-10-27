//! Keys and ids (protocol v2 `identity.md`): device ids and kids, public keys
//! as base64 or base64url, and this server's maker key, which signs the
//! factory records of the units it provisions.

use std::path::Path;

use anyhow::Context;
use base64::Engine;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use op_protocol::{SigningKey, VerifyingKey};

pub use op_protocol::v2::json::{hex, unhex};

/// The maker key's file in the data directory (base64 secret, 0600).
pub const MAKER_KEY_FILE: &str = "maker_ed25519.key";

/// A key's id: hex of the first 8 bytes of SHA-256 over its 32 bytes. For a
/// device key this is the v2 device id.
pub fn kid(key: &VerifyingKey) -> String {
    hex(&op_protocol::v2::crypto::kid(key.as_bytes()))
}

/// A public key as base64 (with or without padding) or base64url (the card's
/// form). Farmer-readable error.
pub fn parse_public_key(text: &str, what: &str) -> Result<VerifyingKey, String> {
    let t = text.trim();
    let bytes = STANDARD
        .decode(t)
        .ok()
        .or_else(|| URL_SAFE_NO_PAD.decode(t.trim_end_matches('=')).ok())
        .and_then(|b| <[u8; 32]>::try_from(b).ok())
        .ok_or_else(|| format!("{what} isn't an Ed25519 public key (32 bytes, base64)."))?;
    VerifyingKey::from_bytes(&bytes).map_err(|_| format!("{what} isn't an Ed25519 public key."))
}

/// A kid as written in JSON: 16 lowercase hex digits.
pub fn parse_kid(text: &str) -> Option<[u8; 8]> {
    let t = text.trim().to_ascii_lowercase();
    unhex(&t).and_then(|b| b.try_into().ok())
}

/// This server's maker key, made the first time it is needed.
pub fn maker_key(data_dir: &Path) -> anyhow::Result<SigningKey> {
    let path = data_dir.join(MAKER_KEY_FILE);
    if path.exists() {
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        return op_protocol::decode_signing_key(&text).map_err(|_| anyhow::anyhow!("{} is not a valid key", path.display()));
    }
    let key = op_protocol::generate_signing_key();
    op_core::secrets::write_private(&path, op_protocol::encode_signing_key(&key).as_bytes())?;
    tracing::info!("generated maker key");
    Ok(key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kid_is_16_hex_of_the_key_hash() {
        let key = SigningKey::from_bytes(&[7u8; 32]).verifying_key();
        let k = kid(&key);
        assert_eq!(k.len(), 16);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(parse_kid(&k), Some(op_protocol::v2::crypto::kid(key.as_bytes())));
        assert_eq!(parse_kid("xyz"), None);
    }

    #[test]
    fn public_keys_read_as_base64_or_base64url() {
        let key = SigningKey::from_bytes(&[3u8; 32]).verifying_key();
        assert_eq!(parse_public_key(&STANDARD.encode(key.as_bytes()), "key").unwrap(), key);
        assert_eq!(parse_public_key(&URL_SAFE_NO_PAD.encode(key.as_bytes()), "key").unwrap(), key);
        assert!(parse_public_key("not base64!", "The device key").unwrap_err().starts_with("The device key"));
        assert!(parse_public_key(&STANDARD.encode([0u8; 16]), "key").is_err());
    }

    #[test]
    fn the_maker_key_is_made_once() {
        let dir = tempfile::tempdir().unwrap();
        let a = maker_key(dir.path()).unwrap();
        let b = maker_key(dir.path()).unwrap();
        assert_eq!(a.to_bytes(), b.to_bytes());
    }
}
