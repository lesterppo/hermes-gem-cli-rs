//! Small shared helpers: error classification, proxy sanitisation,
//! ISO timestamps without a date crate.

use std::time::{SystemTime, UNIX_EPOCH};

/// Mirror of the Python original's error classification.
pub fn classify_error(msg: &str) -> &'static str {
    let u = msg.to_uppercase();
    for p in [
        "UNAUTHENTICATED",
        "COOKIES HAVE EXPIRED",
        "SESSION IS NOT AUTHENTICATED",
        "ERROR CODE: 1100",
        "USER IS NOT AUTHENTICATED",
        "AUTH_EXPIRED",
        "NO SNM0E",
        "NO SN",
    ] {
        if u.contains(p) {
            return "AUTH_EXPIRED";
        }
    }
    for p in [
        "ERROR CODE: 1097",
        "RATE LIMIT",
        "TOO MANY REQUESTS",
        "QUOTA EXCEEDED",
        "RESOURCE HAS BEEN EXHAUSTED",
        "429",
    ] {
        if u.contains(p) {
            return "RATE_LIMIT";
        }
    }
    if u.contains("TIMEOUT") || u.contains("TIMED OUT") {
        return "TIMEOUT";
    }
    "GEN_FAILED"
}

/// The VM's default NO_PROXY contains bracketed IPv6 entries (`[::1]`)
/// that break HTTP clients at construction time. Drop any entry that
/// contains a bracket before any HTTP client is built.
pub fn sanitize_no_proxy() {
    for key in ["NO_PROXY", "no_proxy"] {
        if let Ok(v) = std::env::var(key) {
            let kept: Vec<&str> = v
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty() && !s.contains('[') && !s.contains(']'))
                .collect();
            std::env::set_var(key, kept.join(","));
        }
    }
}

/// Seconds since epoch -> ISO-8601 UTC string (civil-from-days algorithm).
pub fn iso_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}+00:00",
        y,
        m,
        d,
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso_now_shape() {
        let s = iso_now();
        assert!(s.len() == 25 && s.contains('T') && s.ends_with("+00:00"));
    }

    #[test]
    fn error_classes() {
        assert_eq!(classify_error("cookies have expired"), "AUTH_EXPIRED");
        assert_eq!(classify_error("error code: 1097"), "RATE_LIMIT");
        assert_eq!(classify_error("something broke"), "GEN_FAILED");
    }
}
