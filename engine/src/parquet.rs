//! Minimal Parquet writer: one row group, PLAIN, uncompressed, data page v1.

use std::io::Write;

pub enum Col<'a> {
    /// Required UTF8 strings: concatenated bytes + offsets (len n+1).
    Str { data: &'a [u8], offs: &'a [u32] },
    /// Optional UTF8 strings: offsets (len n+1) plus validity.
    OptStr { data: &'a [u8], offs: &'a [u32], valid: &'a [bool] },
    Date(&'a [i32]),
    I64(&'a [i64]),
}

impl Col<'_> {
    fn len(&self) -> usize {
        match self {
            Col::Str { offs, .. } => offs.len() - 1,
            Col::OptStr { offs, .. } => offs.len() - 1,
            Col::Date(v) => v.len(),
            Col::I64(v) => v.len(),
        }
    }
    fn ptype(&self) -> i32 {
        match self {
            Col::Str { .. } | Col::OptStr { .. } => 6,
            Col::Date(_) => 1,
            Col::I64(_) => 2,
        }
    }
}

struct Thrift {
    buf: Vec<u8>,
    last: Vec<i16>,
}

impl Thrift {
    fn new() -> Self {
        Thrift { buf: Vec::new(), last: vec![0] }
    }
    fn varint(&mut self, mut v: u64) {
        while v >= 0x80 {
            self.buf.push((v as u8) | 0x80);
            v >>= 7;
        }
        self.buf.push(v as u8);
    }
    fn zz(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn fh(&mut self, id: i16, t: u8) {
        let last = *self.last.last().unwrap();
        let d = id - last;
        if d > 0 && d <= 15 {
            self.buf.push(((d as u8) << 4) | t);
        } else {
            self.buf.push(t);
            self.zz(id as i64);
        }
        *self.last.last_mut().unwrap() = id;
    }
    fn i32(&mut self, id: i16, v: i32) {
        self.fh(id, 5);
        self.zz(v as i64);
    }
    fn i64(&mut self, id: i16, v: i64) {
        self.fh(id, 6);
        self.zz(v);
    }
    fn bin(&mut self, id: i16, v: &[u8]) {
        self.fh(id, 8);
        self.varint(v.len() as u64);
        self.buf.extend_from_slice(v);
    }
    fn begin(&mut self, id: i16) {
        self.fh(id, 12);
        self.last.push(0);
    }
    fn begin_elem(&mut self) {
        self.last.push(0);
    }
    fn end(&mut self) {
        self.buf.push(0);
        self.last.pop();
    }
    fn list(&mut self, id: i16, et: u8, n: usize) {
        self.fh(id, 9);
        if n < 15 {
            self.buf.push(((n as u8) << 4) | et);
        } else {
            self.buf.push(0xF0 | et);
            self.varint(n as u64);
        }
    }
}

fn rle_bits1(valid: &[bool], out: &mut Vec<u8>) {
    // RLE runs, bit width 1.
    let mut body = Vec::new();
    let mut i = 0;
    while i < valid.len() {
        let v = valid[i];
        let mut j = i;
        while j < valid.len() && valid[j] == v {
            j += 1;
        }
        let mut run = (j - i) as u64;
        // run length header: (run << 1)
        let mut h = run << 1;
        while h >= 0x80 {
            body.push((h as u8) | 0x80);
            h >>= 7;
        }
        body.push(h as u8);
        body.push(v as u8);
        run = 0;
        let _ = run;
        i = j;
    }
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
}

const PAGE_ROWS: usize = 1 << 18;

pub fn write_file(path: &std::path::Path, names: &[&str], cols: &[Col]) -> std::io::Result<()> {
    let nrows = if cols.is_empty() { 0 } else { cols[0].len() };
    let mut out: Vec<u8> = Vec::new();
    out.extend_from_slice(b"PAR1");
    struct Meta {
        off: i64,
        size: i64,
        nvals: i64,
    }
    let mut metas = Vec::new();
    let mut page = Vec::new();
    for col in cols {
        let off = out.len() as i64;
        let mut r = 0;
        loop {
            let hi = (r + PAGE_ROWS).min(nrows);
            page.clear();
            match col {
                Col::Str { data, offs } => {
                    for i in r..hi {
                        let s = &data[offs[i] as usize..offs[i + 1] as usize];
                        page.extend_from_slice(&(s.len() as u32).to_le_bytes());
                        page.extend_from_slice(s);
                    }
                }
                Col::OptStr { data, offs, valid } => {
                    rle_bits1(&valid[r..hi], &mut page);
                    for i in r..hi {
                        if valid[i] {
                            let s = &data[offs[i] as usize..offs[i + 1] as usize];
                            page.extend_from_slice(&(s.len() as u32).to_le_bytes());
                            page.extend_from_slice(s);
                        }
                    }
                }
                Col::Date(v) => {
                    for x in &v[r..hi] {
                        page.extend_from_slice(&x.to_le_bytes());
                    }
                }
                Col::I64(v) => {
                    for x in &v[r..hi] {
                        page.extend_from_slice(&x.to_le_bytes());
                    }
                }
            }
            let mut t = Thrift::new();
            t.i32(1, 0);
            t.i32(2, page.len() as i32);
            t.i32(3, page.len() as i32);
            t.begin(5);
            t.i32(1, (hi - r) as i32);
            t.i32(2, 0);
            t.i32(3, 3);
            t.i32(4, 3);
            t.end();
            t.buf.push(0);
            out.extend_from_slice(&t.buf);
            out.extend_from_slice(&page);
            r = hi;
            if r >= nrows {
                break;
            }
        }
        metas.push(Meta { off, size: out.len() as i64 - off, nvals: nrows as i64 });
    }
    // Footer
    let mut t = Thrift::new();
    t.i32(1, 1);
    t.list(2, 12, cols.len() + 1);
    t.begin_elem();
    t.bin(4, b"schema");
    t.i32(5, cols.len() as i32);
    t.end();
    for (col, name) in cols.iter().zip(names) {
        t.begin_elem();
        t.i32(1, col.ptype());
        t.i32(3, if matches!(col, Col::OptStr { .. }) { 1 } else { 0 });
        t.bin(4, name.as_bytes());
        match col {
            Col::Str { .. } | Col::OptStr { .. } => {
                t.i32(6, 0);
                t.begin(10);
                t.begin(1);
                t.end();
                t.end();
            }
            Col::Date(_) => {
                t.i32(6, 6);
                t.begin(10);
                t.begin(6);
                t.end();
                t.end();
            }
            Col::I64(_) => {}
        }
        t.end();
    }
    t.i64(3, nrows as i64);
    t.list(4, 12, 1);
    t.begin_elem();
    t.list(1, 12, cols.len());
    let mut total = 0;
    for ((col, name), m) in cols.iter().zip(names).zip(&metas) {
        total += m.size;
        t.begin_elem();
        t.i64(2, m.off);
        t.begin(3);
        t.i32(1, col.ptype());
        if matches!(col, Col::OptStr { .. }) {
            t.list(2, 5, 2);
            t.zz(0);
            t.zz(3);
        } else {
            t.list(2, 5, 1);
            t.zz(0);
        }
        t.list(3, 8, 1);
        t.varint(name.len() as u64);
        t.buf.extend_from_slice(name.as_bytes());
        t.i32(4, 0);
        t.i64(5, m.nvals);
        t.i64(6, m.size);
        t.i64(7, m.size);
        t.i64(9, m.off);
        t.end();
        t.end();
    }
    t.i64(2, total);
    t.i64(3, nrows as i64);
    t.end();
    t.bin(6, b"shopping-sessions-engine");
    t.buf.push(0);
    out.extend_from_slice(&t.buf);
    out.extend_from_slice(&(t.buf.len() as u32).to_le_bytes());
    out.extend_from_slice(b"PAR1");
    let mut f = std::fs::File::create(path)?;
    f.write_all(&out)?;
    Ok(())
}
