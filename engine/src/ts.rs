//! Timestamp parsing (F1 / F2 / F3) into epoch milliseconds. Returns None for any bad form.

pub const DAY_MS: i64 = 86_400_000;

#[inline]
fn d2(b: &[u8], i: usize) -> Option<i64> {
    let a = b[i].wrapping_sub(b'0');
    let c = b[i + 1].wrapping_sub(b'0');
    if a < 10 && c < 10 {
        Some((a as i64) * 10 + c as i64)
    } else {
        None
    }
}

#[inline]
pub fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

#[inline]
pub fn days_in_month(y: i64, m: i64) -> i64 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Howard Hinnant's days_from_civil.
#[inline]
pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// Parses `YYYY-MM-DDTHH:MM:SS` (first 19 bytes) into epoch ms of that wall time.
#[inline]
fn civil19(b: &[u8]) -> Option<i64> {
    if b[4] != b'-' || b[7] != b'-' || b[10] != b'T' || b[13] != b':' || b[16] != b':' {
        return None;
    }
    let y = d2(b, 0)? * 100 + d2(b, 2)?;
    let mo = d2(b, 5)?;
    let d = d2(b, 8)?;
    let h = d2(b, 11)?;
    let mi = d2(b, 14)?;
    let s = d2(b, 17)?;
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let days = days_from_civil(y, mo, d);
    Some(((days * 24 + h) * 60 + mi) * 60_000 + s * 1000)
}

/// Full validating parser. Returns epoch ms, or None for a bad form.
/// F3 values that overflow i64 return None (they are outside any window).
pub fn parse_ts(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if n == 0 {
        return None;
    }
    // F3: digits only.
    if b.iter().all(|c| c.is_ascii_digit()) {
        let mut i = 0;
        while i < n && b[i] == b'0' {
            i += 1;
        }
        if n - i > 19 {
            return None;
        }
        let mut v: u64 = 0;
        for &c in &b[i..] {
            v = v * 10 + (c - b'0') as u64;
        }
        if v > i64::MAX as u64 {
            return None;
        }
        return Some(v as i64);
    }
    if n == 20 {
        if b[19] != b'Z' {
            return None;
        }
        return civil19(b);
    }
    if n == 25 {
        let sign = match b[19] {
            b'+' => 1,
            b'-' => -1,
            _ => return None,
        };
        if b[22] != b':' {
            return None;
        }
        let oh = d2(b, 20)?;
        let om = d2(b, 23)?;
        if oh > 23 || om > 59 {
            return None;
        }
        let local = civil19(b)?;
        return Some(local - sign * (oh * 3_600_000 + om * 60_000));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn basics() {
        assert_eq!(parse_ts(b"2026-10-03T23:58:10Z"), Some(1791071890000));
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-04:00"), Some(1791071890000));
        assert_eq!(parse_ts(b"1791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"0001791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"2026-10-03T23:58:10z"), None);
        assert_eq!(parse_ts(b"2026-02-29T00:00:00Z"), None);
        assert!(parse_ts(b"2028-02-29T00:00:00Z").is_some());
        assert_eq!(parse_ts(b"2026-10-03T24:00:00Z"), None);
        assert_eq!(parse_ts(b"2026-10-03T10:00:00+24:00"), None);
        assert_eq!(parse_ts(b"2026-10-03T10:00:00+05:60"), None);
        assert!(parse_ts(b"2026-10-03T10:00:00-00:00").is_some());
        assert_eq!(parse_ts(b"99999999999999999999"), None);
        assert_eq!(parse_ts(b"-5"), None);
    }
}
