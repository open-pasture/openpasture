//! Partial updates: JSON merge patch (RFC 7396) over a record's serde shape.

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::error::ApiError;

pub fn merge(target: &mut Value, patch: &Value) {
    if let Value::Object(p) = patch {
        if !target.is_object() {
            *target = Value::Object(Default::default());
        }
        let t = target.as_object_mut().expect("object");
        for (k, v) in p {
            if v.is_null() {
                t.remove(k);
            } else {
                merge(t.entry(k.clone()).or_insert(Value::Null), v);
            }
        }
    } else {
        *target = patch.clone();
    }
}

/// Apply `patch` to `current`. Keys in `immutable` are ignored. A null
/// removes an optional field; a null on a required field is a 400.
pub fn apply<T: Serialize + DeserializeOwned>(current: &T, patch: &Value, immutable: &[&str]) -> Result<T, ApiError> {
    let Value::Object(p) = patch else {
        return Err(ApiError::bad_request("Expected a JSON object."));
    };
    let mut p = p.clone();
    for key in immutable {
        p.remove(*key);
    }
    let mut value = serde_json::to_value(current).map_err(anyhow::Error::from)?;
    merge(&mut value, &Value::Object(p));
    serde_json::from_value(value).map_err(|e| ApiError::bad_request(e.to_string()))
}
