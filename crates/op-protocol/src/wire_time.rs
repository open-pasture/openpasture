//! RFC 3339 UTC with a `Z`, fractional seconds only when present
//! (`2026-09-25T12:30:00Z`). Parses any RFC 3339 offset.

use chrono::{DateTime, SecondsFormat, Timelike, Utc};
use serde::{Deserialize, Deserializer, Serializer};

pub fn format(t: &DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

pub fn parse(s: &str) -> Result<DateTime<Utc>, chrono::ParseError> {
    DateTime::parse_from_rfc3339(s).map(|t| t.with_timezone(&Utc))
}

/// Drop fractional seconds.
pub fn trunc_secs(t: DateTime<Utc>) -> DateTime<Utc> {
    t.with_nanosecond(0).unwrap_or(t)
}

pub fn serialize<S: Serializer>(t: &DateTime<Utc>, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(&format(t))
}

pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<DateTime<Utc>, D::Error> {
    let s = String::deserialize(d)?;
    parse(&s).map_err(serde::de::Error::custom)
}

pub mod option {
    use super::*;

    pub fn serialize<S: Serializer>(t: &Option<DateTime<Utc>>, s: S) -> Result<S::Ok, S::Error> {
        match t {
            Some(t) => s.serialize_str(&format(t)),
            None => s.serialize_none(),
        }
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Option<DateTime<Utc>>, D::Error> {
        match Option::<String>::deserialize(d)? {
            Some(s) => parse(&s).map(Some).map_err(serde::de::Error::custom),
            None => Ok(None),
        }
    }
}
