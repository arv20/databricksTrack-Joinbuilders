//! A minimal Parquet writer: one row group, one PLAIN data page (v1) per
//! column, no compression, no statistics. Thrift compact protocol by hand.

use std::io::Write;

#[derive(Clone, Copy, PartialEq)]
pub enum Kind {
    Str,
    OptStr,
    Date,
    I32,
    I64,
}

pub enum Data {
    /// Concatenated bytes and end offsets (one per row).
    Str { bytes: Vec<u8>, ends: Vec<u32> },
    /// As `Str`, plus a present flag per row (absent rows have no bytes).
    OptStr { bytes: Vec<u8>, ends: Vec<u32>, present: Vec<bool> },
    I32(Vec<i32>),
    I64(Vec<i64>),
}

pub struct Column {
    pub name: &'static str,
    pub kind: Kind,
    pub data: Data,
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
        loop {
            let b = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(b);
                break;
            }
            self.buf.push(b | 0x80);
        }
    }
    fn zz(&mut self, v: i64) {
        self.varint(((v << 1) ^ (v >> 63)) as u64);
    }
    fn field(&mut self, id: i16, ty: u8) {
        let last = *self.last.last().unwrap();
        let delta = id - last;
        if delta > 0 && delta <= 15 {
            self.buf.push(((delta as u8) << 4) | ty);
        } else {
            self.buf.push(ty);
            self.zz(id as i64);
        }
        *self.last.last_mut().unwrap() = id;
    }
    fn i32f(&mut self, id: i16, v: i32) {
        self.field(id, 5);
        self.zz(v as i64);
    }
    fn i64f(&mut self, id: i16, v: i64) {
        self.field(id, 6);
        self.zz(v);
    }
    fn strf(&mut self, id: i16, s: &str) {
        self.field(id, 8);
        self.varint(s.len() as u64);
        self.buf.extend_from_slice(s.as_bytes());
    }
    fn begin_struct(&mut self, id: i16) {
        self.field(id, 12);
        self.last.push(0);
    }
    /// Begin a struct that is a list element (no field header).
    fn begin_elem(&mut self) {
        self.last.push(0);
    }
    fn end_struct(&mut self) {
        self.buf.push(0);
        self.last.pop();
    }
    fn list(&mut self, id: i16, elem_ty: u8, n: usize) {
        self.field(id, 9);
        if n < 15 {
            self.buf.push(((n as u8) << 4) | elem_ty);
        } else {
            self.buf.push(0xf0 | elem_ty);
            self.varint(n as u64);
        }
    }
}

fn phys(kind: Kind) -> i32 {
    match kind {
        Kind::Str | Kind::OptStr => 6,
        Kind::Date | Kind::I32 => 1,
        Kind::I64 => 2,
    }
}

/// Encodes `cols` (all with `rows` rows) as a complete Parquet file.
pub fn encode(cols: &[Column], rows: usize) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(1 << 16);
    out.extend_from_slice(b"PAR1");
    struct Meta {
        offset: i64,
        size: i64,
        num_values: i64,
    }
    let mut metas = Vec::new();
    if rows > 0 {
        for c in cols {
            let mut body: Vec<u8> = Vec::new();
            match &c.data {
                Data::Str { bytes, ends } => {
                    body.reserve(bytes.len() + 4 * ends.len());
                    let mut s = 0usize;
                    for &e in ends {
                        let e = e as usize;
                        body.extend_from_slice(&((e - s) as u32).to_le_bytes());
                        body.extend_from_slice(&bytes[s..e]);
                        s = e;
                    }
                }
                Data::OptStr { bytes, ends, present } => {
                    // definition levels: RLE/bit-packed hybrid, bit width 1, one bit-packed run
                    let groups = rows.div_ceil(8);
                    let mut lv = Thrift::new();
                    lv.varint(((groups as u64) << 1) | 1);
                    let mut levels = lv.buf;
                    let base = levels.len();
                    levels.resize(base + groups, 0);
                    for (i, &p) in present.iter().enumerate() {
                        if p {
                            levels[base + i / 8] |= 1 << (i % 8);
                        }
                    }
                    body.extend_from_slice(&(levels.len() as u32).to_le_bytes());
                    body.extend_from_slice(&levels);
                    let mut s = 0usize;
                    for (i, &e) in ends.iter().enumerate() {
                        let e = e as usize;
                        if present[i] {
                            body.extend_from_slice(&((e - s) as u32).to_le_bytes());
                            body.extend_from_slice(&bytes[s..e]);
                        }
                        s = e;
                    }
                }
                Data::I32(v) => {
                    body.reserve(4 * v.len());
                    for &x in v {
                        body.extend_from_slice(&x.to_le_bytes());
                    }
                }
                Data::I64(v) => {
                    body.reserve(8 * v.len());
                    for &x in v {
                        body.extend_from_slice(&x.to_le_bytes());
                    }
                }
            }
            let mut ph = Thrift::new();
            ph.i32f(1, 0); // DATA_PAGE
            ph.i32f(2, body.len() as i32);
            ph.i32f(3, body.len() as i32);
            ph.begin_struct(5);
            ph.i32f(1, rows as i32);
            ph.i32f(2, 0); // PLAIN
            ph.i32f(3, 3); // RLE
            ph.i32f(4, 3); // RLE
            ph.end_struct();
            ph.buf.push(0);
            let offset = out.len() as i64;
            out.extend_from_slice(&ph.buf);
            out.extend_from_slice(&body);
            metas.push(Meta { offset, size: out.len() as i64 - offset, num_values: rows as i64 });
        }
    }
    // FileMetaData
    let mut t = Thrift::new();
    t.i32f(1, 1);
    t.list(2, 12, cols.len() + 1);
    t.begin_elem();
    t.strf(4, "schema");
    t.i32f(5, cols.len() as i32);
    t.end_struct();
    for c in cols {
        t.begin_elem();
        t.i32f(1, phys(c.kind));
        t.i32f(3, if c.kind == Kind::OptStr { 1 } else { 0 });
        t.strf(4, c.name);
        match c.kind {
            Kind::Str | Kind::OptStr => {
                t.i32f(6, 0); // UTF8
                t.begin_struct(10);
                t.begin_struct(1); // STRING
                t.end_struct();
                t.end_struct();
            }
            Kind::Date => {
                t.i32f(6, 6); // DATE
                t.begin_struct(10);
                t.begin_struct(6); // DATE
                t.end_struct();
                t.end_struct();
            }
            Kind::I32 => {}
            Kind::I64 => {}
        }
        t.end_struct();
    }
    t.i64f(3, rows as i64);
    if rows > 0 {
        t.list(4, 12, 1);
        t.begin_elem();
        t.list(1, 12, cols.len());
        let mut total = 0i64;
        for (c, m) in cols.iter().zip(&metas) {
            total += m.size;
            t.begin_elem();
            t.i64f(2, m.offset);
            t.begin_struct(3);
            t.i32f(1, phys(c.kind));
            t.list(2, 5, 2);
            t.zz(0); // PLAIN
            t.zz(3); // RLE
            t.list(3, 8, 1);
            t.varint(c.name.len() as u64);
            t.buf.extend_from_slice(c.name.as_bytes());
            t.i32f(4, 0); // UNCOMPRESSED
            t.i64f(5, m.num_values);
            t.i64f(6, m.size);
            t.i64f(7, m.size);
            t.i64f(9, m.offset);
            t.end_struct();
            t.end_struct();
        }
        t.i64f(2, total);
        t.i64f(3, rows as i64);
        t.end_struct();
    } else {
        t.list(4, 12, 0);
    }
    t.strf(6, "shopping-sessions-rust");
    t.buf.push(0);
    out.extend_from_slice(&t.buf);
    out.extend_from_slice(&(t.buf.len() as u32).to_le_bytes());
    out.extend_from_slice(b"PAR1");
    out
}

pub fn write_file(path: &std::path::Path, cols: &[Column], rows: usize) -> std::io::Result<()> {
    let bytes = encode(cols, rows);
    let mut f = std::fs::File::create(path)?;
    f.write_all(&bytes)?;
    Ok(())
}
