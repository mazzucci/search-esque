//! Dates without a dependency: ISO 8601 parsing, civil-date arithmetic and calendar buckets,
//! all in UTC. Kept small so it compiles anywhere, WebAssembly included.

use crate::ast::CalendarInterval;

pub const MS_PER_DAY: i64 = 86_400_000;

/// Parses `YYYY-MM-DD`, optionally followed by `THH:MM[:SS[.fraction]]` and a zone
/// (`Z`, `+HH:MM`, `-HHMM`), into milliseconds since the epoch. A date alone is midnight UTC.
pub fn parse_iso(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y: i64 = s[0..4].parse().ok()?;
    let m: u32 = s[5..7].parse().ok()?;
    let d: u32 = s[8..10].parse().ok()?;
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        return None;
    }
    let mut ms = days_from_civil(y, m, d) * MS_PER_DAY;
    let rest = &s[10..];
    if rest.is_empty() {
        return Some(ms);
    }
    let rest = rest.strip_prefix('T').or_else(|| rest.strip_prefix(' '))?;
    let (time, zone) = split_zone(rest);
    let parts: Vec<&str> = time.split(':').collect();
    if parts.len() < 2 || parts.len() > 3 {
        return None;
    }
    let h: i64 = parts[0].parse().ok()?;
    let mi: i64 = parts[1].parse().ok()?;
    let (sec, frac_ms) = if parts.len() == 3 {
        let mut it = parts[2].splitn(2, '.');
        let sec: i64 = it.next()?.parse().ok()?;
        let frac_ms = match it.next() {
            None => 0,
            Some(f) => {
                if f.is_empty() || !f.bytes().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                let digits: String = f.chars().take(3).collect();
                let v: i64 = digits.parse().ok()?;
                v * 10_i64.pow(3 - digits.len() as u32)
            }
        };
        (sec, frac_ms)
    } else {
        (0, 0)
    };
    if h > 23 || mi > 59 || sec > 59 {
        return None;
    }
    ms += ((h * 60 + mi) * 60 + sec) * 1000 + frac_ms;
    ms -= zone_offset_ms(zone)?;
    Some(ms)
}

fn split_zone(s: &str) -> (&str, &str) {
    if let Some(t) = s.strip_suffix('Z') {
        return (t, "Z");
    }
    if let Some(pos) = s.rfind(['+', '-']) {
        if pos > 0 {
            return (&s[..pos], &s[pos..]);
        }
    }
    (s, "")
}

fn zone_offset_ms(zone: &str) -> Option<i64> {
    if zone.is_empty() || zone == "Z" {
        return Some(0);
    }
    let sign = if zone.starts_with('-') { -1 } else { 1 };
    let body = &zone[1..];
    let (h, m) = match body.len() {
        5 if body.as_bytes()[2] == b':' => (&body[0..2], &body[3..5]),
        4 => (&body[0..2], &body[2..4]),
        2 => (body, "0"),
        _ => return None,
    };
    let h: i64 = h.parse().ok()?;
    let m: i64 = m.parse().ok()?;
    Some(sign * (h * 60 + m) * 60_000)
}

/// A range bound. Elasticsearch rounds a date without a time part up to the last millisecond
/// of that day for `gt` and `lte` (so `lte: "2026-09-01"` includes the whole day) and down to
/// midnight for `gte` and `lt`. `round_up` asks for the former.
pub fn parse_bound(s: &str, round_up: bool) -> Option<i64> {
    let ms = parse_iso(s)?;
    Some(if round_up && s.len() == 10 {
        ms + MS_PER_DAY - 1
    } else {
        ms
    })
}

/// Why a string is not a date we accept, phrased as a hint.
pub fn date_hint(s: &str) -> &'static str {
    let b = s.as_bytes();
    if s.starts_with("now") {
        "date math such as now-1d is not supported yet; give an ISO date"
    } else if b.len() == 7 && b[4] == b'-' {
        "a month is not a date: 2026-01 should be 2026-01-01"
    } else if b.len() == 4 {
        "a year is not a date: 2026 should be 2026-01-01"
    } else {
        "give an ISO date like 2026-01-31 or 2026-01-31T12:00:00Z"
    }
}

pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

pub fn days_in_month(y: i64, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
    }
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
pub fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = ((m + 9) % 12) as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The civil date for a day count since 1970-01-01.
pub fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The start of the calendar bucket holding `ms`, in UTC.
pub fn bucket_start(ms: i64, iv: CalendarInterval) -> i64 {
    let days = ms.div_euclid(MS_PER_DAY);
    let (y, m, _) = civil_from_days(days);
    match iv {
        CalendarInterval::Day => days * MS_PER_DAY,
        CalendarInterval::Month => days_from_civil(y, m, 1) * MS_PER_DAY,
        CalendarInterval::Year => days_from_civil(y, 1, 1) * MS_PER_DAY,
    }
}

/// The start of the bucket after the one starting at `start`.
pub fn next_bucket(start: i64, iv: CalendarInterval) -> i64 {
    let days = start.div_euclid(MS_PER_DAY);
    let (y, m, _) = civil_from_days(days);
    match iv {
        CalendarInterval::Day => start + MS_PER_DAY,
        CalendarInterval::Month => {
            let (y, m) = if m == 12 { (y + 1, 1) } else { (y, m + 1) };
            days_from_civil(y, m, 1) * MS_PER_DAY
        }
        CalendarInterval::Year => days_from_civil(y + 1, 1, 1) * MS_PER_DAY,
    }
}

/// `YYYY-MM-DDTHH:MM:SS.mmmZ`, the form Elasticsearch uses for `key_as_string`.
pub fn format_iso(ms: i64) -> String {
    let days = ms.div_euclid(MS_PER_DAY);
    let rem = ms.rem_euclid(MS_PER_DAY);
    let (y, m, d) = civil_from_days(days);
    let h = rem / 3_600_000;
    let mi = (rem / 60_000) % 60;
    let s = (rem / 1000) % 60;
    let frac = rem % 1000;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        y, m, d, h, mi, s, frac
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_and_formats() {
        assert_eq!(parse_iso("1970-01-01"), Some(0));
        assert_eq!(parse_iso("2026-10-08T15:41:59Z"), Some(1_791_474_119_000));
        assert_eq!(
            parse_iso("2026-10-08T15:41:59.250Z"),
            Some(1_791_474_119_250)
        );
        assert_eq!(
            parse_iso("2026-10-08T17:41:59+02:00"),
            Some(1_791_474_119_000)
        );
        assert_eq!(format_iso(1_791_474_119_250), "2026-10-08T15:41:59.250Z");
        assert_eq!(parse_iso("2026-10"), None);
        assert_eq!(parse_iso("2026-02-30"), None);
        assert_eq!(parse_iso("now-1d"), None);
    }

    #[test]
    fn buckets() {
        let ts = parse_iso("2026-10-08T15:41:59Z").unwrap();
        assert_eq!(
            format_iso(bucket_start(ts, CalendarInterval::Month)),
            "2026-10-01T00:00:00.000Z"
        );
        assert_eq!(
            format_iso(bucket_start(ts, CalendarInterval::Year)),
            "2026-01-01T00:00:00.000Z"
        );
        assert_eq!(
            format_iso(bucket_start(ts, CalendarInterval::Day)),
            "2026-10-08T00:00:00.000Z"
        );
        let dec = parse_iso("2025-12-31T23:00:00Z").unwrap();
        let start = bucket_start(dec, CalendarInterval::Month);
        assert_eq!(
            format_iso(next_bucket(start, CalendarInterval::Month)),
            "2026-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn civil_round_trip() {
        for days in [-800_000_i64, -1, 0, 1, 20_000, 60_000] {
            let (y, m, d) = civil_from_days(days);
            assert_eq!(days_from_civil(y, m, d), days);
        }
    }
}
