//! CSV dialect per rules.md: only `\n` ends a line, quotes are syntax only when they
//! wrap a whole field, exact field count, no value holds a comma/quote/newline.

const LO7: u64 = 0x7F7F_7F7F_7F7F_7F7F;
const ONES: u64 = 0x0101_0101_0101_0101;

/// 0x80 in every byte of `x` that is zero (exact, no false positives).
#[inline(always)]
fn zero_bytes(x: u64) -> u64 {
    !(((x & LO7) + LO7) | x | LO7)
}

/// Splits `line` (without its `\n`) into exactly N fields. Returns false if malformed.
/// Each output is (start, end) of the field's value (quotes stripped).
/// Well-formed iff the number of quotes is exactly twice the number of fields that
/// both start and end with a quote (and have length >= 2).
#[inline]
pub fn split<const N: usize>(line: &[u8], out: &mut [(u32, u32); N]) -> bool {
    let n = line.len();
    let mut commas = [0u32; 16];
    let mut nc = 0usize;
    let mut quotes = 0u32;
    let mut i = 0usize;
    while i < n {
        let w = if i + 8 <= n {
            u64::from_le_bytes(line[i..i + 8].try_into().unwrap())
        } else {
            let mut t = [0u8; 8];
            t[..n - i].copy_from_slice(&line[i..]);
            u64::from_le_bytes(t)
        };
        let mut cm = zero_bytes(w ^ (b',' as u64 * ONES));
        let qm = zero_bytes(w ^ (b'"' as u64 * ONES));
        quotes += qm.count_ones();
        while cm != 0 {
            if nc >= N - 1 {
                return false;
            }
            commas[nc] = (i as u32) + cm.trailing_zeros() / 8;
            nc += 1;
            cm &= cm - 1;
        }
        i += 8;
    }
    if nc != N - 1 {
        return false;
    }
    let mut start = 0usize;
    let mut qf = 0u32;
    for f in 0..N {
        let end = if f < N - 1 { commas[f] as usize } else { n };
        if end - start >= 2 && line[start] == b'"' && line[end - 1] == b'"' {
            out[f] = (start as u32 + 1, end as u32 - 1);
            qf += 1;
        } else {
            out[f] = (start as u32, end as u32);
        }
        start = end + 1;
    }
    quotes == 2 * qf
}

#[cfg(test)]
mod tests {
    use super::*;
    fn s<const N: usize>(x: &[u8]) -> Option<Vec<Vec<u8>>> {
        let mut o = [(0u32, 0u32); N];
        if split::<N>(x, &mut o) {
            Some(o.iter().map(|&(a, b)| x[a as usize..b as usize].to_vec()).collect())
        } else {
            None
        }
    }
    #[test]
    fn dialect() {
        assert_eq!(s::<2>(b"a,b"), Some(vec![b"a".to_vec(), b"b".to_vec()]));
        assert_eq!(s::<2>(b"\"a\",\"\""), Some(vec![b"a".to_vec(), b"".to_vec()]));
        assert_eq!(s::<2>(b""), None);
        assert_eq!(s::<2>(b"a"), None);
        assert_eq!(s::<2>(b"a,b,c"), None);
        assert_eq!(s::<2>(b"a,b\""), None);
        assert_eq!(s::<2>(b"a,\""), None);
        assert_eq!(s::<2>(b"a,\"\"\"\""), None);
        assert_eq!(s::<2>(b"a,\"b\"c"), None);
        assert_eq!(s::<2>(b"a,"), Some(vec![b"a".to_vec(), b"".to_vec()]));
        assert_eq!(s::<2>(b",\r"), Some(vec![b"".to_vec(), b"\r".to_vec()]));
    }
}
