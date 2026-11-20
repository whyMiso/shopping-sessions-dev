//! Oracle (reference implementation) for the "Shopping Sessions" rules.
//! Written from rules.md alone. Plain, single-threaded, byte-oriented.

use std::collections::hash_map::Entry;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use arrow::array::{ArrayRef, Date32Array, Int64Array, StringArray};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use rustc_hash::{FxHashMap, FxHashSet};

const MS_PER_DAY: i64 = 86_400_000;
const SESSION_GAP_MS: i64 = 1_800_000;
const SEVEN_DAYS_MS: i64 = 7 * MS_PER_DAY;
const LATE_MS: i64 = MS_PER_DAY;
const NONE: u32 = u32::MAX;

// ---------------------------------------------------------------------------
// CSV dialect
// ---------------------------------------------------------------------------

/// Split one line (without its terminating '\n') into exactly `N` fields.
/// Returns None if the line is malformed. Quotes are allowed only wrapping a
/// whole field; the wrapped content must hold no quote. Empty line: malformed.
fn parse_line<const N: usize>(line: &[u8]) -> Option<[&[u8]; N]> {
    if line.is_empty() {
        return None;
    }
    let mut out: [&[u8]; N] = [&[]; N];
    let mut n = 0;
    for raw in line.split(|&b| b == b',') {
        if n == N {
            return None; // too many fields
        }
        let field: &[u8] = if raw.first() == Some(&b'"') {
            if raw.len() < 2 || *raw.last().unwrap() != b'"' {
                return None;
            }
            let inner = &raw[1..raw.len() - 1];
            if inner.contains(&b'"') {
                return None;
            }
            inner
        } else {
            if raw.contains(&b'"') {
                return None;
            }
            raw
        };
        out[n] = field;
        n += 1;
    }
    if n == N {
        Some(out)
    } else {
        None
    }
}

/// Calls `f` with every line of every file (header = first line of each
/// file, always skipped). Lines are split on '\n' only.
fn for_each_line(files: &[PathBuf], mut f: impl FnMut(&[u8])) {
    let mut buf: Vec<u8> = Vec::with_capacity(1024);
    for path in files {
        let file = File::open(path).unwrap_or_else(|e| panic!("open {}: {}", path.display(), e));
        let mut rd = BufReader::with_capacity(1 << 20, file);
        let mut first = true;
        loop {
            buf.clear();
            let got = rd.read_until(b'\n', &mut buf).expect("read");
            if got == 0 {
                break;
            }
            let line: &[u8] = if buf.last() == Some(&b'\n') { &buf[..buf.len() - 1] } else { &buf[..] };
            if first {
                first = false;
                continue;
            }
            f(line);
        }
    }
}

fn csv_files(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            if p.is_file() && p.extension().map(|x| x == "csv").unwrap_or(false) {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

// ---------------------------------------------------------------------------
// Timestamps
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Ts {
    Bad,
    /// A valid F3 number too large for i64: valid but outside every window.
    Far,
    Ms(i64),
}

fn is_digit(b: u8) -> bool {
    b.is_ascii_digit()
}

fn num(s: &[u8]) -> Option<i64> {
    let mut v: i64 = 0;
    if s.is_empty() {
        return None;
    }
    for &b in s {
        if !is_digit(b) {
            return None;
        }
        v = v * 10 + (b - b'0') as i64;
    }
    Some(v)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

fn days_in_month(y: i64, m: i64) -> i64 {
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

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn parse_ts(s: &[u8]) -> Ts {
    if s.is_empty() {
        return Ts::Bad;
    }
    // F3: digits only.
    if s.iter().all(|&b| is_digit(b)) {
        let mut i = 0;
        while i < s.len() && s[i] == b'0' {
            i += 1;
        }
        let rest = &s[i..];
        let mut v: i64 = 0;
        for &b in rest {
            match v.checked_mul(10).and_then(|x| x.checked_add((b - b'0') as i64)) {
                Some(x) => v = x,
                None => return Ts::Far,
            }
        }
        return Ts::Ms(v);
    }
    // F1: YYYY-MM-DDTHH:MM:SSZ (20 bytes); F2: YYYY-MM-DDTHH:MM:SS+HH:MM (25 bytes)
    if s.len() != 20 && s.len() != 25 {
        return Ts::Bad;
    }
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return Ts::Bad;
    }
    let (y, mo, d, h, mi, se) = match (
        num(&s[0..4]),
        num(&s[5..7]),
        num(&s[8..10]),
        num(&s[11..13]),
        num(&s[14..16]),
        num(&s[17..19]),
    ) {
        (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f)) => (a, b, c, d, e, f),
        _ => return Ts::Bad,
    };
    if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || se > 59 {
        return Ts::Bad;
    }
    let offset_ms: i64 = if s.len() == 20 {
        if s[19] != b'Z' {
            return Ts::Bad;
        }
        0
    } else {
        let sign = match s[19] {
            b'+' => 1,
            b'-' => -1,
            _ => return Ts::Bad,
        };
        if s[22] != b':' {
            return Ts::Bad;
        }
        let (oh, om) = match (num(&s[20..22]), num(&s[23..25])) {
            (Some(a), Some(b)) => (a, b),
            _ => return Ts::Bad,
        };
        if oh > 23 || om > 59 {
            return Ts::Bad;
        }
        sign * (oh * 3600 + om * 60) * 1000
    };
    let days = days_from_civil(y, mo, d);
    let ms = ((days * 24 + h) * 60 + mi) * 60_000 + se * 1000;
    Ts::Ms(ms - offset_ms)
}

fn in_window(t: Ts, lo: i64, hi: i64) -> Option<i64> {
    match t {
        Ts::Ms(v) if lo <= v && v < hi => Some(v),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Interning
// ---------------------------------------------------------------------------

#[derive(Default)]
struct Interner {
    map: FxHashMap<Box<[u8]>, u32>,
    names: Vec<Box<[u8]>>,
}

impl Interner {
    fn get_or_insert(&mut self, s: &[u8]) -> u32 {
        if let Some(&id) = self.map.get(s) {
            return id;
        }
        let id = self.names.len() as u32;
        let b: Box<[u8]> = s.into();
        self.names.push(b.clone());
        self.map.insert(b, id);
        id
    }
}

// ---------------------------------------------------------------------------
// Union-find
// ---------------------------------------------------------------------------

struct Dsu {
    parent: Vec<u32>,
}

impl Dsu {
    fn grow(&mut self, n: usize) {
        while self.parent.len() < n {
            let i = self.parent.len() as u32;
            self.parent.push(i);
        }
    }
    fn find(&mut self, mut x: u32) -> u32 {
        let mut r = x;
        while self.parent[r as usize] != r {
            r = self.parent[r as usize];
        }
        while self.parent[x as usize] != r {
            let next = self.parent[x as usize];
            self.parent[x as usize] = r;
            x = next;
        }
        r
    }
    fn union(&mut self, a: u32, b: u32) {
        let ra = self.find(a);
        let rb = self.find(b);
        if ra != rb {
            self.parent[ra as usize] = rb;
        }
    }
}

// ---------------------------------------------------------------------------
// Event record (one per event_id: the copy with the smallest ingest_ts)
// ---------------------------------------------------------------------------

const T_VIEW: u8 = 1;
const T_CART: u8 = 2;
const T_PURCHASE: u8 = 3;

#[derive(Clone, Copy)]
struct EventCopy {
    ingest: i64,
    /// Parsed ts if valid and inside the event window, else None.
    ts: Option<i64>,
    user: u32,  // NONE if empty
    cat: u32,   // NONE if product empty; category id otherwise
    price: i64, // valid purchase price (1..=10M) or -1
    etype: u8,  // 0 if not exactly view/cart/purchase
}

fn parse_price(s: &[u8]) -> i64 {
    if s.is_empty() || !s.iter().all(|&b| is_digit(b)) {
        return -1;
    }
    let mut i = 0;
    while i < s.len() && s[i] == b'0' {
        i += 1;
    }
    let rest = &s[i..];
    if rest.len() > 8 {
        return -1;
    }
    let v = if rest.is_empty() { 0 } else { num(rest).unwrap() };
    if (1..=10_000_000).contains(&v) {
        v
    } else {
        -1
    }
}

fn read_manifest(data: &Path) -> (i64, i64) {
    let text = std::fs::read(data.join("manifest.json")).expect("manifest.json");
    let v: serde_json::Value = serde_json::from_slice(&text).expect("manifest json");
    let get = |k: &str| -> i64 {
        let s = v[k].as_str().unwrap_or_else(|| panic!("manifest {}", k));
        match parse_ts(s.as_bytes()) {
            Ts::Ms(x) => x,
            _ => panic!("manifest {} not a timestamp: {}", k, s),
        }
    };
    (get("window_start"), get("window_end"))
}

struct UserDay {
    sessions: i64,
    events: i64,
    revenue: i64,
    cat_rev: FxHashMap<u32, i64>,
}

#[derive(Default)]
struct CampDay {
    sessions: i64,
    purchasing: i64,
    revenue: i64,
    assisted: i64,
}

struct Click {
    ts: i64,
    id: Box<[u8]>,
    campaign: u32,
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (data_dir, out_dir) = if args.len() >= 3 {
        (PathBuf::from(&args[args.len() - 2]), PathBuf::from(&args[args.len() - 1]))
    } else {
        (
            PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into())),
            PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into())),
        )
    };

    let (win_start, win_end) = read_manifest(&data_dir);
    let click_lo = win_start - SEVEN_DAYS_MS;


    // ---- products / categories ----
    let mut cats = Interner::default();
    let mut product_cat: FxHashMap<Box<[u8]>, u32> = FxHashMap::default();
    for_each_line(&[data_dir.join("products.csv")], |line| {
        if let Some(fields) = parse_line::<2>(line) {
            let c = cats.get_or_insert(fields[1]);
            product_cat.entry(fields[0].into()).or_insert(c);
        }
    });
    let unknown_cat = cats.get_or_insert(b"unknown");

    // ---- campaigns ----
    let mut camp_ids: Vec<Box<[u8]>> = Vec::new();
    let mut camp_channel: Vec<Box<[u8]>> = Vec::new();
    let mut camp_index: FxHashMap<Box<[u8]>, u32> = FxHashMap::default();
    for_each_line(&[data_dir.join("campaigns.csv")], |line| {
        if let Some(fields) = parse_line::<2>(line) {
            if let Entry::Vacant(e) = camp_index.entry(fields[0].into()) {
                e.insert(camp_ids.len() as u32);
                camp_ids.push(fields[0].into());
                camp_channel.push(fields[1].into());
            }
        }
    });
    let organic = camp_ids.len() as u32; // pseudo-campaign index
    camp_ids.push(b"organic"[..].into());
    camp_channel.push(b"organic"[..].into());

    // ---- links ----
    let mut users = Interner::default();
    let mut dsu = Dsu { parent: Vec::new() };
    for_each_line(&csv_files(&data_dir.join("links")), |line| {
        let fields = match parse_line::<2>(line) {
            Some(f) => f,
            None => return,
        };
        if fields[0].is_empty() || fields[1].is_empty() {
            return;
        }
        let a = users.get_or_insert(fields[0]);
        let b = users.get_or_insert(fields[1]);
        dsu.grow(users.names.len());
        dsu.union(a, b);
    });

    // ---- events: parse + dedup ----
    let mut best: FxHashMap<Box<[u8]>, EventCopy> = FxHashMap::default();
    for_each_line(&csv_files(&data_dir.join("events")), |line| {
        let fields = match parse_line::<7>(line) {
            Some(f) => f,
            None => return,
        };
        let (eid, uid, ts, ing, et, pid, price) =
            (fields[0], fields[1], fields[2], fields[3], fields[4], fields[5], fields[6]);
        if eid.is_empty() {
            return;
        }
        // ingest_ts is promised to be valid F1; if it is not, the line is
        // treated as malformed (see NOTES.md).
        let ingest = match parse_ts(ing) {
            Ts::Ms(v) => v,
            _ => return,
        };
        if let Some(cur) = best.get(eid) {
            if cur.ingest <= ingest {
                return; // existing copy wins (equal ingest_ts copies are identical)
            }
        }
        let etype = match et {
            b"view" => T_VIEW,
            b"cart" => T_CART,
            b"purchase" => T_PURCHASE,
            _ => 0,
        };
        let copy = EventCopy {
            ingest,
            ts: in_window(parse_ts(ts), win_start, win_end),
            user: if uid.is_empty() { NONE } else { users.get_or_insert(uid) },
            cat: if pid.is_empty() { NONE } else { *product_cat.get(pid).unwrap_or(&unknown_cat) },
            price: if etype == T_PURCHASE { parse_price(price) } else { 0 },
            etype,
        };
        match best.get_mut(eid) {
            Some(cur) => *cur = copy,
            None => {
                best.insert(eid.into(), copy);
            }
        }
    });

    // ---- clicks: parse + dedup + validate ----
    let mut seen_clicks: FxHashSet<Box<[u8]>> = FxHashSet::default();
    let mut raw_clicks: Vec<(u32, Click)> = Vec::new();
    for_each_line(&csv_files(&data_dir.join("clicks")), |line| {
        let fields = match parse_line::<4>(line) {
            Some(f) => f,
            None => return,
        };
        let (cid, uid, camp, ts) = (fields[0], fields[1], fields[2], fields[3]);
        if cid.is_empty() {
            return;
        }
        if seen_clicks.contains(cid) {
            return; // dedup: keep the first parsed copy
        }
        seen_clicks.insert(cid.into());
        if uid.is_empty() {
            return;
        }
        let t = match in_window(parse_ts(ts), click_lo, win_end) {
            Some(t) => t,
            None => return,
        };
        let c = match camp_index.get(camp) {
            Some(&c) => c,
            None => return,
        };
        let u = users.get_or_insert(uid);
        raw_clicks.push((u, Click { ts: t, id: cid.into(), campaign: c }));
    });
    drop(seen_clicks);

    // ---- persons ----
    let n_users = users.names.len();
    dsu.grow(n_users);
    let mut root_of: Vec<u32> = (0..n_users as u32).map(|u| dsu.find(u)).collect();
    // name of each root: byte-wise smallest member
    let mut name_of_root: Vec<u32> = (0..n_users as u32).collect();
    for u in 0..n_users {
        let r = root_of[u] as usize;
        if users.names[u] < users.names[name_of_root[r] as usize] {
            name_of_root[r] = u as u32;
        }
    }
    // person id = interned id of the person's name
    for u in 0..n_users {
        root_of[u] = name_of_root[root_of[u] as usize];
    }
    let person = root_of;

    // ---- events: validate kept copy, late filter ----
    struct Ev {
        person: u32,
        ts: i64,
        cat: u32,
        price: i64, // >0 for purchases, 0 otherwise
        purchase: bool,
    }
    let mut evs: Vec<Ev> = Vec::with_capacity(best.len());
    for (_, c) in best.drain() {
        if c.user == NONE || c.etype == 0 || c.cat == NONE {
            continue;
        }
        let ts = match c.ts {
            Some(t) => t,
            None => continue,
        };
        if c.etype == T_PURCHASE && c.price < 1 {
            continue;
        }
        if c.ingest - ts > LATE_MS {
            continue;
        }
        let purchase = c.etype == T_PURCHASE;
        evs.push(Ev {
            person: person[c.user as usize],
            ts,
            cat: c.cat,
            price: if purchase { c.price } else { 0 },
            purchase,
        });
    }
    drop(best);
    evs.sort_unstable_by_key(|e| (e.person, e.ts));

    // ---- clicks by person, sorted by (ts, click_id) ----
    let mut clicks_by_person: FxHashMap<u32, Vec<Click>> = FxHashMap::default();
    for (u, c) in raw_clicks.drain(..) {
        clicks_by_person.entry(person[u as usize]).or_default().push(c);
    }
    for v in clicks_by_person.values_mut() {
        v.sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.id.cmp(&b.id)));
    }
    let empty: Vec<Click> = Vec::new();

    // ---- sessionize + attribute + aggregate ----
    let mut user_daily: FxHashMap<(u32, i32), UserDay> = FxHashMap::default();
    let mut camp_daily: FxHashMap<(u32, i32), CampDay> = FxHashMap::default();
    let mut assist_stamp: Vec<u64> = vec![0; camp_ids.len()];
    let mut session_no: u64 = 0;

    let mut i = 0;
    while i < evs.len() {
        // one session: evs[i..j]
        let p = evs[i].person;
        let start = evs[i].ts;
        let mut j = i + 1;
        while j < evs.len() && evs[j].person == p && evs[j].ts - evs[j - 1].ts <= SESSION_GAP_MS {
            j += 1;
        }
        let day = start.div_euclid(MS_PER_DAY) as i32;
        let mut n_events = 0i64;
        let mut revenue = 0i64;
        let mut has_purchase = false;
        let ud = user_daily.entry((p, day)).or_insert_with(|| UserDay {
            sessions: 0,
            events: 0,
            revenue: 0,
            cat_rev: FxHashMap::default(),
        });
        for e in &evs[i..j] {
            n_events += 1;
            if e.purchase {
                has_purchase = true;
                revenue += e.price;
                *ud.cat_rev.entry(e.cat).or_insert(0) += e.price;
            }
        }
        ud.sessions += 1;
        ud.events += n_events;
        ud.revenue += revenue;

        // attribution
        let clicks = clicks_by_person.get(&p).unwrap_or(&empty);
        let hi = clicks.partition_point(|c| c.ts <= start);
        let lo = clicks.partition_point(|c| c.ts < start - SEVEN_DAYS_MS);
        let credited = if lo < hi {
            let tmax = clicks[hi - 1].ts;
            let k = clicks.partition_point(|c| c.ts < tmax);
            clicks[k].campaign
        } else {
            organic
        };
        let cd = camp_daily.entry((credited, day)).or_default();
        cd.sessions += 1;
        if has_purchase {
            cd.purchasing += 1;
        }
        cd.revenue += revenue;

        session_no += 1;
        for c in &clicks[lo..hi] {
            if assist_stamp[c.campaign as usize] != session_no {
                assist_stamp[c.campaign as usize] = session_no;
                camp_daily.entry((c.campaign, day)).or_default().assisted += 1;
            }
        }

        i = j;
    }

    // ---- write outputs ----
    let ud_dir = out_dir.join("user_daily");
    let cd_dir = out_dir.join("campaign_daily");
    std::fs::create_dir_all(&ud_dir).expect("mkdir user_daily");
    std::fs::create_dir_all(&cd_dir).expect("mkdir campaign_daily");

    let lossy = |b: &[u8]| -> String { String::from_utf8_lossy(b).into_owned() };

    {
        let mut rows: Vec<(&(u32, i32), &UserDay)> = user_daily.iter().collect();
        rows.sort_by(|a, b| users.names[a.0 .0 as usize].cmp(&users.names[b.0 .0 as usize]).then(a.0 .1.cmp(&b.0 .1)));
        let mut c_user = Vec::new();
        let mut c_day = Vec::new();
        let mut c_sess = Vec::new();
        let mut c_ev = Vec::new();
        let mut c_rev = Vec::new();
        let mut c_top: Vec<Option<String>> = Vec::new();
        for ((p, day), ud) in rows {
            c_user.push(lossy(&users.names[*p as usize]));
            c_day.push(*day);
            c_sess.push(ud.sessions);
            c_ev.push(ud.events);
            c_rev.push(ud.revenue);
            let mut top: Option<(i64, &[u8])> = None;
            for (&cat, &sum) in &ud.cat_rev {
                let name: &[u8] = &cats.names[cat as usize];
                top = match top {
                    None => Some((sum, name)),
                    Some((s, n)) => {
                        if sum > s || (sum == s && name < n) {
                            Some((sum, name))
                        } else {
                            Some((s, n))
                        }
                    }
                };
            }
            c_top.push(top.map(|(_, n)| lossy(n)));
        }
        let schema = Arc::new(Schema::new(vec![
            Field::new("user_id", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
            Field::new("sessions", DataType::Int64, false),
            Field::new("events", DataType::Int64, false),
            Field::new("revenue_cents", DataType::Int64, false),
            Field::new("top_category", DataType::Utf8, true),
        ]));
        let cols: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(c_user)),
            Arc::new(Date32Array::from(c_day)),
            Arc::new(Int64Array::from(c_sess)),
            Arc::new(Int64Array::from(c_ev)),
            Arc::new(Int64Array::from(c_rev)),
            Arc::new(StringArray::from(c_top)),
        ];
        write_parquet(&ud_dir.join("part-0.parquet"), schema, cols);
    }
    {
        let mut rows: Vec<(&(u32, i32), &CampDay)> = camp_daily.iter().collect();
        rows.sort_by(|a, b| camp_ids[a.0 .0 as usize].cmp(&camp_ids[b.0 .0 as usize]).then(a.0 .1.cmp(&b.0 .1)));
        let mut c_camp = Vec::new();
        let mut c_chan = Vec::new();
        let mut c_day = Vec::new();
        let mut c_sess = Vec::new();
        let mut c_purch = Vec::new();
        let mut c_rev = Vec::new();
        let mut c_ass = Vec::new();
        for ((c, day), cd) in rows {
            c_camp.push(lossy(&camp_ids[*c as usize]));
            c_chan.push(lossy(&camp_channel[*c as usize]));
            c_day.push(*day);
            c_sess.push(cd.sessions);
            c_purch.push(cd.purchasing);
            c_rev.push(cd.revenue);
            c_ass.push(if *c == organic { 0 } else { cd.assisted });
        }
        let schema = Arc::new(Schema::new(vec![
            Field::new("campaign_id", DataType::Utf8, false),
            Field::new("channel", DataType::Utf8, false),
            Field::new("day", DataType::Date32, false),
            Field::new("sessions", DataType::Int64, false),
            Field::new("purchasing_sessions", DataType::Int64, false),
            Field::new("revenue_cents", DataType::Int64, false),
            Field::new("assisted_sessions", DataType::Int64, false),
        ]));
        let cols: Vec<ArrayRef> = vec![
            Arc::new(StringArray::from(c_camp)),
            Arc::new(StringArray::from(c_chan)),
            Arc::new(Date32Array::from(c_day)),
            Arc::new(Int64Array::from(c_sess)),
            Arc::new(Int64Array::from(c_purch)),
            Arc::new(Int64Array::from(c_rev)),
            Arc::new(Int64Array::from(c_ass)),
        ];
        write_parquet(&cd_dir.join("part-0.parquet"), schema, cols);
    }
}

fn write_parquet(path: &Path, schema: Arc<Schema>, cols: Vec<ArrayRef>) {
    let batch = RecordBatch::try_new(schema.clone(), cols).expect("record batch");
    let file = File::create(path).expect("create parquet");
    let mut w = ArrowWriter::try_new(file, schema, None).expect("parquet writer");
    w.write(&batch).expect("write parquet");
    w.close().expect("close parquet");
}
