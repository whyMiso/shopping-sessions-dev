//! CSV dialect per rules.md: only `\n` ends a line, quotes are syntax only when they
//! wrap a whole field, exact field count, no value holds a comma/quote/newline.

/// Splits `line` (without its `\n`) into exactly N fields. Returns false if malformed.
/// Each output is (start, end) of the field's value (quotes stripped).
#[inline]
pub fn split<const N: usize>(line: &[u8], out: &mut [(u32, u32); N]) -> bool {
    let n = line.len();
    let mut f = 0usize;
    let mut start = 0usize;
    let mut i = 0usize;
    loop {
        // find end of field
        let mut end = start;
        let mut quotes = 0u32;
        while end < n {
            let c = line[end];
            if c == b',' {
                break;
            }
            quotes += (c == b'"') as u32;
            end += 1;
        }
        if f >= N {
            return false;
        }
        if quotes == 0 {
            out[f] = (start as u32, end as u32);
        } else if quotes == 2 && end - start >= 2 && line[start] == b'"' && line[end - 1] == b'"' {
            out[f] = (start as u32 + 1, end as u32 - 1);
        } else {
            return false;
        }
        f += 1;
        if end >= n {
            break;
        }
        start = end + 1;
        i += 1;
    }
    let _ = i;
    f == N
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
