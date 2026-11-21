mod codec;
mod csv;
mod parquet;
mod ts;

use codec::{cmp_codes, decode_into, encode, mix};
use std::collections::HashMap;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering as AO};
use std::sync::Mutex;
use ts::{parse_ts, parse_ts_c, DateCache, DAY_MS};

const WEEK_MS: i64 = 7 * DAY_MS;
const GAP_MS: i64 = 1_800_000;
const LATE_MS: i64 = DAY_MS;
const CHUNK: u64 = 8 << 20;
const NONE_CAT: u32 = u32::MAX;

// ---------------------------------------------------------------- hash table u64 -> u64
const EMPTY: u64 = u64::MAX;
struct Map64 {
    ents: Vec<[u64; 2]>,
    mask: usize,
}
impl Map64 {
    fn with_capacity(n: usize) -> Self {
        let cap = (n.max(8) * 2).next_power_of_two();
        Map64 { ents: vec![[EMPTY, 0]; cap], mask: cap - 1 }
    }
    /// Touch the slot so the cache line is on its way before `get`.
    #[inline(always)]
    fn prefetch(&self, k: u64) {
        let i = mix(k) as usize & self.mask;
        let p = unsafe { self.ents.as_ptr().add(i) };
        #[cfg(target_arch = "x86_64")]
        unsafe {
            std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0);
        }
        #[cfg(not(target_arch = "x86_64"))]
        unsafe {
            std::hint::black_box(std::ptr::read_volatile(p as *const u64));
        }
    }
    #[inline]
    fn get(&self, k: u64) -> Option<u64> {
        let mut i = mix(k) as usize & self.mask;
        loop {
            let e = self.ents[i];
            if e[0] == k {
                return Some(e[1]);
            }
            if e[0] == EMPTY {
                return None;
            }
            i = (i + 1) & self.mask;
        }
    }
    /// Insert if absent; returns the stored value.
    fn insert(&mut self, k: u64, v: u64) -> u64 {
        let mut i = mix(k) as usize & self.mask;
        loop {
            let e = self.ents[i];
            if e[0] == k {
                return e[1];
            }
            if e[0] == EMPTY {
                self.ents[i] = [k, v];
                return v;
            }
            i = (i + 1) & self.mask;
        }
    }
}

// ---------------------------------------------------------------- inputs
struct Chunk {
    file: usize,
    start: u64,
    end: u64,
    kind: u8, // 0 links, 1 events, 2 clicks
}

fn list_csv(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name();
            let n = name.as_encoded_bytes();
            if !n.ends_with(b".csv") {
                continue;
            }
            let p = e.path();
            if let Ok(md) = std::fs::metadata(&p) {
                if md.is_file() {
                    v.push((p, md.len()));
                }
            }
        }
    }
    v.sort();
    v
}

/// Calls `f(line)` for every line that starts in [start, end) of the file, excluding the header.
fn for_lines(file: &File, size: u64, start: u64, end: u64, buf: &mut Vec<u8>, mut f: impl FnMut(&[u8])) {
    let from = if start == 0 { 0 } else { start - 1 };
    let mut want = (end - from + 65536).min(size - from) as usize;
    loop {
        buf.resize(want, 0);
        file.read_exact_at(&mut buf[..want], from).expect("read");
        // position of first line start >= start
        let mut pos: usize;
        if start == 0 {
            // skip header line
            match memchr(b'\n', &buf[..want]) {
                Some(p) => pos = p + 1,
                None => return,
            }
        } else {
            match memchr(b'\n', &buf[..want]) {
                Some(p) => pos = p + 1,
                None => {
                    if from + want as u64 >= size {
                        return;
                    }
                    want = (want * 2).min((size - from) as usize);
                    continue;
                }
            }
        }
        let lim = (end - from) as usize;
        let at_eof = from + want as u64 >= size;
        // Check that the last line starting before lim is complete in buf.
        let mut ok = true;
        {
            let mut p = pos;
            while p < lim {
                match memchr(b'\n', &buf[p..want]) {
                    Some(q) => p += q + 1,
                    None => {
                        if !at_eof {
                            ok = false;
                        }
                        break;
                    }
                }
            }
        }
        if !ok {
            want = (want * 2).min((size - from) as usize);
            continue;
        }
        while pos < lim && pos < want {
            let e = match memchr(b'\n', &buf[pos..want]) {
                Some(q) => pos + q,
                None => want,
            };
            f(&buf[pos..e]);
            pos = e + 1;
        }
        return;
    }
}

#[inline]
fn memchr(c: u8, s: &[u8]) -> Option<usize> {
    let n = s.len();
    let mut i = 0;
    let pat = (c as u64) * 0x0101010101010101;
    while i + 8 <= n {
        let w = u64::from_le_bytes(s[i..i + 8].try_into().unwrap()) ^ pat;
        let t = w.wrapping_sub(0x0101010101010101) & !w & 0x8080808080808080;
        if t != 0 {
            return Some(i + (t.trailing_zeros() / 8) as usize);
        }
        i += 8;
    }
    while i < n {
        if s[i] == c {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn read_small_csv<const N: usize>(path: &Path) -> Vec<[Vec<u8>; N]> {
    let data = std::fs::read(path).unwrap_or_default();
    let mut out = Vec::new();
    let mut first = true;
    for line in data.split(|&c| c == b'\n') {
        if first {
            first = false;
            continue;
        }
        let mut f = [(0u32, 0u32); N];
        if csv::split::<N>(line, &mut f) {
            out.push(std::array::from_fn(|i| line[f[i].0 as usize..f[i].1 as usize].to_vec()));
        }
    }
    // a trailing empty "line" after the final \n is not a line
    out
}

fn manifest(path: &Path) -> (i64, i64) {
    let s = std::fs::read(path).expect("manifest");
    let get = |key: &[u8]| -> i64 {
        let p = s.windows(key.len()).position(|w| w == key).expect("key") + key.len();
        let rest = &s[p..];
        let q1 = rest.iter().position(|&c| c == b'"').unwrap() + 1;
        let q2 = q1 + rest[q1..].iter().position(|&c| c == b'"').unwrap();
        parse_ts(&rest[q1..q2]).expect("manifest ts")
    };
    (get(b"\"window_start\""), get(b"\"window_end\""))
}

// ---------------------------------------------------------------- records
const NOCAT: u32 = 0xFF;
const DEAD: u64 = u64::MAX;
const BLK: usize = 2048;

/// Event record. `person == DEAD` marks a tombstone (invalid/late kept copy).
#[derive(Clone, Copy)]
struct EvRec {
    code: u64,
    person: u64,
    ts: u32,     // ms relative to base (ws - 7 days)
    ingest: u32, // seconds, saturating, relative to ws/1000 - 2^31
    pc: u32,     // price << 8 | category (category NOCAT for non-purchases)
}

/// Sorted by (person, tp): tp = ts << 32 | pc.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct SRec {
    person: u64,
    tp: u64,
}

#[derive(Clone, Copy)]
struct CRec {
    person: u64,
    tc: u64, // ts << 32 | camp
    code: u64,
}

fn push_blk<T>(list: &mut Vec<Vec<T>>, v: T) {
    if let Some(l) = list.last_mut() {
        if l.len() < l.capacity() {
            l.push(v);
            return;
        }
    }
    let mut n = Vec::with_capacity(BLK);
    n.push(v);
    list.push(n);
}

struct Ctx {
    ws: i64,
    we: i64,
    link: Map64,
    prod_a: Map64,
    prod_b: HashMap<Vec<u8>, u32>,
    unknown_cat: u32,
    camp_a: Map64,
    camp_b: HashMap<Vec<u8>, u32>,
    p1_bits: u32,
    q_bits: u32,
    base: i64,
    ing0: i64,
}

impl Ctx {
    #[inline]
    fn person(&self, user: &[u8]) -> u64 {
        let c = encode(user);
        self.link.get(c).unwrap_or(c)
    }
    #[inline]
    fn cat(&self, prod: &[u8]) -> u32 {
        if let Some(c) = codec::encode_a(prod) {
            self.prod_a.get(c).map(|v| v as u32).unwrap_or(self.unknown_cat)
        } else {
            *self.prod_b.get(prod).unwrap_or(&self.unknown_cat)
        }
    }
    #[inline]
    fn camp(&self, c: &[u8]) -> Option<u32> {
        if let Some(code) = codec::encode_a(c) {
            self.camp_a.get(code).map(|v| v as u32)
        } else {
            self.camp_b.get(c).copied()
        }
    }
}

#[inline]
fn parse_price(p: &[u8]) -> Option<u32> {
    if p.is_empty() || !p.iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let mut i = 0;
    while i < p.len() && p[i] == b'0' {
        i += 1;
    }
    let sig = &p[i..];
    if sig.is_empty() || sig.len() > 8 {
        return None;
    }
    let mut v: u32 = 0;
    for &c in sig {
        v = v * 10 + (c - b'0') as u32;
    }
    if v <= 10_000_000 {
        Some(v)
    } else {
        None
    }
}

fn parse_event(ctx: &Ctx, dc: &mut DateCache, line: &[u8]) -> Option<EvRec> {
    let mut f = [(0u32, 0u32); 7];
    if !csv::split::<7>(line, &mut f) {
        return None;
    }
    let g = |i: usize| &line[f[i].0 as usize..f[i].1 as usize];
    let eid = g(0);
    if eid.is_empty() {
        return None;
    }
    let code = encode(eid);
    let ing = parse_ts_c(g(3), dc);
    let ingest = match ing {
        Some(v) => (v.div_euclid(1000) - ctx.ing0).clamp(0, u32::MAX as i64) as u32,
        None => u32::MAX,
    };
    let mut r = EvRec { code, person: DEAD, ts: 0, ingest, pc: NOCAT };
    let user = g(1);
    if user.is_empty() {
        return Some(r);
    }
    let ts = match parse_ts_c(g(2), dc) {
        Some(t) if t >= ctx.ws && t < ctx.we => t,
        _ => return Some(r),
    };
    let purchase = match g(4) {
        b"view" | b"cart" => false,
        b"purchase" => true,
        _ => return Some(r),
    };
    let prod = g(5);
    if prod.is_empty() {
        return Some(r);
    }
    if purchase {
        match parse_price(g(6)) {
            Some(p) => r.pc = (p << 8) | ctx.cat(prod),
            None => return Some(r),
        }
    }
    match ing {
        Some(v) if v - ts <= LATE_MS => {}
        _ => return Some(r),
    }
    r.ts = (ts - ctx.base) as u32;
    let uc = encode(user);
    ctx.link.prefetch(uc);
    r.person = uc;
    Some(r)
}

fn parse_click(ctx: &Ctx, dc: &mut DateCache, line: &[u8]) -> Option<CRec> {
    let mut f = [(0u32, 0u32); 4];
    if !csv::split::<4>(line, &mut f) {
        return None;
    }
    let g = |i: usize| &line[f[i].0 as usize..f[i].1 as usize];
    let cid = g(0);
    if cid.is_empty() {
        return None;
    }
    let user = g(1);
    if user.is_empty() {
        return None;
    }
    let ts = match parse_ts_c(g(3), dc) {
        Some(t) if t >= ctx.ws - WEEK_MS && t < ctx.we => t,
        _ => return None,
    };
    let camp = ctx.camp(g(2))?;
    let uc = encode(user);
    ctx.link.prefetch(uc);
    Some(CRec { person: uc, tc: (((ts - ctx.base) as u64) << 32) | camp as u64, code: encode(cid) })
}

// ---------------------------------------------------------------- union find
fn find(p: &mut [u32], mut x: u32) -> u32 {
    while p[x as usize] != x {
        let gp = p[p[x as usize] as usize];
        p[x as usize] = gp;
        x = gp;
    }
    x
}

// ---------------------------------------------------------------- output accumulators
#[derive(Default)]
struct UserOut {
    ids: Vec<u8>,
    offs: Vec<u32>,
    day: Vec<i32>,
    sessions: Vec<i64>,
    events: Vec<i64>,
    revenue: Vec<i64>,
    topc: Vec<u32>,
}

fn run_parallel<F: Fn(usize) + Sync>(nthreads: usize, f: F) {
    std::thread::scope(|s| {
        for t in 0..nthreads {
            let f = &f;
            s.spawn(move || f(t));
        }
    });
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (data_dir, out_dir) = if args.len() >= 3 {
        (PathBuf::from(&args[args.len() - 2]), PathBuf::from(&args[args.len() - 1]))
    } else {
        (
            PathBuf::from(std::env::var("DATA_DIR").unwrap_or("/data".into())),
            PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or("/output".into())),
        )
    };
    let nthreads: usize = std::env::var("NCPU")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or_else(|| std::thread::available_parallelism().map(|n| n.get()).unwrap_or(8))
        .max(1);
    let timing = std::env::var("ENGINE_TIMING").is_ok();
    let t0 = std::time::Instant::now();
    let lap = |name: &str| {
        if timing {
            eprintln!("{:>8.3}s {}", t0.elapsed().as_secs_f64(), name);
        }
    };
    codec::init();

    let (ws, we) = manifest(&data_dir.join("manifest.json"));

    // products / categories
    let prods = read_small_csv::<2>(&data_dir.join("products.csv"));
    let mut cats: Vec<Vec<u8>> = prods.iter().map(|r| r[1].clone()).collect();
    cats.push(b"unknown".to_vec());
    cats.sort();
    cats.dedup();
    let cat_idx: HashMap<Vec<u8>, u32> = cats.iter().enumerate().map(|(i, c)| (c.clone(), i as u32)).collect();
    let unknown_cat = cat_idx[&b"unknown"[..]];
    let mut prod_a = Map64::with_capacity(prods.len());
    let mut prod_b = HashMap::new();
    for r in &prods {
        let ci = cat_idx[&r[1]];
        if r[0].is_empty() {
            continue;
        }
        match codec::encode_a(&r[0]) {
            Some(c) => {
                prod_a.insert(c, ci as u64);
            }
            None => {
                prod_b.entry(r[0].clone()).or_insert(ci);
            }
        }
    }
    // campaigns
    let camps = read_small_csv::<2>(&data_dir.join("campaigns.csv"));
    let mut camp_ids: Vec<Vec<u8>> = Vec::new();
    let mut channels: Vec<Vec<u8>> = Vec::new();
    let mut camp_a = Map64::with_capacity(camps.len());
    let mut camp_b = HashMap::new();
    for r in &camps {
        if r[0].is_empty() {
            continue;
        }
        let idx = camp_ids.len() as u64;
        let stored = match codec::encode_a(&r[0]) {
            Some(c) => camp_a.insert(c, idx),
            None => *camp_b.entry(r[0].clone()).or_insert(idx as u32) as u64,
        };
        if stored == idx {
            camp_ids.push(r[0].clone());
            channels.push(r[1].clone());
        }
    }
    let ncamp = camp_ids.len();
    camp_ids.push(b"organic".to_vec());
    channels.push(b"organic".to_vec());
    lap("small tables");

    let ncats = cats.len();
    // file lists and chunks
    let mut files: Vec<(PathBuf, u64)> = Vec::new();
    let mut chunks: Vec<Chunk> = Vec::new();
    let mut ev_bytes = 0u64;
    for (kind, sub) in [(0u8, "links"), (1, "events"), (2, "clicks")] {
        for (p, sz) in list_csv(&data_dir.join(sub)) {
            let fi = files.len();
            files.push((p, sz));
            if kind == 1 {
                ev_bytes += sz;
            }
            let mut s = 0;
            while s < sz {
                let e = (s + CHUNK).min(sz);
                chunks.push(Chunk { file: fi, start: s, end: e, kind });
                s = e;
            }
        }
    }
    let handles: Vec<File> = files.iter().map(|(p, _)| File::open(p).expect("open")).collect();
    let (link_chunks, rest): (Vec<_>, Vec<_>) = chunks.into_iter().partition(|c| c.kind == 0);
    let mut main_chunks = rest;
    // largest-first order is not needed for equal-size chunks; keep events then clicks interleaved
    main_chunks.sort_by_key(|c| std::cmp::Reverse(c.end - c.start));

    // ---------------- links
    let edges: Mutex<Vec<Vec<(u64, u64)>>> = Mutex::new(Vec::new());
    {
        let next = AtomicUsize::new(0);
        run_parallel(nthreads, |_| {
            let mut buf = Vec::new();
            let mut local = Vec::new();
            loop {
                let i = next.fetch_add(1, AO::Relaxed);
                if i >= link_chunks.len() {
                    break;
                }
                let c = &link_chunks[i];
                for_lines(&handles[c.file], files[c.file].1, c.start, c.end, &mut buf, |line| {
                    let mut f = [(0u32, 0u32); 2];
                    if !csv::split::<2>(line, &mut f) {
                        return;
                    }
                    let a = &line[f[0].0 as usize..f[0].1 as usize];
                    let b = &line[f[1].0 as usize..f[1].1 as usize];
                    if a.is_empty() || b.is_empty() {
                        return;
                    }
                    local.push((encode(a), encode(b)));
                });
            }
            edges.lock().unwrap().push(local);
        });
    }
    let edges = edges.into_inner().unwrap();
    let nedges: usize = edges.iter().map(|v| v.len()).sum();
    let mut idx = Map64::with_capacity(nedges * 2);
    let mut codes: Vec<u64> = Vec::new();
    let mut parent: Vec<u32> = Vec::new();
    for v in &edges {
        for &(a, b) in v {
            let ia = idx.insert(a, codes.len() as u64) as u32;
            if ia as usize == codes.len() {
                codes.push(a);
                parent.push(ia);
            }
            let ib = idx.insert(b, codes.len() as u64) as u32;
            if ib as usize == codes.len() {
                codes.push(b);
                parent.push(ib);
            }
            let ra = find(&mut parent, ia);
            let rb = find(&mut parent, ib);
            if ra != rb {
                if ra < rb {
                    parent[rb as usize] = ra;
                } else {
                    parent[ra as usize] = rb;
                }
            }
        }
    }
    drop(edges);
    let n = codes.len();
    let mut name: Vec<u64> = vec![u64::MAX; n];
    let mut has: Vec<bool> = vec![false; n];
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        if !has[r] || cmp_codes(codes[i], name[r]) == std::cmp::Ordering::Less {
            name[r] = codes[i];
            has[r] = true;
        }
    }
    let mut link = Map64::with_capacity(n);
    for i in 0..n {
        let r = find(&mut parent, i as u32) as usize;
        link.insert(codes[i], name[r]);
    }
    drop(idx);
    lap("links");

    // ---------------- parse events & clicks
    let est_rows = (ev_bytes / 70).max(1024);
    let p1_bits = ((est_rows / 40_000).max(1).next_power_of_two().trailing_zeros()).min(12);
    let q_bits = ((est_rows / 60_000).max(1).next_power_of_two().trailing_zeros()).clamp(4, 12);
    let base = ws - WEEK_MS;
    if we - base >= (1i64 << 32) || ncats >= 255 {
        eprintln!("unsupported window/category count");
        std::process::exit(3);
    }
    let ing0 = ws.div_euclid(1000) - (1i64 << 31);
    let ctx = Ctx { ws, we, link, prod_a, prod_b, unknown_cat, camp_a, camp_b, p1_bits, q_bits, base, ing0 };
    let np1 = 1usize << p1_bits;
    let nq = 1usize << q_bits;
    let part_of = |code: u64, bits: u32| -> usize {
        if bits == 0 {
            0
        } else {
            (mix(code) >> (64 - bits)) as usize
        }
    };

    struct TA {
        ev: Vec<Vec<Vec<EvRec>>>,
        ck: Vec<Vec<Vec<CRec>>>,
    }
    let tas: Vec<Mutex<TA>> = (0..nthreads)
        .map(|_| Mutex::new(TA { ev: (0..np1).map(|_| Vec::new()).collect(), ck: (0..nq).map(|_| Vec::new()).collect() }))
        .collect();
    {
        let next = AtomicUsize::new(0);
        run_parallel(nthreads, |t| {
            let mut ta = tas[t].lock().unwrap();
            let ta = &mut *ta;
            let mut buf = Vec::new();
            let mut dc = DateCache::new();
            loop {
                let i = next.fetch_add(1, AO::Relaxed);
                if i >= main_chunks.len() {
                    break;
                }
                let c = &main_chunks[i];
                if c.kind == 1 {
                    let mut ring: Vec<EvRec> = Vec::with_capacity(32);
                    let mut flush = |ring: &mut Vec<EvRec>, ta: &mut TA| {
                        for r in ring.iter_mut() {
                            if r.person != DEAD {
                                r.person = ctx.link.get(r.person).unwrap_or(r.person);
                            }
                            push_blk(&mut ta.ev[part_of(r.code, ctx.p1_bits)], *r);
                        }
                        ring.clear();
                    };
                    for_lines(&handles[c.file], files[c.file].1, c.start, c.end, &mut buf, |line| {
                        if let Some(r) = parse_event(&ctx, &mut dc, line) {
                            ring.push(r);
                            if ring.len() == 32 {
                                flush(&mut ring, ta);
                            }
                        }
                    });
                    flush(&mut ring, ta);
                } else {
                    let mut ring: Vec<CRec> = Vec::with_capacity(32);
                    let mut flush = |ring: &mut Vec<CRec>, ta: &mut TA| {
                        for r in ring.iter_mut() {
                            r.person = ctx.link.get(r.person).unwrap_or(r.person);
                            push_blk(&mut ta.ck[part_of(r.person, ctx.q_bits)], *r);
                        }
                        ring.clear();
                    };
                    for_lines(&handles[c.file], files[c.file].1, c.start, c.end, &mut buf, |line| {
                        if let Some(r) = parse_click(&ctx, &mut dc, line) {
                            ring.push(r);
                            if ring.len() == 32 {
                                flush(&mut ring, ta);
                            }
                        }
                    });
                    flush(&mut ring, ta);
                }
            }
        });
    }
    lap("parse");

    // ---------------- dedup events
    let mut tas: Vec<TA> = tas.into_iter().map(|m| m.into_inner().unwrap()).collect();
    let ev_parts: Vec<Vec<Vec<Vec<EvRec>>>> = tas.iter_mut().map(|t| std::mem::take(&mut t.ev)).collect();
    let ck_parts: Vec<Vec<Vec<Vec<CRec>>>> = tas.iter_mut().map(|t| std::mem::take(&mut t.ck)).collect();
    drop(tas);
    let sparts: Vec<Mutex<Vec<Vec<Vec<SRec>>>>> =
        (0..nthreads).map(|_| Mutex::new((0..nq).map(|_| Vec::new()).collect())).collect();
    {
        let next = AtomicUsize::new(0);
        run_parallel(nthreads, |t| {
            let mut out = sparts[t].lock().unwrap();
            let mut table: Vec<EvRec> = Vec::new();
            let empty = EvRec { code: DEAD, person: DEAD, ts: 0, ingest: 0, pc: 0 };
            loop {
                let p = next.fetch_add(1, AO::Relaxed);
                if p >= np1 {
                    break;
                }
                let n: usize = ev_parts.iter().map(|tp| tp[p].iter().map(|b| b.len()).sum::<usize>()).sum();
                let cap = (n.max(4) * 2).next_power_of_two();
                table.clear();
                table.resize(cap, empty);
                let mask = cap - 1;
                for tp in &ev_parts {
                    for blk in &tp[p] {
                        for r in blk {
                            let mut i = mix(r.code) as usize & mask;
                            loop {
                                let e = &mut table[i];
                                if e.code == r.code {
                                    if r.ingest < e.ingest {
                                        *e = *r;
                                    }
                                    break;
                                }
                                if e.code == DEAD {
                                    *e = *r;
                                    break;
                                }
                                i = (i + 1) & mask;
                            }
                        }
                    }
                }
                for r in &table {
                    if r.code != DEAD && r.person != DEAD {
                        push_blk(
                            &mut out[part_of(r.person, ctx.q_bits)],
                            SRec { person: r.person, tp: ((r.ts as u64) << 32) | r.pc as u64 },
                        );
                    }
                }
            }
        });
    }
    drop(ev_parts);
    lap("dedup");

    // ---------------- sessionize per person partition
    let sparts: Vec<Vec<Vec<Vec<SRec>>>> = sparts.into_iter().map(|m| m.into_inner().unwrap()).collect();
    let day0 = ws.div_euclid(DAY_MS);
    let day1 = (we - 1).div_euclid(DAY_MS);
    let ndays = (day1 - day0 + 1).max(1) as usize;
    let ncol = ncamp + 1;
    struct TC {
        uo: UserOut,
        camp: Vec<[i64; 4]>, // [ncol * ndays]: sessions, purchasing, revenue, assisted
    }
    let tcs: Vec<Mutex<TC>> = (0..nthreads)
        .map(|_| Mutex::new(TC { uo: UserOut { offs: vec![0], ..Default::default() }, camp: vec![[0; 4]; ncol * ndays] }))
        .collect();
    {
        let next = AtomicUsize::new(0);
        run_parallel(nthreads, |t| {
            let mut tc = tcs[t].lock().unwrap();
            let tc = &mut *tc;
            let mut ev: Vec<SRec> = Vec::new();
            let mut ck: Vec<CRec> = Vec::new();
            let mut catsum = vec![0i64; ncats];
            let mut cnt = vec![0u32; ncol];
            let mut active: Vec<u32> = Vec::new();
            let mut apos = vec![u32::MAX; ncol];
            loop {
                let q = next.fetch_add(1, AO::Relaxed);
                if q >= nq {
                    break;
                }
                ev.clear();
                ck.clear();
                for tp in &sparts {
                    for blk in &tp[q] {
                        ev.extend_from_slice(blk);
                    }
                }
                for tp in &ck_parts {
                    for blk in &tp[q] {
                        ck.extend_from_slice(blk);
                    }
                }
                ev.sort_unstable();
                ck.sort_unstable_by(|a, b| {
                    a.person
                        .cmp(&b.person)
                        .then((a.tc >> 32).cmp(&(b.tc >> 32)))
                        .then_with(|| cmp_codes(a.code, b.code))
                });
                ck.dedup_by(|b, a| a.code == b.code && a.person == b.person && (a.tc >> 32) == (b.tc >> 32));
                // walk persons
                let mut ci = 0usize; // click cursor
                let mut i = 0usize;
                while i < ev.len() {
                    let person = ev[i].person;
                    let mut pe = i;
                    while pe < ev.len() && ev[pe].person == person {
                        pe += 1;
                    }
                    while ci < ck.len() && ck[ci].person < person {
                        ci += 1;
                    }
                    let cs = ci;
                    while ci < ck.len() && ck[ci].person == person {
                        ci += 1;
                    }
                    let clicks = &ck[cs..ci];
                    let (mut lo, mut hi) = (0usize, 0usize);
                    // person-day accumulator
                    let mut cur_day: i64 = i64::MIN;
                    let mut d_sess = 0i64;
                    let mut d_ev = 0i64;
                    let mut d_rev = 0i64;
                    let mut touched: Vec<u32> = Vec::new();
                    let flush = |uo: &mut UserOut, day: i64, s: i64, e: i64, rv: i64, catsum: &mut Vec<i64>, touched: &mut Vec<u32>| {
                        decode_into(person, &mut uo.ids);
                        uo.offs.push(uo.ids.len() as u32);
                        uo.day.push(day as i32);
                        uo.sessions.push(s);
                        uo.events.push(e);
                        uo.revenue.push(rv);
                        let mut best = NONE_CAT;
                        let mut bestv = -1i64;
                        for &c in touched.iter() {
                            let v = catsum[c as usize];
                            if v > bestv || (v == bestv && c < best) {
                                bestv = v;
                                best = c;
                            }
                            catsum[c as usize] = 0;
                        }
                        touched.clear();
                        uo.topc.push(best);
                    };
                    let mut s = i;
                    while s < pe {
                        let start = (ev[s].tp >> 32) as i64 + base;
                        let mut e = s + 1;
                        while e < pe && (ev[e].tp >> 32) as i64 - (ev[e - 1].tp >> 32) as i64 <= GAP_MS {
                            e += 1;
                        }
                        let day = start.div_euclid(DAY_MS);
                        if day != cur_day {
                            if cur_day != i64::MIN {
                                flush(&mut tc.uo, cur_day, d_sess, d_ev, d_rev, &mut catsum, &mut touched);
                            }
                            cur_day = day;
                            d_sess = 0;
                            d_ev = 0;
                            d_rev = 0;
                        }
                        let mut rev = 0i64;
                        let mut purch = false;
                        for r in &ev[s..e] {
                            let pc = r.tp as u32;
                            let cat = pc & 0xFF;
                            if cat != NOCAT {
                                let price = (pc >> 8) as i64;
                                purch = true;
                                rev += price;
                                if catsum[cat as usize] == 0 {
                                    touched.push(cat);
                                }
                                catsum[cat as usize] += price;
                            }
                        }
                        // attribution: window [start - 7d, start]
                        while hi < clicks.len() && (clicks[hi].tc >> 32) as i64 + base <= start {
                            let c = clicks[hi].tc as u32 as usize;
                            if cnt[c] == 0 {
                                apos[c] = active.len() as u32;
                                active.push(c as u32);
                            }
                            cnt[c] += 1;
                            hi += 1;
                        }
                        while lo < hi && ((clicks[lo].tc >> 32) as i64 + base) < start - WEEK_MS {
                            let c = clicks[lo].tc as u32 as usize;
                            cnt[c] -= 1;
                            if cnt[c] == 0 {
                                let p = apos[c] as usize;
                                let last = *active.last().unwrap();
                                active[p] = last;
                                apos[last as usize] = p as u32;
                                active.pop();
                            }
                            lo += 1;
                        }
                        let credited = if hi > lo {
                            let lt = clicks[hi - 1].tc >> 32;
                            let mut k = hi - 1;
                            while k > lo && (clicks[k - 1].tc >> 32) == lt {
                                k -= 1;
                            }
                            clicks[k].tc as u32 as usize
                        } else {
                            ncamp
                        };
                        let di = (day - day0) as usize;
                        let slot = &mut tc.camp[credited * ndays + di];
                        slot[0] += 1;
                        slot[1] += purch as i64;
                        slot[2] += rev;
                        for &c in &active {
                            tc.camp[c as usize * ndays + di][3] += 1;
                        }
                        d_sess += 1;
                        d_ev += (e - s) as i64;
                        d_rev += rev;
                        s = e;
                    }
                    // note: catsum for the current day already includes this day's purchases;
                    // flush handles the last day.
                    flush(&mut tc.uo, cur_day, d_sess, d_ev, d_rev, &mut catsum, &mut touched);
                    // reset sliding window
                    for &c in &active {
                        cnt[c as usize] = 0;
                    }
                    active.clear();
                    i = pe;
                }
            }
        });
    }
    lap("sessionize");

    // ---------------- outputs
    let tcs: Vec<TC> = tcs.into_iter().map(|m| m.into_inner().unwrap()).collect();
    let ud = out_dir.join("user_daily");
    let cd = out_dir.join("campaign_daily");
    std::fs::create_dir_all(&ud).expect("mkdir");
    std::fs::create_dir_all(&cd).expect("mkdir");
    let mut camp_tot = vec![[0i64; 4]; ncol * ndays];
    for tc in &tcs {
        for (a, b) in camp_tot.iter_mut().zip(&tc.camp) {
            for k in 0..4 {
                a[k] += b[k];
            }
        }
    }
    // category name bytes table
    let mut cat_data = Vec::new();
    let mut cat_offs = vec![0u32];
    for c in &cats {
        cat_data.extend_from_slice(c);
        cat_offs.push(cat_data.len() as u32);
    }
    std::thread::scope(|s| {
        for (t, tc) in tcs.iter().enumerate() {
            let ud = &ud;
            let cats = &cats;
            s.spawn(move || {
                let uo = &tc.uo;
                let mut tdata = Vec::new();
                let mut toffs = vec![0u32];
                let mut valid = Vec::with_capacity(uo.topc.len());
                for &c in &uo.topc {
                    if c != NONE_CAT {
                        tdata.extend_from_slice(&cats[c as usize]);
                        valid.push(true);
                    } else {
                        valid.push(false);
                    }
                    toffs.push(tdata.len() as u32);
                }
                let days: Vec<i32> = uo.day.clone();
                parquet::write_file(
                    &ud.join(format!("part-{t:03}.parquet")),
                    &["user_id", "day", "sessions", "events", "revenue_cents", "top_category"],
                    &[
                        parquet::Col::Str { data: &uo.ids, offs: &uo.offs },
                        parquet::Col::Date(&days),
                        parquet::Col::I64(&uo.sessions),
                        parquet::Col::I64(&uo.events),
                        parquet::Col::I64(&uo.revenue),
                        parquet::Col::OptStr { data: &tdata, offs: &toffs, valid: &valid },
                    ],
                )
                .expect("write user_daily");
            });
        }
        // campaign_daily
        let mut cid = Vec::new();
        let mut coffs = vec![0u32];
        let mut chn = Vec::new();
        let mut choffs = vec![0u32];
        let mut day = Vec::new();
        let mut cols: [Vec<i64>; 4] = Default::default();
        for c in 0..ncol {
            for d in 0..ndays {
                let v = camp_tot[c * ndays + d];
                if v[0] == 0 && v[3] == 0 {
                    continue;
                }
                cid.extend_from_slice(&camp_ids[c]);
                coffs.push(cid.len() as u32);
                chn.extend_from_slice(&channels[c]);
                choffs.push(chn.len() as u32);
                day.push((day0 + d as i64) as i32);
                for k in 0..4 {
                    cols[k].push(v[k]);
                }
            }
        }
        parquet::write_file(
            &cd.join("part-000.parquet"),
            &["campaign_id", "channel", "day", "sessions", "purchasing_sessions", "revenue_cents", "assisted_sessions"],
            &[
                parquet::Col::Str { data: &cid, offs: &coffs },
                parquet::Col::Str { data: &chn, offs: &choffs },
                parquet::Col::Date(&day),
                parquet::Col::I64(&cols[0]),
                parquet::Col::I64(&cols[1]),
                parquet::Col::I64(&cols[2]),
                parquet::Col::I64(&cols[3]),
            ],
        )
        .expect("write campaign_daily");
    });
    lap("write");
    let _ = (ncats, cat_data, cat_offs);
    // Skip destructors.
    std::process::exit(0);
}
