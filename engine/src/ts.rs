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
/// Value of an all-digit slice of length <= 18, or None if any byte is not an ASCII digit.
#[inline]
fn digits_fast(b: &[u8]) -> Option<u64> {
    let n = b.len();
    let mut v: u64 = 0;
    let mut i = 0;
    while i + 8 <= n {
        let w = u64::from_le_bytes(b[i..i + 8].try_into().unwrap());
        let t = w ^ 0x3030_3030_3030_3030;
        if (t | t.wrapping_add(0x7676_7676_7676_7676)) & 0x8080_8080_8080_8080 != 0 {
            return None;
        }
        let mut x = t.wrapping_mul(2561) >> 8;
        x = (x & 0x00FF_00FF_00FF_00FF).wrapping_mul(6_553_601) >> 16;
        x = (x & 0x0000_FFFF_0000_FFFF).wrapping_mul(42_949_672_960_001) >> 32;
        v = v * 100_000_000 + x;
        i += 8;
    }
    while i < n {
        let d = b[i].wrapping_sub(b'0');
        if d >= 10 {
            return None;
        }
        v = v * 10 + d as u64;
        i += 1;
    }
    Some(v)
}

pub fn parse_ts(b: &[u8]) -> Option<i64> {
    let n = b.len();
    if n == 0 {
        return None;
    }
    if n <= 18 {
        if let Some(v) = digits_fast(b) {
            return Some(v as i64);
        }
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

/// Direct-mapped cache of validated `YYYY-MM-DD` prefixes -> days since epoch.
pub struct DateCache {
    k1: [u64; 64],
    k2: [u16; 64],
    v: [i64; 64],
    used: [bool; 64],
}

impl DateCache {
    pub fn new() -> Self {
        DateCache { k1: [0; 64], k2: [0; 64], v: [0; 64], used: [false; 64] }
    }
}

#[inline]
fn date_days(b: &[u8]) -> Option<i64> {
    if b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let y = d2(b, 0)? * 100 + d2(b, 2)?;
    let mo = d2(b, 5)?;
    let d = d2(b, 8)?;
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) {
        return None;
    }
    Some(days_from_civil(y, mo, d))
}

/// Same result as `parse_ts`, with a date-prefix cache for F1/F2.
#[inline]
pub fn parse_ts_c(b: &[u8], c: &mut DateCache) -> Option<i64> {
    let n = b.len();
    if n != 20 && n != 25 {
        return parse_ts(b);
    }
    let k1 = u64::from_le_bytes(b[0..8].try_into().unwrap());
    let k2 = u16::from_le_bytes([b[8], b[9]]);
    let h = ((k1 ^ ((k2 as u64) << 17)).wrapping_mul(0x9E3779B97F4A7C15) >> 58) as usize;
    let days = if c.used[h] && c.k1[h] == k1 && c.k2[h] == k2 {
        c.v[h]
    } else {
        match date_days(b) {
            Some(d) => {
                c.used[h] = true;
                c.k1[h] = k1;
                c.k2[h] = k2;
                c.v[h] = d;
                d
            }
            None => return None,
        }
    };
    if b[10] != b'T' {
        return None;
    }
    // "HH:MM:SS" as one word: colons at bytes 2 and 5, digits elsewhere.
    let w = u64::from_le_bytes(b[11..19].try_into().unwrap());
    let t = w ^ 0x3030_3030_3030_3030;
    const COLONS: u64 = 0x0000_FF00_00FF_0000;
    if t & COLONS != 0x0000_0A00_000A_0000 {
        return None;
    }
    let d = t & !COLONS;
    if (d | d.wrapping_add(0x7676_7676_7676_7676)) & 0x8080_8080_8080_8080 != 0 {
        return None;
    }
    let h = (d & 0xFF) * 10 + ((d >> 8) & 0xFF);
    let mi = ((d >> 24) & 0xFF) * 10 + ((d >> 32) & 0xFF);
    let s = ((d >> 48) & 0xFF) * 10 + ((d >> 56) & 0xFF);
    let (h, mi, s) = (h as i64, mi as i64, s as i64);
    if h > 23 || mi > 59 || s > 59 {
        return None;
    }
    let local = ((days * 24 + h) * 60 + mi) * 60_000 + s * 1000;
    if n == 20 {
        return if b[19] == b'Z' { Some(local) } else { None };
    }
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
    Some(local - sign * (oh * 3_600_000 + om * 60_000))
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
    #[test]
    fn cached_equals_slow() {
        let mut c = DateCache::new();
        let mut xs: Vec<Vec<u8>> = Vec::new();
        for y in [2025, 2026, 2028, 2100, 1999] {
            for mo in 0..=13 {
                for d in [0, 1, 15, 28, 29, 30, 31, 32] {
                    for t in [&b"T00:00:00Z"[..], b"T23:59:59Z", b"T24:00:00Z", b"T10:60:00Z", b"T10:00:60Z", b"t10:00:00Z", b"T10:00:00z", b"T10:00:00+05:30", b"T10:00:00-23:59", b"T10:00:00+24:00", b"T10:00:00+05:60", b"T10:00:00+0530", b"T10:00:00-00:00"] {
                        xs.push(format!("{y:04}-{mo:02}-{d:02}").into_bytes().into_iter().chain(t.iter().copied()).collect());
                    }
                }
            }
        }
        xs.push(b"2026-10-03 10:00:00Z".to_vec());
        xs.push(b"2026-10-03T10:00:00.5Z".to_vec());
        xs.push(b"20x6-10-03T10:00:00Z".to_vec());
        for x in &xs {
            for _ in 0..2 {
                assert_eq!(parse_ts_c(x, &mut c), parse_ts(x), "{}", String::from_utf8_lossy(x));
            }
        }
    }
}
