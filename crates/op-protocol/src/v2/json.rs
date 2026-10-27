//! The JSON view of v2 messages: what logs, tools, schemas and test vectors
//! show. Byte strings are lowercase hex; integers stay integers.

use serde_json::{Value as Json, json};

use super::cbor::{IntMap, Value};

pub fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn unhex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect()
}

/// Any CBOR value: byte strings as hex, maps as objects keyed by their key's
/// JSON text.
pub fn value_json(v: &Value) -> Json {
    match v {
        Value::Int(i) => i64::try_from(*i).map(Json::from).or_else(|_| u64::try_from(*i).map(Json::from)).unwrap_or_else(|_| json!(i.to_string())),
        Value::Bytes(b) => json!(hex(b)),
        Value::Text(t) => json!(t),
        Value::Array(a) => Json::Array(a.iter().map(value_json).collect()),
        Value::Map(m) => Json::Object(
            m.iter()
                .map(|(k, v)| {
                    let key = match k {
                        Value::Text(t) => t.clone(),
                        other => value_json(other).to_string(),
                    };
                    (key, value_json(v))
                })
                .collect(),
        ),
        Value::Tag(t, inner) => json!({"tag": t, "value": value_json(inner)}),
        Value::Bool(b) => json!(b),
        Value::Null => Json::Null,
    }
}

pub fn int_map_json(m: &IntMap) -> Json {
    let mut entries = m.0.clone();
    entries.sort_by_key(|(k, _)| *k);
    Json::Object(entries.iter().map(|(k, v)| (k.to_string(), value_json(v))).collect())
}

/// A named field of an integer-keyed map, for maps whose fields are a table
/// rather than a struct (health, device, anchor).
pub struct Field {
    pub key: u64,
    pub name: &'static str,
    /// For a nested map, its own table.
    pub nested: Option<&'static [Field]>,
}

/// A map through its table: known keys by name, unknown ones by number.
pub fn table_json(m: &IntMap, table: &[Field]) -> Json {
    let mut entries = m.0.clone();
    entries.sort_by_key(|(k, _)| *k);
    let mut o = serde_json::Map::new();
    for (k, v) in &entries {
        match table.iter().find(|f| f.key == *k) {
            Some(f) => {
                let j = match (f.nested, IntMap::from_value(v)) {
                    (Some(t), Some(inner)) => table_json(&inner, t),
                    _ => value_json(v),
                };
                o.insert(f.name.into(), j);
            }
            None => {
                o.insert(k.to_string(), value_json(v));
            }
        }
    }
    Json::Object(o)
}
