//! COSE_Sign1 (RFC 9052) with EdDSA, the envelope of every signed v2 message.
//!
//! ```text
//! 18([ protected: bstr .cbor {1: -8, 4: kid}, unprotected: {}, payload: bstr, signature: bstr .size 64 ])
//! Sig_structure = ["Signature1", protected, external_aad = "opencollar/2", payload]
//! ```
//!
//! The signature covers the payload bytes exactly as sent, so any transport
//! carries a signed message as an opaque byte string and nothing is ever
//! re-encoded. The unprotected bucket is always empty: anything in it would
//! change the message's bytes (its digest, its LoRa message id) without
//! touching the signature.

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};

use super::cbor::{self, IntMap, Value};
use super::crypto::{CONTEXT, kid};
use super::registry::Reject;

/// The CBOR tag of COSE_Sign1.
pub const TAG: u64 = 18;
/// COSE algorithm EdDSA.
pub const ALG_EDDSA: i128 = -8;

/// A parsed COSE_Sign1 whose signature has not been checked yet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sign1 {
    /// The protected header as received; the signature covers these bytes.
    pub protected: Vec<u8>,
    pub kid: [u8; 8],
    pub payload: Vec<u8>,
    pub signature: [u8; 64],
}

fn protected_header(kid: &[u8; 8]) -> Vec<u8> {
    let mut h = IntMap::new();
    h.put(1, Value::Int(ALG_EDDSA)).put(4, Value::bytes(kid.to_vec()));
    cbor::encode(&h.into_value())
}

fn to_be_signed(protected: &[u8], payload: &[u8]) -> Vec<u8> {
    cbor::encode(&Value::Array(vec![
        Value::text("Signature1"),
        Value::bytes(protected.to_vec()),
        Value::bytes(CONTEXT.to_vec()),
        Value::bytes(payload.to_vec()),
    ]))
}

/// Sign `payload` (a deterministic CBOR map) with `key`; returns the
/// tagged COSE_Sign1 bytes.
pub fn sign(payload: &[u8], key: &SigningKey) -> Vec<u8> {
    let kid = kid(key.verifying_key().as_bytes());
    let protected = protected_header(&kid);
    let signature = key.sign(&to_be_signed(&protected, payload));
    cbor::encode(&Value::Tag(
        TAG,
        Box::new(Value::Array(vec![Value::bytes(protected), Value::Map(vec![]), Value::bytes(payload.to_vec()), Value::bytes(signature.to_bytes().to_vec())])),
    ))
}

impl Sign1 {
    /// Parse the envelope: tag 18, four elements, a protected header naming
    /// EdDSA and an 8-byte kid, an empty unprotected map, a 64-byte
    /// signature. Anything else is `bad_cbor` (`bad_sig` for another
    /// algorithm or a `crit` header).
    pub fn parse(bytes: &[u8]) -> Result<Self, Reject> {
        let v = cbor::decode(bytes).map_err(|_| Reject::BadCbor)?;
        let Value::Tag(TAG, inner) = v else { return Err(Reject::BadCbor) };
        let [protected, unprotected, payload, signature] = inner.as_array().ok_or(Reject::BadCbor)? else {
            return Err(Reject::BadCbor);
        };
        let protected = protected.as_bytes().ok_or(Reject::BadCbor)?.to_vec();
        // Nothing unsigned travels with a signed message (RFC 9052 §3.1 also
        // requires `crit` to be protected).
        if !unprotected.as_map().ok_or(Reject::BadCbor)?.is_empty() {
            return Err(Reject::BadCbor);
        }
        let header = IntMap::from_value(&cbor::decode_untagged(&protected).map_err(|_| Reject::BadCbor)?).ok_or(Reject::BadCbor)?;
        if header.get(1) != Some(&Value::Int(ALG_EDDSA)) {
            return Err(Reject::BadSig);
        }
        // `crit`: we understand no extension that would need it.
        if header.get(2).is_some() {
            return Err(Reject::BadSig);
        }
        let kid: [u8; 8] = header.get(4).and_then(Value::as_bytes).and_then(|b| b.try_into().ok()).ok_or(Reject::BadCbor)?;
        let payload = payload.as_bytes().ok_or(Reject::BadCbor)?.to_vec();
        let signature: [u8; 64] = signature.as_bytes().and_then(|b| b.try_into().ok()).ok_or(Reject::BadCbor)?;
        Ok(Self { protected, kid, payload, signature })
    }

    /// Check the signature with `key`, whose kid must be the envelope's.
    pub fn verify(&self, key: &VerifyingKey) -> Result<(), Reject> {
        if kid(key.as_bytes()) != self.kid {
            return Err(Reject::UnknownKid);
        }
        key.verify(&to_be_signed(&self.protected, &self.payload), &Signature::from_bytes(&self.signature)).map_err(|_| Reject::BadSig)
    }

    /// The payload as an integer-keyed map: deterministic CBOR with no tags
    /// and unsigned integer keys, else `bad_cbor`.
    pub fn payload_map(&self) -> Result<IntMap, Reject> {
        IntMap::from_value(&cbor::decode_untagged(&self.payload).map_err(|_| Reject::BadCbor)?).ok_or(Reject::BadCbor)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_parse_verify() {
        let key = SigningKey::from_bytes(&[9; 32]);
        let payload = cbor::encode(&Value::Map(vec![(Value::uint(0u8), Value::uint(1u8))]));
        let bytes = sign(&payload, &key);
        assert_eq!(bytes[0], 0xd2, "tagged");
        let s = Sign1::parse(&bytes).unwrap();
        assert_eq!(s.payload, payload);
        s.verify(&key.verifying_key()).unwrap();
        let other = SigningKey::from_bytes(&[8; 32]);
        assert_eq!(s.verify(&other.verifying_key()), Err(Reject::UnknownKid));
        let mut tampered = bytes.clone();
        let n = tampered.len();
        tampered[n - 67] ^= 1; // the payload's last byte, just before the signature
        let t = Sign1::parse(&tampered).unwrap();
        assert_eq!(t.verify(&key.verifying_key()), Err(Reject::BadSig));
        assert_eq!(Sign1::parse(&bytes[1..]), Err(Reject::BadCbor), "untagged");
    }

    /// Astra 01 F13: `{2: [99], 99: true}` added to the unprotected bucket
    /// without re-signing was accepted, and changed the message's digest.
    #[test]
    fn nothing_rides_unprotected() {
        let key = SigningKey::from_bytes(&[9; 32]);
        let payload = cbor::encode(&Value::Map(vec![(Value::uint(0u8), Value::uint(1u8))]));
        let Value::Tag(_, inner) = cbor::decode(&sign(&payload, &key)).unwrap() else { panic!() };
        let mut parts = inner.as_array().unwrap().to_vec();
        parts[1] = Value::Map(vec![(Value::uint(2u8), Value::Array(vec![Value::uint(99u8)])), (Value::uint(99u8), Value::Bool(true))]);
        let forged = cbor::encode(&Value::Tag(TAG, Box::new(Value::Array(parts.clone()))));
        assert_eq!(Sign1::parse(&forged), Err(Reject::BadCbor));
        // A crit in the protected header is refused even when signed.
        let mut h = IntMap::new();
        h.put(1, Value::Int(ALG_EDDSA)).put(2, Value::Array(vec![Value::uint(99u8)])).put(4, Value::bytes(kid(key.verifying_key().as_bytes()).to_vec()));
        parts[0] = Value::bytes(cbor::encode(&h.into_value()));
        parts[1] = Value::Map(vec![]);
        assert_eq!(Sign1::parse(&cbor::encode(&Value::Tag(TAG, Box::new(Value::Array(parts))))), Err(Reject::BadSig));
    }

    /// Astra 01 F13: a signed command with `{99: tag(1, 0)}` was accepted.
    #[test]
    fn payload_tags_are_refused() {
        let key = SigningKey::from_bytes(&[9; 32]);
        let payload = cbor::encode(&Value::Map(vec![(Value::uint(0u8), Value::uint(1u8)), (Value::uint(99u8), Value::Tag(1, Box::new(Value::uint(0u8))))]));
        let s = Sign1::parse(&sign(&payload, &key)).unwrap();
        s.verify(&key.verifying_key()).unwrap();
        assert_eq!(s.payload_map(), Err(Reject::BadCbor));
    }
}
