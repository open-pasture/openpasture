//! Commands as a collar receives them: raw bytes, scanned the way the
//! firmware scans them (protocol v1, §3.2).
//!
//! The collar doesn't parse JSON into a tree. It records up to 16 top-level
//! key/value spans, sorts them by key bytes, copies each value token verbatim
//! with whitespace outside strings removed, and verifies the Ed25519
//! signature over the result (without `sig`). [`canonical_wire`] is a port of
//! that scanner; [`verify_wire`] and [`verify_config_wire`] add the signature
//! and parse the command. Unlike [`crate::verify_json`] they see duplicate
//! keys, nesting and size.
//!
//! Checked in this order:
//! 1. `too_large`: more than [`MAX_COMMAND_BYTES`](crate::MAX_COMMAND_BYTES) bytes.
//! 2. `bad_json`: not UTF-8; not one JSON object; a nested object anywhere;
//!    arrays nested deeper than [`MAX_ARRAY_DEPTH`]; a key with a backslash
//!    escape; a duplicate key; more than 16 keys; anything after the object.
//! 3. `bad_sig`: no `sig`, `sig` not a plain base64 string of 64 bytes, or a
//!    signature that doesn't verify.
//! 4. `bad_json`: the fields don't make a command (types, required fields,
//!    ids empty or over 64 bytes). Unknown keys are signed and ignored.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use ed25519_dalek::{Signature, Verifier};

use crate::{BoundaryCommand, ConfigCommand, MAX_COMMAND_BYTES, MAX_TOP_LEVEL_KEYS, RejectCode, VerifyingKey};

/// Deepest array nesting a command may use. Holes need 3 (`[[[lon, lat]]]`).
pub const MAX_ARRAY_DEPTH: usize = 4;

/// A scanned command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Canonical {
    /// Canonical JSON without `sig`: what the signature covers.
    pub bytes: Vec<u8>,
    /// The `sig` value token as sent (with its quotes), if there was one.
    pub sig: Option<Vec<u8>>,
}

/// Scan a command's wire bytes as the collar does.
pub fn canonical_wire(wire: &[u8]) -> Result<Canonical, RejectCode> {
    if wire.len() > MAX_COMMAND_BYTES {
        return Err(RejectCode::TooLarge);
    }
    if std::str::from_utf8(wire).is_err() {
        return Err(RejectCode::BadJson);
    }
    let mut s = Scanner { b: wire, i: 0 };
    let spans = s.object()?;
    let mut sig = None;
    let mut kept: Vec<&(Vec<u8>, Vec<u8>)> = Vec::with_capacity(spans.len());
    for span in &spans {
        if span.0 == b"sig" {
            sig = Some(span.1.clone());
        } else {
            kept.push(span);
        }
    }
    kept.sort_by(|a, b| a.0.cmp(&b.0));
    let mut bytes = Vec::with_capacity(wire.len());
    bytes.push(b'{');
    for (k, (key, value)) in kept.into_iter().enumerate() {
        if k > 0 {
            bytes.push(b',');
        }
        bytes.push(b'"');
        bytes.extend_from_slice(key);
        bytes.extend_from_slice(b"\":");
        bytes.extend_from_slice(value);
    }
    bytes.push(b'}');
    Ok(Canonical { bytes, sig })
}

/// Scan, verify the signature, and parse a boundary command.
pub fn verify_wire(wire: &[u8], key: &VerifyingKey) -> Result<BoundaryCommand, RejectCode> {
    verify_signed(wire, key)?;
    let cmd: BoundaryCommand = serde_json::from_slice(wire).map_err(|_| RejectCode::BadJson)?;
    cmd.check_ids()?;
    Ok(cmd)
}

/// Scan, verify the signature, and parse a config command. Its collar,
/// version and values are [`ConfigCommand::check`]'s.
pub fn verify_config_wire(wire: &[u8], key: &VerifyingKey) -> Result<ConfigCommand, RejectCode> {
    verify_signed(wire, key)?;
    let cmd: ConfigCommand = serde_json::from_slice(wire).map_err(|_| RejectCode::BadJson)?;
    cmd.check_ids()?;
    Ok(cmd)
}

fn verify_signed(wire: &[u8], key: &VerifyingKey) -> Result<(), RejectCode> {
    let c = canonical_wire(wire)?;
    let token = c.sig.ok_or(RejectCode::BadSig)?;
    let b64 = token.strip_prefix(b"\"").and_then(|t| t.strip_suffix(b"\"")).filter(|t| !t.contains(&b'\\')).ok_or(RejectCode::BadSig)?;
    let bytes: [u8; 64] = STANDARD.decode(b64).ok().and_then(|b| b.try_into().ok()).ok_or(RejectCode::BadSig)?;
    key.verify(&c.bytes, &Signature::from_bytes(&bytes)).map_err(|_| RejectCode::BadSig)
}

struct Scanner<'a> {
    b: &'a [u8],
    i: usize,
}

type Spans = Vec<(Vec<u8>, Vec<u8>)>;

impl Scanner<'_> {
    fn ws(&mut self) {
        while self.i < self.b.len() && matches!(self.b[self.i], b' ' | b'\t' | b'\n' | b'\r') {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> Result<(), RejectCode> {
        if self.peek() == Some(c) {
            self.i += 1;
            Ok(())
        } else {
            Err(RejectCode::BadJson)
        }
    }

    /// The whole input: one object of flat key/value spans.
    fn object(&mut self) -> Result<Spans, RejectCode> {
        let mut spans: Spans = Vec::new();
        self.ws();
        self.eat(b'{')?;
        self.ws();
        if self.peek() == Some(b'}') {
            self.i += 1;
        } else {
            loop {
                self.ws();
                let mut token = Vec::new();
                self.string(&mut token)?;
                let key = token[1..token.len() - 1].to_vec();
                if key.contains(&b'\\') {
                    return Err(RejectCode::BadJson);
                }
                self.ws();
                self.eat(b':')?;
                self.ws();
                let mut value = Vec::new();
                self.value(0, &mut value)?;
                if spans.len() == MAX_TOP_LEVEL_KEYS || spans.iter().any(|(k, _)| *k == key) {
                    return Err(RejectCode::BadJson);
                }
                spans.push((key, value));
                self.ws();
                match self.peek() {
                    Some(b',') => self.i += 1,
                    Some(b'}') => {
                        self.i += 1;
                        break;
                    }
                    _ => return Err(RejectCode::BadJson),
                }
            }
        }
        self.ws();
        if self.i != self.b.len() {
            return Err(RejectCode::BadJson);
        }
        Ok(spans)
    }

    /// One value token, whitespace outside strings dropped.
    fn value(&mut self, depth: usize, out: &mut Vec<u8>) -> Result<(), RejectCode> {
        match self.peek() {
            Some(b'"') => self.string(out),
            Some(b'[') => {
                if depth == MAX_ARRAY_DEPTH {
                    return Err(RejectCode::BadJson);
                }
                self.i += 1;
                out.push(b'[');
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    out.push(b']');
                    return Ok(());
                }
                loop {
                    self.ws();
                    self.value(depth + 1, out)?;
                    self.ws();
                    match self.peek() {
                        Some(b',') => {
                            self.i += 1;
                            out.push(b',');
                        }
                        Some(b']') => {
                            self.i += 1;
                            out.push(b']');
                            return Ok(());
                        }
                        _ => return Err(RejectCode::BadJson),
                    }
                }
            }
            Some(b'-' | b'0'..=b'9') => self.number(out),
            Some(b't') => self.literal(b"true", out),
            Some(b'f') => self.literal(b"false", out),
            Some(b'n') => self.literal(b"null", out),
            // `{` (a nested object) and anything else.
            _ => Err(RejectCode::BadJson),
        }
    }

    fn literal(&mut self, lit: &[u8], out: &mut Vec<u8>) -> Result<(), RejectCode> {
        if self.b[self.i..].starts_with(lit) {
            self.i += lit.len();
            out.extend_from_slice(lit);
            Ok(())
        } else {
            Err(RejectCode::BadJson)
        }
    }

    /// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`, copied as sent.
    fn number(&mut self, out: &mut Vec<u8>) -> Result<(), RejectCode> {
        let start = self.i;
        let digits = |s: &mut Self| {
            let from = s.i;
            while s.peek().is_some_and(|c| c.is_ascii_digit()) {
                s.i += 1;
            }
            s.i - from
        };
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        match self.peek() {
            Some(b'0') => self.i += 1,
            Some(b'1'..=b'9') => {
                digits(self);
            }
            _ => return Err(RejectCode::BadJson),
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if digits(self) == 0 {
                return Err(RejectCode::BadJson);
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if digits(self) == 0 {
                return Err(RejectCode::BadJson);
            }
        }
        out.extend_from_slice(&self.b[start..self.i]);
        Ok(())
    }

    /// A string token, quotes and escapes included, copied as sent.
    fn string(&mut self, out: &mut Vec<u8>) -> Result<(), RejectCode> {
        let start = self.i;
        self.eat(b'"')?;
        loop {
            match self.peek() {
                None => return Err(RejectCode::BadJson),
                Some(b'"') => {
                    self.i += 1;
                    break;
                }
                Some(b'\\') => {
                    self.i += 1;
                    match self.peek() {
                        Some(b'"' | b'\\' | b'/' | b'b' | b'f' | b'n' | b'r' | b't') => self.i += 1,
                        Some(b'u') => {
                            let hex = self.b.get(self.i + 1..self.i + 5).ok_or(RejectCode::BadJson)?;
                            if !hex.iter().all(u8::is_ascii_hexdigit) {
                                return Err(RejectCode::BadJson);
                            }
                            self.i += 5;
                        }
                        _ => return Err(RejectCode::BadJson),
                    }
                }
                Some(c) if c < 0x20 => return Err(RejectCode::BadJson),
                Some(_) => self.i += 1,
            }
        }
        out.extend_from_slice(&self.b[start..self.i]);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{SigningKey, canonical_json, sign_command, signing_payload, verify_json};
    use op_geo::Polygon;

    fn key() -> SigningKey {
        SigningKey::from_bytes(&core::array::from_fn(|i| i as u8))
    }

    fn signed() -> BoundaryCommand {
        let poly = Polygon::from_ring(vec![[-92.41, 38.12], [-92.40, 38.12], [-92.40, 38.13], [-92.41, 38.13]]);
        let mut cmd = BoundaryCommand::from_polygon("bnd_1", 5, &poly, None).unwrap();
        cmd.herd_id = Some("herd_1".into());
        cmd.warn_m = Some(5.0);
        sign_command(&mut cmd, &key());
        cmd
    }

    /// Sign a raw JSON object text as the server would (canonical without sig).
    fn sign_text(text: &str) -> String {
        let v: serde_json::Value = serde_json::from_str(text).unwrap();
        let sig = crate::sign::sign_payload(canonical_json(&v).as_bytes(), &key());
        format!("{},\"sig\":\"{sig}\"}}", text.trim_end().strip_suffix('}').unwrap())
    }

    #[test]
    fn canonical_matches_the_signing_payload() {
        let cmd = signed();
        let compact = serde_json::to_string(&cmd).unwrap();
        let pretty = serde_json::to_string_pretty(&cmd).unwrap();
        for text in [&compact, &pretty] {
            let c = canonical_wire(text.as_bytes()).unwrap();
            assert_eq!(c.bytes, signing_payload(&cmd));
            assert_eq!(verify_wire(text.as_bytes(), &key().verifying_key()).unwrap(), cmd);
        }
    }

    #[test]
    fn scanner_rules() {
        let k = key().verifying_key();
        assert_eq!(canonical_wire(b"[1,2]"), Err(RejectCode::BadJson));
        assert_eq!(canonical_wire(b"{\"a\":1} x"), Err(RejectCode::BadJson));
        assert_eq!(canonical_wire(b"{\"a\":01}"), Err(RejectCode::BadJson));
        assert_eq!(canonical_wire(b"{\"a\":1.}"), Err(RejectCode::BadJson));
        assert_eq!(canonical_wire(b"{\"a\":\"x\ny\"}"), Err(RejectCode::BadJson), "raw control character");
        assert_eq!(canonical_wire(b"{\"a\":\"\\q\"}"), Err(RejectCode::BadJson));
        assert_eq!(canonical_wire(b"{\"\\u0061\":1}"), Err(RejectCode::BadJson), "escaped key");
        assert_eq!(canonical_wire(b"{\"a\":[[[[[1]]]]]}"), Err(RejectCode::BadJson), "too deep");
        assert!(canonical_wire(b"{\"a\":[[[[1]]]]}").is_ok());
        assert_eq!(canonical_wire(b"{\"a\":1,\"a\":1}"), Err(RejectCode::BadJson));
        assert_eq!(canonical_wire(b"{\"a\":{\"b\":1}}"), Err(RejectCode::BadJson));
        assert_eq!(canonical_wire(b"{\"a\":[{\"b\":1}]}"), Err(RejectCode::BadJson));
        assert_eq!(canonical_wire(&[b'{', 0xff, b'}']), Err(RejectCode::BadJson));
        let c = canonical_wire(b" { \"b\" : [ 1 , \"x y\" ] ,\n \"a\" : 1e-7 , \"sig\":\"s\" } ").unwrap();
        assert_eq!(c.bytes, br#"{"a":1e-7,"b":[1,"x y"]}"#);
        assert_eq!(c.sig.as_deref(), Some(&b"\"s\""[..]));
        assert_eq!(verify_wire(br#"{"a":1}"#, &k), Err(RejectCode::BadSig));
        assert_eq!(verify_wire(br#"{"a":1,"sig":1}"#, &k), Err(RejectCode::BadSig));
    }

    #[test]
    fn catches_what_verify_json_misses() {
        let k = key().verifying_key();
        let cmd = signed();
        let wire = serde_json::to_string(&cmd).unwrap();
        // A duplicate key ahead of the signed one: serde_json keeps the last.
        let dup = wire.replacen('{', "{\"version\":99,", 1);
        verify_json(&serde_json::from_str(&dup).unwrap(), &k).unwrap();
        assert_eq!(verify_wire(dup.as_bytes(), &k), Err(RejectCode::BadJson));
        // A signed nested object.
        let body = &wire[..wire.find(",\"sig\"").unwrap()];
        let nested = sign_text(&format!("{body},\"x\":{{\"a\":1}}}}"));
        let v: serde_json::Value = serde_json::from_str(&nested).unwrap();
        verify_json(&v, &k).unwrap();
        assert_eq!(verify_wire(nested.as_bytes(), &k), Err(RejectCode::BadJson));
        // Oversize but signed.
        let pad = sign_text(&format!("{body},\"pad\":\"{}\"}}", "x".repeat(MAX_COMMAND_BYTES)));
        verify_json(&serde_json::from_str(&pad).unwrap(), &k).unwrap();
        assert_eq!(verify_wire(pad.as_bytes(), &k), Err(RejectCode::TooLarge));
    }

    #[test]
    fn keys_and_ids() {
        let k = key().verifying_key();
        // Unknown keys are signed and ignored.
        let wire = serde_json::to_string(&signed()).unwrap();
        let body = &wire[..wire.find(",\"sig\"").unwrap()];
        let extra = sign_text(&format!("{body},\"future\":[1,2]}}"));
        assert_eq!(verify_wire(extra.as_bytes(), &k).unwrap().version, 5);
        // An over-long id parses but is refused.
        let mut long = signed();
        long.command_id = "b".repeat(65);
        sign_command(&mut long, &key());
        assert_eq!(verify_wire(serde_json::to_string(&long).unwrap().as_bytes(), &k), Err(RejectCode::BadJson));
        // Wrong types are bad_json after the signature.
        let typed = sign_text(r#"{"command_id":"b","version":"5","boundary":[]}"#);
        assert_eq!(verify_wire(typed.as_bytes(), &k), Err(RejectCode::BadJson));
    }
}
