//! The server's Ed25519 key pair, for signing boundaries. Lives in the data
//! directory as `server_ed25519.key` (base64 secret, 0600).

use std::path::Path;

use anyhow::Context;
use op_protocol::{SigningKey, decode_signing_key, encode_signing_key, generate_signing_key};

pub const KEY_FILE: &str = "server_ed25519.key";

pub fn load_or_create(data_dir: &Path) -> anyhow::Result<SigningKey> {
    let path = data_dir.join(KEY_FILE);
    if path.exists() {
        let text = std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        return decode_signing_key(&text).map_err(|_| anyhow::anyhow!("{} is not a valid key", path.display()));
    }
    let key = generate_signing_key();
    crate::secrets::write_private(&path, encode_signing_key(&key).as_bytes())?;
    tracing::info!("generated server signing key");
    Ok(key)
}

/// sha256 hex of a collar key, as stored in `collars.key_hash`.
pub fn hash_key(key: &str) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(key.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

/// A fresh random collar key (hex, 32 bytes).
pub fn new_collar_key() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}
