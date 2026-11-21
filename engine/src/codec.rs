//! Injective ID codec into u64.
//!
//! Class A (bit 63 clear): length 1..=9, first byte anything, the rest from the
//! alphabet [0-9_a-z]. code = first_byte << 48 | 6-bit symbols (end = 0), so numeric
//! order of class-A codes equals byte-wise order of the strings.
//! Class B (bit 63 set): index into a global exact string table.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Mutex;

pub const CLASS_B: u64 = 1 << 63;
const LOW48: u64 = (1u64 << 48) - 1;

static DIGIT: [u8; 256] = {
    let mut t = [0xFFu8; 256];
    let mut i = 0;
    while i < 10 {
        t[b'0' as usize + i] = 1 + i as u8;
        i += 1;
    }
    t[b'_' as usize] = 11;
    let mut j = 0;
    while j < 26 {
        t[b'a' as usize + j] = 12 + j as u8;
        j += 1;
    }
    t
};
static SYM: [u8; 38] = *b"\x000123456789_abcdefghijklmnopqrstuvwxyz";

/// Class-A encoding if possible. `s` must be non-empty.
#[inline]
pub fn encode_a(s: &[u8]) -> Option<u64> {
    let n = s.len();
    if n == 0 || n > 9 {
        return None;
    }
    let mut v: u64 = 0;
    let mut bad = 0u8;
    for i in 1..9 {
        let d = if i < n { DIGIT[s[i] as usize] } else { 0 };
        bad |= d;
        v = (v << 6) | (d & 0x3F) as u64;
    }
    if bad & 0x80 != 0 {
        return None;
    }
    Some(((s[0] as u64) << 48) | v)
}

pub struct StrTable {
    map: HashMap<Vec<u8>, u32>,
    strs: Vec<Vec<u8>>,
}

pub static TABLE: Mutex<Option<StrTable>> = Mutex::new(None);

pub fn init() {
    *TABLE.lock().unwrap() = Some(StrTable { map: HashMap::new(), strs: Vec::new() });
}

/// Encode, inserting into the global table when needed.
#[inline]
pub fn encode(s: &[u8]) -> u64 {
    if let Some(c) = encode_a(s) {
        return c;
    }
    encode_slow(s)
}

#[cold]
#[inline(never)]
pub fn encode_slow(s: &[u8]) -> u64 {
    let mut g = TABLE.lock().unwrap();
    let t = g.as_mut().unwrap();
    if let Some(&i) = t.map.get(s) {
        return CLASS_B | i as u64;
    }
    let i = t.strs.len() as u32;
    t.strs.push(s.to_vec());
    t.map.insert(s.to_vec(), i);
    CLASS_B | i as u64
}

/// Encode without inserting: None if the string is class B and not in the table.
pub fn lookup(s: &[u8]) -> Option<u64> {
    if let Some(c) = encode_a(s) {
        return Some(c);
    }
    let g = TABLE.lock().unwrap();
    g.as_ref().unwrap().map.get(s).map(|&i| CLASS_B | i as u64)
}

pub fn decode_into(c: u64, out: &mut Vec<u8>) {
    if c & CLASS_B != 0 {
        let g = TABLE.lock().unwrap();
        out.extend_from_slice(&g.as_ref().unwrap().strs[(c & !CLASS_B) as usize]);
        return;
    }
    out.push((c >> 48) as u8);
    let v = c & LOW48;
    let mut digs = [0u8; 8];
    for i in 0..8 {
        digs[i] = ((v >> (6 * (7 - i))) & 0x3F) as u8;
    }
    for d in digs {
        if d == 0 {
            break;
        }
        out.push(SYM[d as usize]);
    }
}

pub fn decode(c: u64) -> Vec<u8> {
    let mut v = Vec::with_capacity(12);
    decode_into(c, &mut v);
    v
}

/// Byte-wise comparison of the strings behind two codes.
#[inline]
pub fn cmp_codes(a: u64, b: u64) -> Ordering {
    if (a | b) & CLASS_B == 0 {
        return a.cmp(&b);
    }
    if a == b {
        return Ordering::Equal;
    }
    decode(a).cmp(&decode(b))
}

#[inline]
pub fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^ (z >> 31)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn roundtrip_order() {
        init();
        let xs: Vec<&[u8]> = vec![
            b"u1", b"U1", b"u", b"u10", b"u1 ", b" u1", b"c_12", b"e42huhoug", b"e42huhou", b"zzzzzzzzz",
            b"zzzzzzzzzz", b"a\x00", b"a", b"\x00", b"user_id_a", b"p13649", b"k_", b"a\tb",
        ];
        for a in &xs {
            let c = encode(a);
            assert_eq!(&decode(c)[..], *a);
            for b in &xs {
                let d = encode(b);
                assert_eq!(cmp_codes(c, d), a.cmp(b), "{:?} {:?}", a, b);
                assert_eq!(c == d, a == b);
            }
        }
    }
}
