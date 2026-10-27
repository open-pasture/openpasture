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

/// One maker key made at a time in this process; across processes the
/// hard link below decides.
static MAKER_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn read_maker(path: &Path) -> anyhow::Result<Option<SigningKey>> {
    match std::fs::read_to_string(path) {
        Ok(text) => op_protocol::decode_signing_key(&text).map(Some).map_err(|_| anyhow::anyhow!("{} is not a valid key", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// This server's maker key, made the first time it is needed (and at
/// start, [`crate::start`]). Every caller gets the key that is on disk: a
/// new key is written whole to its own temporary file and linked into
/// place only if no key is there yet, so two first uses at once (two
/// requests, or two processes) agree, and nothing is signed with a key
/// that was never kept.
pub fn maker_key(data_dir: &Path) -> anyhow::Result<SigningKey> {
    let path = data_dir.join(MAKER_KEY_FILE);
    let _one = MAKER_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(key) = read_maker(&path)? {
        return Ok(key);
    }
    let fresh = op_protocol::generate_signing_key();
    let tmp = data_dir.join(format!(".{MAKER_KEY_FILE}.{}.tmp", ulid::Ulid::new()));
    let linked = write_new(&tmp, op_protocol::encode_signing_key(&fresh).as_bytes()).and_then(|()| match std::fs::hard_link(&tmp, &path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
        Err(e) => Err(anyhow::Error::from(e).context(format!("writing {}", path.display()))),
    });
    let _ = std::fs::remove_file(&tmp);
    if linked? {
        if let Ok(dir) = std::fs::File::open(data_dir) {
            let _ = dir.sync_all();
        }
        tracing::info!("generated maker key");
    }
    read_maker(&path)?.ok_or_else(|| anyhow::anyhow!("{} vanished", path.display()))
}

/// A new file readable only by the owner, synced.
fn write_new(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(path).with_context(|| format!("writing {}", path.display()))?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
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
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join(MAKER_KEY_FILE)).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    /// Review P1-07: 32 first uses at once on an empty directory. Every one
    /// succeeds with the key that ends up on disk, and no temporary is left.
    #[test]
    fn concurrent_first_uses_all_get_the_persisted_key() {
        for _ in 0..3 {
            let dir = tempfile::tempdir().unwrap();
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(32));
            let got: Vec<anyhow::Result<[u8; 32]>> = std::thread::scope(|s| {
                let hs: Vec<_> = (0..32)
                    .map(|_| {
                        let b = barrier.clone();
                        let d = dir.path();
                        s.spawn(move || {
                            b.wait();
                            maker_key(d).map(|k| k.to_bytes())
                        })
                    })
                    .collect();
                hs.into_iter().map(|h| h.join().unwrap()).collect()
            });
            let on_disk = read_maker(&dir.path().join(MAKER_KEY_FILE)).unwrap().unwrap().to_bytes();
            for g in got {
                assert_eq!(g.unwrap(), on_disk);
            }
            let left: Vec<_> = std::fs::read_dir(dir.path()).unwrap().map(|e| e.unwrap().file_name()).collect();
            assert_eq!(left, vec![std::ffi::OsString::from(MAKER_KEY_FILE)]);
        }
    }

    /// Two processes racing is the same as a key appearing between the read
    /// and the link: the one on disk wins and is returned.
    #[test]
    fn a_key_already_linked_wins() {
        let dir = tempfile::tempdir().unwrap();
        let theirs = op_protocol::generate_signing_key();
        std::fs::write(dir.path().join(MAKER_KEY_FILE), op_protocol::encode_signing_key(&theirs)).unwrap();
        assert_eq!(maker_key(dir.path()).unwrap().to_bytes(), theirs.to_bytes());
    }
}
