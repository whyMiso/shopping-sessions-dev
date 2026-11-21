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

// ---------------------------------------------------------------- mask scanner

/// Bit i of each mask: byte i of the 64-byte block at `p` is `\n` / `,` / `"`.
#[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
#[inline(always)]
unsafe fn masks64(p: *const u8) -> (u64, u64, u64) {
    use std::arch::x86_64::*;
    let a = _mm256_loadu_si256(p as *const __m256i);
    let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
    let m = |v: __m256i| -> u64 {
        ((_mm256_movemask_epi8(_mm256_cmpeq_epi8(b, v)) as u32 as u64) << 32)
            | (_mm256_movemask_epi8(_mm256_cmpeq_epi8(a, v)) as u32 as u64)
    };
    (m(_mm256_set1_epi8(b'\n' as i8)), m(_mm256_set1_epi8(b',' as i8)), m(_mm256_set1_epi8(b'"' as i8)))
}

#[cfg(not(all(target_arch = "x86_64", target_feature = "avx2")))]
#[inline(always)]
unsafe fn masks64(p: *const u8) -> (u64, u64, u64) {
    #[inline(always)]
    fn bits(flags: u64) -> u64 {
        ((flags & 0x8080_8080_8080_8080) >> 7).wrapping_mul(0x0102_0408_1020_4080) >> 56
    }
    let (mut nl, mut cm, mut qm) = (0u64, 0u64, 0u64);
    for i in 0..8 {
        let w = (p.add(8 * i) as *const u64).read_unaligned();
        nl |= bits(zero_bytes(w ^ (b'\n' as u64 * ONES))) << (8 * i);
        cm |= bits(zero_bytes(w ^ (b',' as u64 * ONES))) << (8 * i);
        qm |= bits(zero_bytes(w ^ (b'"' as u64 * ONES))) << (8 * i);
    }
    (nl, cm, qm)
}

/// Calls `f(line_start, line_end, commas, n_commas, n_quotes)` for every line that starts in
/// `[start, lim)`. `buf` must have at least 64 zero bytes of padding after `data_end`.
/// A final line without `\n` ends at `data_end`. Commas past 16 are counted, not stored.
#[inline(always)]
pub fn scan_lines(buf: &[u8], start: usize, lim: usize, data_end: usize, mut f: impl FnMut(usize, usize, &[u32; 16], usize, u32)) {
    debug_assert!(buf.len() >= data_end + 64);
    let mut ls = start;
    if ls >= lim || ls >= data_end {
        return;
    }
    let base = buf.as_ptr();
    let mut blk = ls & !63;
    let (mut nl, mut cm, mut qm) = unsafe { masks64(base.add(blk)) };
    let low = !0u64 << (ls - blk);
    nl &= low;
    cm &= low;
    qm &= low;
    loop {
        let mut commas = [0u32; 16];
        let mut nc = 0usize;
        let mut quotes = 0u32;
        let le;
        loop {
            if nl != 0 {
                let e = nl.trailing_zeros();
                let m = (1u64 << e) - 1;
                let mut c = cm & m;
                quotes += (qm & m).count_ones();
                while c != 0 {
                    if nc < 16 {
                        commas[nc] = blk as u32 + c.trailing_zeros();
                    }
                    nc += 1;
                    c &= c - 1;
                }
                le = blk + e as usize;
                // drop bits up to and including the newline
                if e == 63 {
                    nl = 0;
                    cm = 0;
                    qm = 0;
                } else {
                    let keep = !0u64 << (e + 1);
                    nl &= keep;
                    cm &= keep;
                    qm &= keep;
                }
                break;
            }
            let mut c = cm;
            quotes += qm.count_ones();
            while c != 0 {
                if nc < 16 {
                    commas[nc] = blk as u32 + c.trailing_zeros();
                }
                nc += 1;
                c &= c - 1;
            }
            blk += 64;
            if blk >= data_end {
                // last line without a newline
                f(ls, data_end, &commas, nc, quotes);
                return;
            }
            let m = unsafe { masks64(base.add(blk)) };
            nl = m.0;
            cm = m.1;
            qm = m.2;
        }
        f(ls, le, &commas, nc, quotes);
        ls = le + 1;
        if ls >= lim || ls >= data_end {
            return;
        }
        // ensure the cached block is the one containing `ls` (newline at bit 63 moves on)
        if ls >= blk + 64 {
            blk = ls & !63;
            let m = unsafe { masks64(base.add(blk)) };
            let low = !0u64 << (ls - blk);
            nl = m.0 & low;
            cm = m.1 & low;
            qm = m.2 & low;
        }
    }
}

/// Fields of a line from precomputed comma positions (absolute) and quote count.
/// `line_start` is the absolute start of the line; outputs are relative to it.
#[inline(always)]
pub fn fields<const N: usize>(buf: &[u8], ls: usize, le: usize, commas: &[u32; 16], nc: usize, quotes: u32, out: &mut [(u32, u32); N]) -> bool {
    if nc != N - 1 {
        return false;
    }
    let mut start = ls;
    let mut qf = 0u32;
    for f in 0..N {
        let end = if f < N - 1 { commas[f] as usize } else { le };
        if end - start >= 2 && buf[start] == b'"' && buf[end - 1] == b'"' {
            out[f] = ((start - ls) as u32 + 1, (end - ls) as u32 - 1);
            qf += 1;
        } else {
            out[f] = ((start - ls) as u32, (end - ls) as u32);
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

    #[test]
    fn scan_matches_split() {
        // pseudo-random lines from a small alphabet, many blocks, with and without final newline
        let alpha = b"ab,,\"\" \r1\t";
        let mut x: u64 = 0x1234_5678_9abc_def0;
        for round in 0..300 {
            let mut data: Vec<u8> = Vec::new();
            let nlines = 1 + (round % 40);
            for _ in 0..nlines {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                let len = (x % 150) as usize;
                for _ in 0..len {
                    x ^= x << 13;
                    x ^= x >> 7;
                    x ^= x << 17;
                    data.push(alpha[(x % alpha.len() as u64) as usize]);
                }
                data.push(b'\n');
            }
            if round % 3 == 0 {
                data.pop();
            }
            let n = data.len();
            let mut buf = data.clone();
            buf.extend_from_slice(&[0u8; 64]);
            let mut got: Vec<(usize, usize, Option<Vec<Vec<u8>>>)> = Vec::new();
            scan_lines(&buf, 0, n, n, |ls, le, commas, nc, quotes| {
                let mut o = [(0u32, 0u32); 3];
                let r = if fields::<3>(&buf, ls, le, commas, nc, quotes, &mut o) {
                    Some(o.iter().map(|&(a, b)| buf[ls + a as usize..ls + b as usize].to_vec()).collect())
                } else {
                    None
                };
                got.push((ls, le, r));
            });
            let mut exp = Vec::new();
            let mut p = 0;
            for line in data.split(|&c| c == b'\n') {
                if p >= n {
                    break;
                }
                let mut o = [(0u32, 0u32); 3];
                let r = if split::<3>(line, &mut o) {
                    Some(o.iter().map(|&(a, b)| line[a as usize..b as usize].to_vec()).collect())
                } else {
                    None
                };
                exp.push((p, p + line.len(), r));
                p += line.len() + 1;
            }
            assert_eq!(got, exp, "round {round}");
        }
    }
}
