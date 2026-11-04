//! Small shared helpers: time-zone conversion, lenient number/date parsing.

use chrono::{DateTime, Datelike, Duration, NaiveDate, NaiveDateTime, Weekday};

/// Convert a UNIX timestamp (seconds, UTC) to New York local wall-clock time.
///
/// Implements the US DST rule in force since 2007 (second Sunday of March 02:00
/// to first Sunday of November 02:00) so we do not need a tz database.
pub fn unix_to_new_york(ts: i64) -> NaiveDateTime {
    let utc = DateTime::from_timestamp(ts, 0)
        .map(|d| d.naive_utc())
        .unwrap_or_default();
    utc + Duration::hours(new_york_offset_hours(utc))
}

/// UTC offset (hours) of America/New_York at a given UTC instant.
pub fn new_york_offset_hours(utc: NaiveDateTime) -> i64 {
    let y = utc.year();
    // DST starts 2nd Sunday of March at 02:00 EST = 07:00 UTC,
    // ends 1st Sunday of November at 02:00 EDT = 06:00 UTC.
    let start = nth_weekday(y, 3, Weekday::Sun, 2).and_hms_opt(7, 0, 0).unwrap();
    let end = nth_weekday(y, 11, Weekday::Sun, 1).and_hms_opt(6, 0, 0).unwrap();
    if utc >= start && utc < end {
        -4
    } else {
        -5
    }
}

fn nth_weekday(year: i32, month: u32, wd: Weekday, n: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    let shift = (7 + wd.num_days_from_monday() as i64 - first.weekday().num_days_from_monday() as i64) % 7;
    first + Duration::days(shift + 7 * (n as i64 - 1))
}

/// Current New York wall-clock time.
pub fn now_new_york() -> NaiveDateTime {
    unix_to_new_york(chrono::Utc::now().timestamp())
}

/// Lenient float parser: trims, strips `$`, `,`, `%`, `+`; returns `None` for
/// empty / `.` / `-` / `nan` / `null` / `N/A`.
pub fn parse_num(s: &str) -> Option<f64> {
    let t = s.trim().trim_matches('"').trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if matches!(lower.as_str(), "." | "-" | "--" | "nan" | "null" | "none" | "n/a" | "na" | "#n/a") {
        return None;
    }
    let cleaned: String = t
        .chars()
        .filter(|c| !matches!(c, '$' | ',' | '%' | '+' | ' ' | '\u{a0}'))
        .collect();
    let v: f64 = cleaned.parse().ok()?;
    if v.is_finite() {
        Some(v)
    } else {
        None
    }
}

/// Parse many common date spellings: `2024-01-31`, `2024/01/31`, `20240131`,
/// `01/31/2024`, `1/31/24`, `31-Jan-2024`, `Jan 31 2024`, `Jan 31, 2024`,
/// `31 JAN 24` (thinkorswim), ISO datetimes (`2024-01-31T16:00:00Z`, `2024-01-31 00:00:00-05:00`).
pub fn parse_date(s: &str) -> Option<NaiveDate> {
    let t = s.trim().trim_matches('"').trim();
    if t.is_empty() {
        return None;
    }
    // ISO datetime: take date part.
    if t.len() >= 10 && t.as_bytes()[4] == b'-' && t.as_bytes()[7] == b'-' {
        if let Ok(d) = NaiveDate::parse_from_str(&t[..10], "%Y-%m-%d") {
            return Some(d);
        }
    }
    const FMTS: &[&str] = &[
        "%Y-%m-%d", "%Y/%m/%d", "%Y%m%d", "%m/%d/%Y", "%m/%d/%y", "%m-%d-%Y", "%d-%b-%Y", "%d-%b-%y",
        "%b %d %Y", "%b %d, %Y", "%B %d, %Y", "%B %d %Y", "%d %b %Y", "%d %b %y", "%d.%m.%Y", "%Y.%m.%d",
    ];
    for f in FMTS {
        if let Ok(d) = NaiveDate::parse_from_str(t, f) {
            return Some(d);
        }
    }
    // Title-case month abbreviations for upper-case inputs like "20 SEP 24".
    let titled = title_case_months(t);
    if titled != t {
        for f in FMTS {
            if let Ok(d) = NaiveDate::parse_from_str(&titled, f) {
                return Some(d);
            }
        }
    }
    // Try first whitespace-separated token (e.g. "2024-01-31 16:00").
    if let Some(first) = t.split_whitespace().next() {
        if first != t {
            return parse_date(first);
        }
    }
    None
}

fn title_case_months(s: &str) -> String {
    s.split(' ')
        .map(|w| {
            if w.len() >= 3 && w.chars().all(|c| c.is_ascii_alphabetic()) {
                let mut c = w.chars();
                let f = c.next().unwrap().to_ascii_uppercase();
                std::iter::once(f).chain(c.map(|x| x.to_ascii_lowercase())).collect()
            } else {
                w.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalise a header name: lowercase ASCII alphanumerics only.
pub fn norm_header(h: &str) -> String {
    h.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

/// Filesystem-safe version of a ticker (`^VIX` -> `_VIX`, `BRK.B` -> `BRK.B`).
pub fn safe_name(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_') { c.to_ascii_uppercase() } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ny_time() {
        // 2025-02-21 21:00:00 UTC = 16:00 EST
        let t = unix_to_new_york(1740171600);
        assert_eq!(t.to_string(), "2025-02-21 16:00:00");
        // 2026-03-24 17:31:18 UTC = 13:31:18 EDT
        let t = unix_to_new_york(1774373478);
        assert_eq!(t.to_string(), "2026-03-24 13:31:18");
    }
    #[test]
    fn dates() {
        let d = NaiveDate::from_ymd_opt(2024, 9, 20).unwrap();
        for s in ["2024-09-20", "2024/09/20", "20240920", "09/20/2024", "9/20/2024", "20-Sep-2024", "Sep 20 2024",
            "Sep 20, 2024", "20 SEP 24", "2024-09-20T16:00:00Z", "2024-09-20 00:00:00-04:00"] {
            assert_eq!(parse_date(s), Some(d), "{s}");
        }
        assert_eq!(parse_date("garbage"), None);
    }
    #[test]
    fn nums() {
        assert_eq!(parse_num(" $1,234.50 "), Some(1234.5));
        assert_eq!(parse_num("25.3%"), Some(25.3));
        assert_eq!(parse_num("."), None);
        assert_eq!(parse_num("nan"), None);
        assert_eq!(parse_num("+0.12"), Some(0.12));
    }
}
