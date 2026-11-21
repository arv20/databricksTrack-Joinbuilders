use crate::iface::*;
use crate::keyrec::unscramble;
use crate::sys::*;
use crate::*;
use std::sync::atomic::Ordering;

pub const CHUNK: usize = 4096;
pub const LINK_OFF: usize = 4032;
const BLOCK: usize = 2 << 20;
pub static mut NT: bool = true;
pub static mut RELEASE: bool = true;
pub static mut NOSINK: bool = false;
pub static mut PREFETCH: bool = false;
pub static mut SKIP: u32 = 0;

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Line([u64; 8]);

#[derive(Clone, Copy)]
struct Meta {
    cur: *mut u64,
    n: u32,
    fill: u32,
}

pub struct Part {
    lines: Vec<Line>,
    meta: Vec<Meta>,
    head: Vec<*mut u64>,
    arena: *const Arena,
    blk: *mut u8,
    blk_end: *mut u8,
}
unsafe impl Send for Part {}
impl Part {
    pub fn empty() -> Part {
        Part { lines: Vec::new(), meta: Vec::new(), head: Vec::new(), arena: std::ptr::null(), blk: std::ptr::null_mut(), blk_end: std::ptr::null_mut() }
    }
    pub fn new(arena: &Arena, nb: usize) -> Part {
        Part {
            lines: vec![Line([0; 8]); nb],
            meta: vec![Meta { cur: std::ptr::null_mut(), n: 0, fill: 0 }; nb],
            head: vec![std::ptr::null_mut(); nb],
            arena,
            blk: std::ptr::null_mut(),
            blk_end: std::ptr::null_mut(),
        }
    }
    #[inline(always)]
    pub fn push(&mut self, b: usize, w0: u64, w1: u64) {
        unsafe {
            let m = &mut *self.meta.as_mut_ptr().add(b);
            let f = m.fill as usize;
            let l = self.lines.as_mut_ptr().add(b) as *mut u64;
            *l.add(2 * f) = w0;
            *l.add(2 * f + 1) = w1;
            if f == 3 {
                m.fill = 0;
                m.n += 4;
                let mut c = m.cur;
                if c.is_null() || (c as usize & (CHUNK - 1)) == LINK_OFF {
                    c = self.new_chunk(b, c);
                }
                if NT {
                    use std::arch::x86_64::*;
                    _mm512_stream_si512(c as *mut __m512i, _mm512_load_si512(l as *const __m512i));
                } else {
                    std::ptr::copy_nonoverlapping(l, c, 8);
                }
                (*self.meta.as_mut_ptr().add(b)).cur = c.add(8);
            } else {
                m.fill = (f + 1) as u32;
            }
        }
    }
    #[inline(never)]
    unsafe fn new_chunk(&mut self, b: usize, old: *mut u64) -> *mut u64 {
        if self.blk == self.blk_end {
            self.blk = (*self.arena).alloc(BLOCK);
            self.blk_end = self.blk.add(BLOCK);
        }
        let n = self.blk as *mut u64;
        self.blk = self.blk.add(CHUNK);
        if old.is_null() {
            self.head[b] = n;
        } else {
            *(old as *mut usize) = n as usize;
        }
        n
    }
    pub fn finish(&mut self) {
        unsafe {
            for b in 0..self.meta.len() {
                let f = self.meta[b].fill as usize;
                if f > 0 {
                    let mut c = self.meta[b].cur;
                    if c.is_null() || (c as usize & (CHUNK - 1)) == LINK_OFF {
                        c = self.new_chunk(b, c);
                    }
                    std::ptr::copy_nonoverlapping(self.lines.as_ptr().add(b) as *const u64, c, 2 * f);
                    self.meta[b].cur = c.add(2 * f);
                    self.meta[b].n += f as u32;
                    self.meta[b].fill = 0;
                }
            }
            if NT {
                std::arch::x86_64::_mm_sfence();
            }
        }
    }
    #[inline(always)]
    pub fn for_each(&self, b: usize, mut f: impl FnMut(&[Rec])) {
        unsafe {
            if b >= self.head.len() {
                return;
            }
            let mut c = self.head[b];
            if c.is_null() {
                return;
            }
            let cur = self.meta[b].cur as usize;
            let last = (cur - 1) & !(CHUNK - 1);
            loop {
                if c as usize == last {
                    f(std::slice::from_raw_parts(c as *const Rec, (cur - last) / 16));
                    break;
                }
                let next = *((c as usize + LINK_OFF) as *const usize) as *mut u64;
                if PREFETCH {
                    use std::arch::x86_64::*;
                    _mm_prefetch::<_MM_HINT_T0>(next as *const i8);
                    _mm_prefetch::<_MM_HINT_T0>((next as *const i8).add(64));
                    _mm_prefetch::<_MM_HINT_T0>((next as *const i8).add(LINK_OFF));
                }
                f(std::slice::from_raw_parts(c as *const Rec, LINK_OFF / 16));
                c = next;
            }
        }
    }
    pub fn count(&self, b: usize) -> usize {
        self.meta[b].n as usize
    }
}

struct Ctx<'a> {
    g: &'a G,
    t: &'a mut TState,
    tid: u64,
    base: u64,
    fend: u64,
}
impl<'a> Ctx<'a> {
    #[inline(always)]
    fn token(&mut self, id: u64) -> u64 {
        if id & ID_ODD == 0 {
            return id;
        }
        self.raw_token(self.base + odd_offset(id) as u64, odd_len(id))
    }
    #[cold]
    #[inline(never)]
    fn raw_token(&mut self, off: u64, mut len: usize) -> u64 {
        if len >= 255 {
            let p = self.g.region;
            unsafe {
                while off + (len as u64) < self.fend {
                    let c = *p.add(off as usize + len);
                    if c == b',' || c == b'"' || c == b'\n' {
                        break;
                    }
                    len += 1;
                }
            }
        }
        let idx = self.t.raws.len() as u64;
        if idx >= (1 << 34) {
            fallback("too many raw IDs");
        }
        self.t.raws.push((off, len as u32));
        RAW | self.tid << 34 | idx
    }
}

impl<'a> EventSink for Ctx<'a> {
    #[inline(always)]
    fn event(&mut self, r: &EvRow) {
        let g = self.g;
        self.t.n_ev_rows += 1;
        if unsafe { NOSINK } {
            self.t.prof[7] = self.t.prof[7].wrapping_add(r.eid ^ r.uid ^ r.ts as u64 ^ r.ingest as u64 ^ r.price as u64 ^ r.product as u64);
            return;
        }
        let ing = r.ingest.wrapping_sub(g.ing_base) as u64;
        if r.ingest == TS_BAD || ing >= ING_MAX * 1000 {
            fallback("ingest_ts bad or out of range");
        }
        let ing_s = ing / 1000;
        let rel = r.ts.wrapping_sub(g.ws) as u64;
        let valid = r.uid != ID_EMPTY
            && rel < g.wlen
            && r.kind <= 2
            && r.product != PROD_EMPTY
            && (r.kind != 2 || r.price != 0)
            && r.ingest.wrapping_sub(r.ts) <= DAY_MS;
        let mut w0 = u64::MAX;
        let mut user = 0u64;
        if valid {
            let mut cat = 0u64;
            if r.price != 0 {
                let p = if (r.product as usize) < PBOUND { r.product as usize } else { PBOUND };
                cat = unsafe { *g.prod.get_unchecked(p) } as u64;
            }
            let mut alias = 0u64;
            let mut normal = false;
            if r.uid < (1 << 40) {
                let w = unscramble(r.uid, g.ukey);
                if (w >> 9) < g.ucap && (w & 511) < NALIAS {
                    user = w >> 9;
                    alias = w & 511;
                    normal = true;
                }
            }
            if !normal {
                let tok = self.token(r.uid);
                let idx = self.t.odd_users.len() as u64;
                if idx >= (1 << 22) {
                    fallback("too many odd-user rows");
                }
                self.t.odd_users.push(tok);
                user = ODDU | self.tid << 22 | idx;
            }
            w0 = rel << 33 | (r.price as u64) << 9 | cat << 4 | alias;
        }
        if r.eid < (1 << 40) {
            let v = unscramble(r.eid, g.ekey);
            if v < g.ecap {
                let w1 = (v & (BSIZE as u64 - 1)) | ing_s << 16 | user << 37;
                if unsafe { SKIP } & 32 != 0 {
                    self.t.prof[7] = self.t.prof[7].wrapping_add(w0 ^ w1);
                    return;
                }
                self.t.ev.push((v >> BB) as usize, w0, w1);
                return;
            }
        }
        let eid = self.token(r.eid);
        self.t.odd_events.push(OddEv { eid, ing: r.ingest, w0, user });
    }
}

impl<'a> ClickSink for Ctx<'a> {
    #[inline(always)]
    fn click(&mut self, r: &ClRow) {
        let g = self.g;
        self.t.n_ck_rows += 1;
        let rel = r.ts.wrapping_sub(g.cbase) as u64;
        if r.uid == ID_EMPTY || rel >= g.clen || r.campaign as usize >= CBOUND {
            return;
        }
        if unsafe { *g.camp.get_unchecked(r.campaign as usize) } == 255 {
            return;
        }
        let key = rel << 32 | (r.campaign as u64) << 4;
        let cid = self.token(r.cid);
        if r.uid < (1 << 40) {
            let w = unscramble(r.uid, g.ukey);
            if (w >> 9) < g.ucap && (w & 511) < NALIAS {
                let u = w >> 9;
                self.t.us.push((u >> UB) as usize, cid | (u & (UBSIZE as u64 - 1)) << 43, key | (w & 511));
                return;
            }
        }
        let utok = self.token(r.uid);
        self.t.odd_clicks.push([utok, key, cid]);
    }
}

fn links_chunk(c: &mut Ctx, buf: &[u8]) {
    let g = c.g;
    crate::k_ref::for_each_line(buf, |s, e| {
        let line = &buf[s..e];
        let Some((a, b)) = crate::setup::split2(line) else { return };
        if a.is_empty() || b.is_empty() {
            return;
        }
        c.t.n_lk_rows += 1;
        if a == b {
            return;
        }
        let oa = a.as_ptr() as usize - buf.as_ptr() as usize;
        let ob = b.as_ptr() as usize - buf.as_ptr() as usize;
        let ia = crate::k_ref::parse_id(buf, (oa, oa + a.len()), b'u');
        let ib = crate::k_ref::parse_id(buf, (ob, ob + b.len()), b'u');
        if ia & ID_ODD == 0 && ib & ID_ODD == 0 {
            if let (Some((ua, aa)), Some((ub, ab))) = (g.normal(ia), g.normal(ib)) {
                if ua == ub {
                    let u = ua as u64;
                    c.t.us.push((u >> UB) as usize, 1 << 63 | (u & (UBSIZE as u64 - 1)) << 43 | (aa as u64) << 4 | ab as u64, 0);
                    return;
                }
            }
        }
        let ta = if ia & ID_ODD == 0 { ia } else { c.raw_token(c.base + oa as u64, a.len()) };
        let tb = if ib & ID_ODD == 0 { ib } else { c.raw_token(c.base + ob as u64, b.len()) };
        c.t.gen_edges.push((ta, tb));
    });
}

#[inline]
fn next_line_start(data: &[u8], from: usize) -> usize {
    match data[from..].iter().position(|&b| b == b'\n') {
        Some(i) => from + i + 1,
        None => data.len(),
    }
}

pub fn stage1(g: &G, tid: usize) {
    let t = g.t(tid);
    t.ev = Part::new(&g.ev_arena, g.nb);
    t.us = Part::new(&g.us_arena, g.nub);
    t.core = crate::core::Core::new(g.nc, g.nd, g.row_cap, g.ws_mod, g.day0);
    loop {
        let i = g.qnext.fetch_add(1, Ordering::Relaxed);
        if i >= g.queue.len() {
            break;
        }
        let c = g.queue[i];
        let f = &g.files[c.file as usize];
        let data = unsafe { std::slice::from_raw_parts(g.region.add(f.off), f.len) };
        let s = if c.a == f.hdr { c.a } else { next_line_start(data, c.a - 1) };
        let e = if c.b >= f.len { f.len } else { next_line_start(data, c.b - 1) };
        if s >= e {
            continue;
        }
        let buf = &data[s..e];
        let mut ctx = Ctx { g, t: &mut *t, tid: tid as u64, base: (f.off + s) as u64, fend: (f.off + f.len) as u64 };
        match f.kind {
            Kind::Ev => <K as Kernel>::events(buf, &mut ctx),
            Kind::Ck => <K as Kernel>::clicks(buf, &mut ctx),
            Kind::Lk => links_chunk(&mut ctx, buf),
        }
        if unsafe { RELEASE } {
            let a = (f.off + s + 4095) & !4095;
            let b = (f.off + e) & !4095;
            if b > a {
                unsafe { madvise(g.region.add(a) as *mut u8, b - a, MADV_DONTNEED) };
            }
        }
    }
    t.ev.finish();
    t.us.finish();
}
