//! Time ranges and buckets from query strings.
//!
//! `from` / `to` are RFC 3339, a date (`2026-09-26`), `now`, or relative to
//! now (`-24h`, `-7d`, `-30m`, `-2w`). Default: the last 24 hours.

use chrono::{DateTime, Duration, NaiveDate, Utc};
use op_core::ApiError;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeRange {
    pub from: DateTime<Utc>,
    pub to: DateTime<Utc>,
}

impl TimeRange {
    pub fn new(from: DateTime<Utc>, to: DateTime<Utc>) -> Self {
        Self { from, to }
    }

    /// Parse `from` / `to` with a default span when `from` is missing.
    pub fn parse(from: Option<&str>, to: Option<&str>, default_span: Duration, now: DateTime<Utc>) -> Result<Self, ApiError> {
        let to = match to.filter(|s| !s.trim().is_empty()) {
            Some(s) => parse_time(s, now)?,
            None => now,
        };
        let from = match from.filter(|s| !s.trim().is_empty()) {
            Some(s) => parse_time(s, now)?,
            None => to - default_span,
        };
        if from >= to {
            return Err(ApiError::bad_request("`from` must be before `to`."));
        }
        Ok(Self { from, to })
    }

    pub fn from_ms(&self) -> i64 {
        self.from.timestamp_millis()
    }
    pub fn to_ms(&self) -> i64 {
        self.to.timestamp_millis()
    }
    pub fn span_ms(&self) -> i64 {
        self.to_ms() - self.from_ms()
    }
}

pub fn parse_time(s: &str, now: DateTime<Utc>) -> Result<DateTime<Utc>, ApiError> {
    let s = s.trim();
    if s.eq_ignore_ascii_case("now") {
        return Ok(now);
    }
    if let Some(rest) = s.strip_prefix('-') {
        return Ok(now - parse_duration(rest).map_err(|_| bad_time(s))?);
    }
    if let Ok(t) = DateTime::parse_from_rfc3339(s) {
        return Ok(t.with_timezone(&Utc));
    }
    if let Ok(d) = NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Ok(d.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc());
    }
    Err(bad_time(s))
}

fn bad_time(s: &str) -> ApiError {
    ApiError::bad_request(format!("Can't read the time `{s}`. Use RFC 3339 or a relative time like -24h or -7d."))
}

/// `90s`, `5m`, `1h`, `7d`, `2w`, or plain seconds.
pub fn parse_duration(s: &str) -> Result<Duration, ApiError> {
    let s = s.trim();
    let bad = || ApiError::bad_request(format!("Can't read the duration `{s}`. Use 30s, 5m, 1h, 1d or 1w."));
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().map_err(|_| bad())?;
    let secs = match unit.trim() {
        "" | "s" => 1.0,
        "m" | "min" => 60.0,
        "h" => 3600.0,
        "d" => 86_400.0,
        "w" => 604_800.0,
        _ => return Err(bad()),
    };
    let ms = (n * secs * 1000.0).round();
    if !ms.is_finite() || ms <= 0.0 || ms > 1e15 {
        return Err(bad());
    }
    Ok(Duration::milliseconds(ms as i64))
}

/// Bucket widths auto-picked from, in seconds.
const NICE_BUCKETS_S: [i64; 12] = [60, 300, 600, 900, 1800, 3600, 7200, 10_800, 21_600, 43_200, 86_400, 604_800];

/// Most buckets a request may ask for (memory is per collar per bucket).
pub const MAX_BUCKETS: i64 = 2000;

/// The bucket from the query, or the smallest nice width giving at most
/// `target` buckets. Never below one minute, never more than [`MAX_BUCKETS`].
pub fn bucket_ms(bucket: Option<&str>, range: &TimeRange, target: i64) -> Result<i64, ApiError> {
    if let Some(b) = bucket.filter(|s| !s.trim().is_empty()) {
        let ms = parse_duration(b)?.num_milliseconds().max(60_000);
        if range.span_ms() / ms > MAX_BUCKETS {
            return Err(ApiError::bad_request("That bucket is too small for the range; use a larger one."));
        }
        return Ok(ms);
    }
    let span_s = range.span_ms() / 1000;
    for b in NICE_BUCKETS_S {
        if span_s / b <= target {
            return Ok(b * 1000);
        }
    }
    // Very long ranges: whole weeks, few enough of them.
    let week = NICE_BUCKETS_S[NICE_BUCKETS_S.len() - 1];
    let n = MAX_BUCKETS.min(target.max(1));
    let weeks = ((span_s + week * n - 1) / (week * n)).max(1);
    Ok(weeks * week * 1000)
}

/// UTC date of a unix-ms time, `YYYY-MM-DD`.
pub fn date_of(ms: i64) -> NaiveDate {
    op_core::time::from_unix_ms(ms).date_naive()
}

/// Unix ms of midnight UTC starting `d`.
pub fn day_start_ms(d: NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn now() -> DateTime<Utc> {
        DateTime::parse_from_rfc3339("2026-09-26T12:00:00Z").unwrap().with_timezone(&Utc)
    }

    #[test]
    fn relative_and_absolute() {
        let r = TimeRange::parse(Some("-7d"), None, Duration::hours(24), now()).unwrap();
        assert_eq!(r.to, now());
        assert_eq!(r.from, now() - Duration::days(7));
        let r = TimeRange::parse(None, None, Duration::hours(24), now()).unwrap();
        assert_eq!(r.span_ms(), 86_400_000);
        let r = TimeRange::parse(Some("2026-09-20T00:00:00Z"), Some("2026-09-21"), Duration::hours(24), now()).unwrap();
        assert_eq!(r.span_ms(), 86_400_000);
        assert!(TimeRange::parse(Some("-1h"), Some("-2h"), Duration::hours(24), now()).is_err());
        assert!(TimeRange::parse(Some("yesterday"), None, Duration::hours(24), now()).is_err());
        assert_eq!(parse_duration("90").unwrap(), Duration::seconds(90));
        assert_eq!(parse_duration("1.5h").unwrap(), Duration::minutes(90));
    }

    #[test]
    fn auto_bucket() {
        let day = TimeRange::new(now() - Duration::hours(24), now());
        assert_eq!(bucket_ms(None, &day, 150).unwrap(), 600_000);
        let week = TimeRange::new(now() - Duration::days(7), now());
        assert_eq!(bucket_ms(None, &week, 150).unwrap(), 7_200_000);
        assert_eq!(bucket_ms(Some("1h"), &day, 150).unwrap(), 3_600_000);
        assert_eq!(bucket_ms(Some("5s"), &TimeRange::new(now() - Duration::hours(1), now()), 150).unwrap(), 60_000);
        // Bucket count is capped, asked for or not.
        let decade = TimeRange::new(now() - Duration::days(3650), now());
        assert!(bucket_ms(Some("1h"), &decade, 150).is_err());
        let b = bucket_ms(None, &decade, 150).unwrap();
        assert!(decade.span_ms() / b <= 150, "{b}");
        let century = TimeRange::new(now() - Duration::days(36_500), now());
        assert!(century.span_ms() / bucket_ms(None, &century, 150).unwrap() <= MAX_BUCKETS);
    }
}
