//! The primitives v2 is built from: SHA-256, HMAC-SHA-256, HKDF-SHA-256,
//! X25519 from Ed25519 keys, and AES-128-CCM with an 8-byte tag (RFC 3610,
//! L = 2, 13-byte nonce). See `opencollar/protocol/v2/identity.md`.

use aes::Aes128;
use aes::cipher::{BlockCipherEncrypt, KeyInit};
use ed25519_dalek::{SigningKey, VerifyingKey};
use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// Salt of every key derivation, and the COSE external data: binds keys and
/// signatures to this protocol version.
pub const CONTEXT: &[u8] = b"opencollar/2";

/// Length of a frame's authentication tag.
pub const TAG_BYTES: usize = 8;
/// Length of an AES-CCM nonce.
pub const NONCE_BYTES: usize = 13;

pub fn sha256(parts: &[&[u8]]) -> [u8; 32] {
    let mut h = Sha256::new();
    for p in parts {
        h.update(p);
    }
    h.finalize().into()
}

/// A key's id: the first 8 bytes of SHA-256 of the 32-byte public key. A
/// collar's device id is the kid of its device key.
pub fn kid(public_key: &[u8; 32]) -> [u8; 8] {
    sha256(&[public_key])[..8].try_into().unwrap()
}

pub fn hmac_sha256(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes any key length");
    for p in parts {
        m.update(p);
    }
    m.finalize().into_bytes().into()
}

/// HKDF-SHA-256 (RFC 5869) with `salt` = [`CONTEXT`], for up to 32 bytes.
pub fn hkdf(ikm: &[u8], info: &[&[u8]], len: usize) -> Vec<u8> {
    assert!(len <= 32, "one HKDF block");
    let prk = hmac_sha256(CONTEXT, &[ikm]);
    let mut parts: Vec<&[u8]> = info.to_vec();
    parts.push(&[1]);
    hmac_sha256(&prk, &parts)[..len].to_vec()
}

/// X25519 between our Ed25519 key and theirs, both in X25519 form (the
/// birational map libsodium and Monocypher use).
pub fn x25519(ours: &SigningKey, theirs: &VerifyingKey) -> [u8; 32] {
    theirs.to_montgomery().mul_clamped(ours.to_scalar_bytes()).0
}

/// The key a collar's frames are sealed with while it is bound to the server
/// key with id `server_kid`. Either side computes it: the collar from its
/// device key and the server's public key, the server the other way round.
pub fn frame_key(shared: &[u8; 32], device_id: &[u8; 8], server_kid: &[u8; 8]) -> [u8; 16] {
    hkdf(shared, &[b"frame", device_id, server_kid], 16).try_into().unwrap()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpenError;

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("frame does not authenticate")
    }
}

impl std::error::Error for OpenError {}

/// AES-128-CCM: `plaintext` encrypted, followed by the 8-byte tag.
pub fn ccm_seal(key: &[u8; 16], nonce: &[u8; NONCE_BYTES], aad: &[u8], plaintext: &[u8]) -> Vec<u8> {
    let aes = Aes128::new(&(*key).into());
    let tag = cbc_mac(&aes, nonce, aad, plaintext);
    let mut out = plaintext.to_vec();
    ctr(&aes, nonce, &mut out);
    let s0 = block(&aes, &ctr_block(nonce, 0));
    out.extend(tag.iter().zip(s0).map(|(t, s)| t ^ s));
    out
}

/// The plaintext of a [`ccm_seal`] output, if the tag verifies.
pub fn ccm_open(key: &[u8; 16], nonce: &[u8; NONCE_BYTES], aad: &[u8], sealed: &[u8]) -> Result<Vec<u8>, OpenError> {
    let split = sealed.len().checked_sub(TAG_BYTES).ok_or(OpenError)?;
    let (ct, tag) = sealed.split_at(split);
    let aes = Aes128::new(&(*key).into());
    let mut pt = ct.to_vec();
    ctr(&aes, nonce, &mut pt);
    let s0 = block(&aes, &ctr_block(nonce, 0));
    let want = cbc_mac(&aes, nonce, aad, &pt);
    // Constant time over the tag.
    let diff = want.iter().zip(s0).zip(tag).fold(0u8, |acc, ((w, s), t)| acc | (w ^ s ^ t));
    if diff == 0 { Ok(pt) } else { Err(OpenError) }
}

fn block(aes: &Aes128, input: &[u8; 16]) -> [u8; 16] {
    let mut b = (*input).into();
    aes.encrypt_block(&mut b);
    b.into()
}

fn ctr_block(nonce: &[u8; NONCE_BYTES], i: u16) -> [u8; 16] {
    let mut a = [0u8; 16];
    a[0] = 0x01; // L − 1
    a[1..14].copy_from_slice(nonce);
    a[14..].copy_from_slice(&i.to_be_bytes());
    a
}

fn ctr(aes: &Aes128, nonce: &[u8; NONCE_BYTES], data: &mut [u8]) {
    assert!(data.len() < 0xffff * 16, "CCM with L = 2 carries under 1 MB");
    for (i, chunk) in data.chunks_mut(16).enumerate() {
        let s = block(aes, &ctr_block(nonce, i as u16 + 1));
        for (d, k) in chunk.iter_mut().zip(s) {
            *d ^= k;
        }
    }
}

fn cbc_mac(aes: &Aes128, nonce: &[u8; NONCE_BYTES], aad: &[u8], msg: &[u8]) -> [u8; TAG_BYTES] {
    assert!(msg.len() <= 0xffff && aad.len() < 0xff00, "CCM lengths");
    let mut b0 = [0u8; 16];
    // Adata, M' = (8 − 2) / 2, L' = 1.
    b0[0] = if aad.is_empty() { 0 } else { 0x40 } | (((TAG_BYTES as u8 - 2) / 2) << 3) | 0x01;
    b0[1..14].copy_from_slice(nonce);
    b0[14..].copy_from_slice(&(msg.len() as u16).to_be_bytes());
    let mut x = block(aes, &b0);
    let absorb = |data: &[u8], x: &mut [u8; 16]| {
        for chunk in data.chunks(16) {
            for (xi, d) in x.iter_mut().zip(chunk) {
                *xi ^= d;
            }
            *x = block(aes, x);
        }
    };
    if !aad.is_empty() {
        let mut a = (aad.len() as u16).to_be_bytes().to_vec();
        a.extend(aad);
        absorb(&a, &mut x);
    }
    absorb(msg, &mut x);
    x[..TAG_BYTES].try_into().unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// RFC 3610 packet vector #1 (M = 8, L = 2, 13-byte nonce, 8 header bytes).
    #[test]
    fn rfc3610_packet_vector_1() {
        let key: [u8; 16] = unhex("C0C1C2C3C4C5C6C7C8C9CACBCCCDCECF").try_into().unwrap();
        let nonce: [u8; 13] = unhex("00000003020100A0A1A2A3A4A5").try_into().unwrap();
        let aad = unhex("0001020304050607");
        let pt = unhex("08090A0B0C0D0E0F101112131415161718191A1B1C1D1E");
        let sealed = ccm_seal(&key, &nonce, &aad, &pt);
        assert_eq!(sealed, unhex("588C979A61C663D2F066D0C2C0F989806D5F6B61DAC38417E8D12CFDF926E0"));
        assert_eq!(ccm_open(&key, &nonce, &aad, &sealed).unwrap(), pt);
        let mut bad = sealed.clone();
        bad[3] ^= 1;
        assert_eq!(ccm_open(&key, &nonce, &aad, &bad), Err(OpenError));
        assert_eq!(ccm_open(&key, &nonce, &unhex("0001020304050606"), &sealed), Err(OpenError));
    }

    /// RFC 3610 packet vector #2: 24 bytes of payload, a block boundary.
    #[test]
    fn rfc3610_packet_vector_2() {
        let key: [u8; 16] = unhex("C0C1C2C3C4C5C6C7C8C9CACBCCCDCECF").try_into().unwrap();
        let nonce: [u8; 13] = unhex("00000004030201A0A1A2A3A4A5").try_into().unwrap();
        let sealed = ccm_seal(&key, &nonce, &unhex("0001020304050607"), &unhex("08090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F"));
        assert_eq!(sealed, unhex("72C91A36E135F8CF291CA894085C87E3CC15C439C9E43A3BA091D56E10400916"));
    }

    #[test]
    fn x25519_agrees_both_ways() {
        let a = SigningKey::from_bytes(&[1; 32]);
        let b = SigningKey::from_bytes(&[2; 32]);
        assert_eq!(x25519(&a, &b.verifying_key()), x25519(&b, &a.verifying_key()));
        // libsodium: crypto_scalarmult(crypto_sign_ed25519_sk_to_curve25519(a),
        // crypto_sign_ed25519_pk_to_curve25519(B)), seeds 01… and 02…
        assert_eq!(x25519(&a, &b.verifying_key()).to_vec(), unhex("4181d7302557342bdb6d061c4b1eebea828ecb625c3368b7111680793307220b"));
    }

    /// RFC 5869 test case 1 doesn't apply (our salt is fixed), so check the
    /// definition directly.
    #[test]
    fn hkdf_is_extract_then_expand() {
        let prk = hmac_sha256(CONTEXT, &[b"ikm"]);
        assert_eq!(hkdf(b"ikm", &[b"in", b"fo"], 16), hmac_sha256(&prk, &[b"info", &[1]])[..16].to_vec());
    }
}
