//! Deterministic CBOR (RFC 8949 §4.2.1), the subset protocol v2 uses:
//! integers, byte and text strings, arrays, maps, tags, `false`, `true` and
//! `null`. No floats and no indefinite lengths.
//!
//! [`encode`] always writes the deterministic form: shortest heads, map keys
//! sorted by their encoded bytes. [`decode`] accepts only that form, so a
//! message has exactly one encoding and decoding then encoding gives back the
//! bytes received.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    /// −2⁶⁴ to 2⁶⁴ − 1.
    Int(i128),
    Bytes(Vec<u8>),
    Text(String),
    Array(Vec<Value>),
    /// Entries in any order; encoding sorts them.
    Map(Vec<(Value, Value)>),
    Tag(u64, Box<Value>),
    Bool(bool),
    Null,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CborError {
    /// The input ended inside an item.
    Truncated,
    /// Bytes after the item.
    Trailing,
    /// Not the deterministic form: a longer head than needed, map keys out of
    /// order or repeated.
    NotDeterministic,
    /// Indefinite length, a float, a reserved value, or invalid UTF-8.
    Unsupported,
    /// Nested deeper than [`MAX_DEPTH`].
    TooDeep,
}

impl fmt::Display for CborError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Truncated => "CBOR ends early",
            Self::Trailing => "bytes after the CBOR item",
            Self::NotDeterministic => "CBOR is not in deterministic form",
            Self::Unsupported => "CBOR item not allowed here",
            Self::TooDeep => "CBOR nested too deep",
        })
    }
}

impl std::error::Error for CborError {}

/// Deepest nesting of arrays, maps and tags.
pub const MAX_DEPTH: usize = 16;

impl Value {
    pub fn uint(v: impl Into<u64>) -> Self {
        Self::Int(i128::from(v.into()))
    }

    pub fn int(v: impl Into<i64>) -> Self {
        Self::Int(i128::from(v.into()))
    }

    pub fn bytes(v: impl Into<Vec<u8>>) -> Self {
        Self::Bytes(v.into())
    }

    pub fn text(v: impl Into<String>) -> Self {
        Self::Text(v.into())
    }

    pub fn as_u64(&self) -> Option<u64> {
        match self {
            Self::Int(v) => u64::try_from(*v).ok(),
            _ => None,
        }
    }

    pub fn as_u32(&self) -> Option<u32> {
        self.as_u64().and_then(|v| u32::try_from(v).ok())
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int(v) => i64::try_from(*v).ok(),
            _ => None,
        }
    }

    pub fn as_i32(&self) -> Option<i32> {
        self.as_i64().and_then(|v| i32::try_from(v).ok())
    }

    pub fn as_bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes(b) => Some(b),
            _ => None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&[Value]> {
        match self {
            Self::Array(a) => Some(a),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&[(Value, Value)]> {
        match self {
            Self::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(b) => Some(*b),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Self::Null)
    }
}

/// The deterministic encoding of `v`.
pub fn encode(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    write(v, &mut out);
    out
}

fn head(major: u8, n: u64, out: &mut Vec<u8>) {
    let m = major << 5;
    match n {
        0..=23 => out.push(m | n as u8),
        24..=0xff => out.extend([m | 24, n as u8]),
        0x100..=0xffff => {
            out.push(m | 25);
            out.extend((n as u16).to_be_bytes());
        }
        0x1_0000..=0xffff_ffff => {
            out.push(m | 26);
            out.extend((n as u32).to_be_bytes());
        }
        _ => {
            out.push(m | 27);
            out.extend(n.to_be_bytes());
        }
    }
}

fn write(v: &Value, out: &mut Vec<u8>) {
    match v {
        Value::Int(i) if *i >= 0 => head(0, u64::try_from(*i).expect("CBOR integer above 2^64 - 1"), out),
        Value::Int(i) => head(1, u64::try_from(-1 - *i).expect("CBOR integer below -2^64"), out),
        Value::Bytes(b) => {
            head(2, b.len() as u64, out);
            out.extend(b);
        }
        Value::Text(t) => {
            head(3, t.len() as u64, out);
            out.extend(t.as_bytes());
        }
        Value::Array(a) => {
            head(4, a.len() as u64, out);
            for item in a {
                write(item, out);
            }
        }
        Value::Map(m) => {
            let mut entries: Vec<(Vec<u8>, &Value)> = m.iter().map(|(k, v)| (encode(k), v)).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            debug_assert!(entries.windows(2).all(|w| w[0].0 != w[1].0), "duplicate CBOR map key");
            head(5, entries.len() as u64, out);
            for (k, v) in entries {
                out.extend(k);
                write(v, out);
            }
        }
        Value::Tag(t, inner) => {
            head(6, *t, out);
            write(inner, out);
        }
        Value::Bool(false) => out.push(0xf4),
        Value::Bool(true) => out.push(0xf5),
        Value::Null => out.push(0xf6),
    }
}

/// Decode exactly one item in deterministic form, with nothing after it.
pub fn decode(bytes: &[u8]) -> Result<Value, CborError> {
    let mut r = Reader { bytes, pos: 0 };
    let v = r.item(0)?;
    if r.pos != bytes.len() {
        return Err(CborError::Trailing);
    }
    Ok(v)
}

/// Decode one item at the start of `bytes`; returns it and the bytes it took.
pub fn decode_prefix(bytes: &[u8]) -> Result<(Value, usize), CborError> {
    let mut r = Reader { bytes, pos: 0 };
    let v = r.item(0)?;
    Ok((v, r.pos))
}

struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], CborError> {
        let end = self.pos.checked_add(n).filter(|&e| e <= self.bytes.len()).ok_or(CborError::Truncated)?;
        let s = &self.bytes[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    /// Major type, additional info and argument, checking the head is shortest.
    fn head(&mut self) -> Result<(u8, u8, u64), CborError> {
        let b = self.take(1)?[0];
        let (major, info) = (b >> 5, b & 0x1f);
        if major == 7 && info >= 24 {
            // Floats and one-byte simple values.
            return Err(CborError::Unsupported);
        }
        let n = match info {
            0..=23 => u64::from(info),
            24 => {
                let n = u64::from(self.take(1)?[0]);
                if n < 24 {
                    return Err(CborError::NotDeterministic);
                }
                n
            }
            25 => {
                let n = u64::from(u16::from_be_bytes(self.take(2)?.try_into().unwrap()));
                if n <= 0xff {
                    return Err(CborError::NotDeterministic);
                }
                n
            }
            26 => {
                let n = u64::from(u32::from_be_bytes(self.take(4)?.try_into().unwrap()));
                if n <= 0xffff {
                    return Err(CborError::NotDeterministic);
                }
                n
            }
            27 => {
                let n = u64::from_be_bytes(self.take(8)?.try_into().unwrap());
                if n <= 0xffff_ffff {
                    return Err(CborError::NotDeterministic);
                }
                n
            }
            _ => return Err(CborError::Unsupported),
        };
        Ok((major, info, n))
    }

    fn len(&self, n: u64) -> Result<usize, CborError> {
        let n = usize::try_from(n).map_err(|_| CborError::Truncated)?;
        if n > self.bytes.len() - self.pos {
            return Err(CborError::Truncated);
        }
        Ok(n)
    }

    fn item(&mut self, depth: usize) -> Result<Value, CborError> {
        if depth > MAX_DEPTH {
            return Err(CborError::TooDeep);
        }
        let (major, info, n) = self.head()?;
        Ok(match major {
            0 => Value::Int(i128::from(n)),
            1 => Value::Int(-1 - i128::from(n)),
            2 => {
                let len = self.len(n)?;
                Value::Bytes(self.take(len)?.to_vec())
            }
            3 => {
                let len = self.len(n)?;
                let s = std::str::from_utf8(self.take(len)?).map_err(|_| CborError::Unsupported)?;
                Value::Text(s.to_owned())
            }
            4 => {
                // Every item takes at least one byte.
                let len = self.len(n)?;
                let mut items = Vec::with_capacity(len);
                for _ in 0..len {
                    items.push(self.item(depth + 1)?);
                }
                Value::Array(items)
            }
            5 => {
                let len = self.len(n)?;
                let mut entries = Vec::with_capacity(len);
                let mut last: Option<&[u8]> = None;
                for _ in 0..len {
                    let start = self.pos;
                    let k = self.item(depth + 1)?;
                    let key_bytes = &self.bytes[start..self.pos];
                    if last.is_some_and(|l| l >= key_bytes) {
                        return Err(CborError::NotDeterministic);
                    }
                    last = Some(key_bytes);
                    let v = self.item(depth + 1)?;
                    entries.push((k, v));
                }
                Value::Map(entries)
            }
            6 => Value::Tag(n, Box::new(self.item(depth + 1)?)),
            _ => match info {
                20 => Value::Bool(false),
                21 => Value::Bool(true),
                22 => Value::Null,
                _ => return Err(CborError::Unsupported),
            },
        })
    }
}

/// A map with unsigned integer keys, the shape of every v2 message body.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntMap(pub Vec<(u64, Value)>);

impl IntMap {
    pub fn new() -> Self {
        Self::default()
    }

    /// From a decoded map. Entries whose key isn't an unsigned integer are
    /// dropped: they are some later version's, and ignored like any unknown key.
    pub fn from_value(v: &Value) -> Option<Self> {
        Some(Self(v.as_map()?.iter().filter_map(|(k, v)| Some((k.as_u64()?, v.clone()))).collect()))
    }

    pub fn get(&self, key: u64) -> Option<&Value> {
        self.0.iter().find(|(k, _)| *k == key).map(|(_, v)| v)
    }

    pub fn put(&mut self, key: u64, v: Value) -> &mut Self {
        self.0.retain(|(k, _)| *k != key);
        self.0.push((key, v));
        self
    }

    pub fn put_opt(&mut self, key: u64, v: Option<Value>) -> &mut Self {
        if let Some(v) = v {
            self.put(key, v);
        }
        self
    }

    pub fn into_value(self) -> Value {
        Value::Map(self.0.into_iter().map(|(k, v)| (Value::uint(k), v)).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(b: &[u8]) -> String {
        b.iter().map(|x| format!("{x:02x}")).collect()
    }

    #[test]
    fn rfc8949_appendix_a_examples() {
        let cases: [(Value, &str); 14] = [
            (Value::uint(0u8), "00"),
            (Value::uint(23u8), "17"),
            (Value::uint(24u8), "1818"),
            (Value::uint(1000u16), "1903e8"),
            (Value::uint(1_000_000u32), "1a000f4240"),
            (Value::uint(1_000_000_000_000u64), "1b000000e8d4a51000"),
            (Value::Int(-1), "20"),
            (Value::Int(-1000), "3903e7"),
            (Value::bytes(vec![1, 2, 3, 4]), "4401020304"),
            (Value::text("IETF"), "6449455446"),
            (Value::Array(vec![Value::uint(1u8), Value::Array(vec![Value::uint(2u8), Value::uint(3u8)])]), "8201820203"),
            (Value::Map(vec![(Value::uint(2u8), Value::uint(4u8)), (Value::uint(1u8), Value::uint(2u8))]), "a201020204"),
            (Value::Null, "f6"),
            (Value::Tag(18, Box::new(Value::Array(vec![]))), "d280"),
        ];
        for (v, want) in cases {
            let got = encode(&v);
            assert_eq!(hex(&got), want, "{v:?}");
            let back = decode(&got).unwrap();
            assert_eq!(encode(&back), got);
        }
    }

    #[test]
    fn decode_refuses_what_is_not_deterministic() {
        let bad = |h: &str| decode(&(0..h.len()).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).unwrap()).collect::<Vec<_>>());
        assert_eq!(bad("1817"), Err(CborError::NotDeterministic), "23 in two bytes");
        assert_eq!(bad("190017"), Err(CborError::NotDeterministic));
        assert_eq!(bad("a202010101"), Err(CborError::NotDeterministic), "keys out of order");
        assert_eq!(bad("a201010101"), Err(CborError::NotDeterministic), "repeated key");
        assert_eq!(bad("9f01ff"), Err(CborError::Unsupported), "indefinite array");
        assert_eq!(bad("f93c00"), Err(CborError::Unsupported), "half float");
        assert_eq!(bad("f7"), Err(CborError::Unsupported), "undefined");
        assert_eq!(bad("62c328"), Err(CborError::Unsupported), "invalid UTF-8");
        assert_eq!(bad("0000"), Err(CborError::Trailing));
        assert_eq!(bad("4401"), Err(CborError::Truncated));
        assert_eq!(bad("9bffffffffffffffff"), Err(CborError::Truncated), "a huge length doesn't allocate");
        let deep = "81".repeat(MAX_DEPTH + 1) + "00";
        assert_eq!(bad(&deep), Err(CborError::TooDeep));
    }

    #[test]
    fn extreme_integers() {
        for v in [i128::from(u64::MAX), -1 - i128::from(u64::MAX), 0, -24, -25] {
            let b = encode(&Value::Int(v));
            assert_eq!(decode(&b).unwrap(), Value::Int(v));
        }
    }
}
