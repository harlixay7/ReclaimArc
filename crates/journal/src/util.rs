//! Timestamp helper (UTC ISO-8601 with millisecond precision).

/// Current UTC time as an ISO-8601 string.
pub fn now_iso() -> String {
    let now = std::time::SystemTime::now();
    let dt: chrono::DateTime<chrono::Utc> = now.into();
    dt.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}
