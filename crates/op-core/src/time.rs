//! Timestamps. Stored as RFC 3339 UTC text with milliseconds so they sort as
//! text; telemetry also stores unix milliseconds in `t`.

use chrono::{DateTime, SubsecRound, Utc};

/// Now, truncated to milliseconds so it round-trips through the database.
pub fn now() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(3)
}

/// Database text form: `2026-09-26T12:00:00.000Z`.
pub fn to_db(t: &DateTime<Utc>) -> String {
    t.format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

pub fn from_db(s: &str) -> anyhow::Result<DateTime<Utc>> {
    Ok(DateTime::parse_from_rfc3339(s)?.with_timezone(&Utc))
}

pub fn opt_from_db(s: Option<String>) -> anyhow::Result<Option<DateTime<Utc>>> {
    s.as_deref().map(from_db).transpose()
}

pub fn unix_ms(t: &DateTime<Utc>) -> i64 {
    t.timestamp_millis()
}

pub fn from_unix_ms(ms: i64) -> DateTime<Utc> {
    DateTime::from_timestamp_millis(ms).unwrap_or_default()
}
