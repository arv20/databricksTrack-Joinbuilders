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
pub static mut PREFETCH: bool = false;
static POPW: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
fn popw() -> bool {
    *POPW.get_or_init(|| !std::env::var("XL_POPW").is_ok_and(|v| v == "0"))
}
const MADV_POPULATE_WRITE: i32 = 23;
#[cfg(feature = "knobs")]
pub static mut NOSINK: bool = false;
#[cfg(feature = "knobs")]
pub static mut SKIP: u32 = 0;
#[cfg(not(feature = "knobs"))]
pub const NOSINK: bool = false;
#[cfg(not(feature = "knobs"))]
pub const SKIP: u32 = 0;

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Line([u64; 8]);

pub struct Part {
    lines: Vec<Line>,
    fill: Vec<u8>,
    cur: Vec<*mut u64>,
    nch: Vec<u32>,
    n: Vec<u32>,
    head: Vec<*mut u64>,
    arena: *const Arena,
    blk: *mut u8,
    blk_end: *mut u8,
    popw: bool,
    pend: Vec<Line>,
    pb: Vec<u32>,
}
const NOCHUNK: usize = LINK_OFF;
unsafe impl Send for Part {}
impl Part {
    pub fn empty() -> Part {
        Part { lines: Vec::new(), fill: Vec::new(), cur: Vec::new(), nch: Vec::new(), n: Vec::new(), head: Vec::new(), arena: std::ptr::null(), blk: std::ptr::null_mut(), blk_end: std::ptr::null_mut(), popw: false, pend: Vec::new(), pb: Vec::new() }
    }
    pub fn new(arena: &Arena, nb: usize) -> Part {
        Part {
            lines: vec![Line([0; 8]); nb],
            fill: vec![0; nb + 64],
            cur: vec![NOCHUNK as *mut u64; nb],
            nch: vec![0; nb],
            n: vec![0; nb],
            head: vec![std::ptr::null_mut(); nb],
            arena,
            blk: std::ptr::null_mut(),
            blk_end: std::ptr::null_mut(),
            popw: popw(),
            pend: vec![Line([0; 8]); 64],
            pb: vec![0; 64],
        }
    }
    #[inline(always)]
    pub fn push(&mut self, b: usize, w0: u64, w1: u64) {
        unsafe {
            let fp = self.fill.as_mut_ptr().add(b);
            let f = *fp as usize;
            let l = self.lines.as_mut_ptr().add(b) as *mut u64;
            *l.add(2 * f) = w0;
            *l.add(2 * f + 1) = w1;
            if f == 3 {
                *fp = 0;
                self.flush(b, l);
            } else {
                *fp = (f + 1) as u8;
            }
        }
    }
    #[inline(always)]
    pub unsafe fn push_many(&mut self, mut m: u64, ob: *const u64, o0: *const u64, o1: *const u64) {
        use std::arch::x86_64::*;
        let fill = self.fill.as_mut_ptr();
        let lines = self.lines.as_mut_ptr() as *mut u64;
        let pend = self.pend.as_mut_ptr() as *mut __m512i;
        let pb = self.pb.as_mut_ptr();
        let mut k = 0usize;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            let b = *ob.add(i) as usize;
            let w0 = *o0.add(i);
            let w1 = *o1.add(i);
            let f = *fill.add(b) as usize;
            let l = lines.add(b * 8);
            let z = _mm512_load_si512(l as *const __m512i);
            *fill.add(b) = ((f + 1) & 3) as u8;
            *l.add(2 * f) = w0;
            *l.add(2 * f + 1) = w1;
            let z = _mm512_mask_broadcast_i64x2(z, 0xC0, _mm_set_epi64x(w1 as i64, w0 as i64));
            _mm512_store_si512(pend.add(k), z);
            *pb.add(k) = b as u32;
            k += (f == 3) as usize;
        }
        for j in 0..k {
            let b = *pb.add(j) as usize;
            let cp = self.cur.as_mut_ptr().add(b);
            let mut c = *cp;
            if (c as usize & (CHUNK - 1)) == LINK_OFF {
                c = self.new_chunk(b, c);
            }
            if NT {
                _mm512_stream_si512(c as *mut __m512i, _mm512_load_si512(pend.add(j)));
            } else {
                _mm512_storeu_si512(c as *mut __m512i, _mm512_load_si512(pend.add(j)));
            }
            *cp = c.add(8);
        }
    }
    #[inline(always)]
    unsafe fn flush(&mut self, b: usize, l: *mut u64) {
        let cp = self.cur.as_mut_ptr().add(b);
        let mut c = *cp;
        if (c as usize & (CHUNK - 1)) == LINK_OFF {
            c = self.new_chunk(b, c);
        }
        if NT {
            use std::arch::x86_64::*;
            _mm512_stream_si512(c as *mut __m512i, _mm512_load_si512(l as *const __m512i));
        } else {
            std::ptr::copy_nonoverlapping(l, c, 8);
        }
        *cp = c.add(8);
    }
    #[inline(never)]
    unsafe fn new_chunk(&mut self, b: usize, old: *mut u64) -> *mut u64 {
        if self.blk == self.blk_end {
            self.blk = (*self.arena).alloc(BLOCK);
            self.blk_end = self.blk.add(BLOCK);
            if self.popw {
                madvise(self.blk, BLOCK, MADV_POPULATE_WRITE);
            }
        }
        let n = self.blk as *mut u64;
        self.blk = self.blk.add(CHUNK);
        if old as usize == NOCHUNK {
            self.head[b] = n;
        } else {
            *(old as *mut usize) = n as usize;
        }
        self.nch[b] += 1;
        n
    }
    pub fn finish(&mut self) {
        unsafe {
            for b in 0..self.head.len() {
                let f = self.fill[b] as usize;
                if f > 0 {
                    let mut c = self.cur[b];
                    if (c as usize & (CHUNK - 1)) == LINK_OFF {
                        c = self.new_chunk(b, c);
                    }
                    std::ptr::copy_nonoverlapping(self.lines.as_ptr().add(b) as *const u64, c, 2 * f);
                    self.cur[b] = c.add(2 * f);
                    self.fill[b] = 0;
                }
                self.n[b] = if self.head[b].is_null() {
                    0
                } else {
                    let cur = self.cur[b] as usize;
                    let last = (cur - 1) & !(CHUNK - 1);
                    (self.nch[b] - 1) * (LINK_OFF as u32 / 16) + ((cur - last) / 16) as u32
                };
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
            let cur = self.cur[b] as usize;
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
        self.n[b] as usize
    }
}

#[inline(always)]
unsafe fn mul8(a: std::arch::x86_64::__m512i, b: std::arch::x86_64::__m512i) -> std::arch::x86_64::__m512i {
    use std::arch::x86_64::*;
    let m = |x: __m256i, y: __m256i| {
        let r: __m256i;
        std::arch::asm!("vpmullq {r}, {x}, {y}", r = lateout(ymm_reg) r, x = in(ymm_reg) x, y = in(ymm_reg) y, options(pure, nomem, nostack, preserves_flags));
        r
    };
    let lo = m(_mm512_castsi512_si256(a), _mm512_castsi512_si256(b));
    let hi = m(_mm512_extracti64x4_epi64::<1>(a), _mm512_extracti64x4_epi64::<1>(b));
    _mm512_inserti64x4::<1>(_mm512_castsi256_si512(lo), hi)
}
#[inline(always)]
unsafe fn mulu32_8(a: std::arch::x86_64::__m512i, b: std::arch::x86_64::__m512i) -> std::arch::x86_64::__m512i {
    use std::arch::x86_64::*;
    let m = |x: __m256i, y: __m256i| {
        let r: __m256i;
        std::arch::asm!("vpmuludq {r}, {x}, {y}", r = lateout(ymm_reg) r, x = in(ymm_reg) x, y = in(ymm_reg) y, options(pure, nomem, nostack, preserves_flags));
        r
    };
    let lo = m(_mm512_castsi512_si256(a), _mm512_castsi512_si256(b));
    let hi = m(_mm512_extracti64x4_epi64::<1>(a), _mm512_extracti64x4_epi64::<1>(b));
    _mm512_inserti64x4::<1>(_mm512_castsi256_si512(lo), hi)
}

const NB: usize = 64;

#[repr(C, align(64))]
pub struct EvBatch {
    eid: [u64; NB],
    uid: [u64; NB],
    ts: [i64; NB],
    ing: [i64; NB],
    pp: [u64; NB],
    o0: [u64; NB],
    o1: [u64; NB],
    ob: [u64; NB],
    n: usize,
}
#[repr(C, align(64))]
pub struct CkBatch {
    cid: [u64; NB],
    uid: [u64; NB],
    ts: [i64; NB],
    camp: [u64; NB],
    o0: [u64; NB],
    o1: [u64; NB],
    ob: [u64; NB],
    n: usize,
}
impl EvBatch {
    fn new() -> Box<EvBatch> {
        unsafe { Box::new(std::mem::zeroed()) }
    }
    fn row(&self, i: usize) -> EvRow {
        let pp = self.pp[i];
        EvRow { eid: self.eid[i], uid: self.uid[i], ts: self.ts[i], ingest: self.ing[i], kind: (pp >> 56) as u8, product: pp as u32, price: (pp >> 32) as u32 & 0xFF_FFFF }
    }
}
impl CkBatch {
    fn new() -> Box<CkBatch> {
        unsafe { Box::new(std::mem::zeroed()) }
    }
}

struct Tabs {
    prod: Vec<u8>,
    camp: Vec<u8>,
}
static TABS: std::sync::OnceLock<Tabs> = std::sync::OnceLock::new();
fn tabs(g: &G) -> &'static Tabs {
    TABS.get_or_init(|| {
        let mut prod = vec![0u8; PBOUND + 1 + 64];
        prod[..PBOUND + 1].copy_from_slice(&g.prod[..PBOUND + 1]);
        let mut camp = vec![255u8; CBOUND + 64];
        camp[..CBOUND].copy_from_slice(&g.camp[..CBOUND]);
        Tabs { prod, camp }
    })
}

struct Ctx<'a> {
    g: &'a G,
    t: &'a mut TState,
    eb: &'a mut EvBatch,
    cb: &'a mut CkBatch,
    tabs: &'static Tabs,
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

    #[inline(never)]
    fn ev_one(&mut self, r: &EvRow) {
        let g = self.g;
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
                self.t.ev.push((v >> BB) as usize, w0, w1);
                return;
            }
        }
        let eid = self.token(r.eid);
        self.t.odd_events.push(OddEv { eid, ing: r.ingest, w0, user });
    }

    #[inline(never)]
    fn ev_batch(&mut self) {
        let n = self.eb.n;
        if n == 0 {
            return;
        }
        self.eb.n = 0;
        self.t.n_ev_rows += n as u64;
        if unsafe { NOSINK } {
            for i in 0..n {
                self.t.prof[7] = self.t.prof[7].wrapping_add(self.eb.eid[i] ^ self.eb.uid[i] ^ self.eb.ts[i] as u64 ^ self.eb.ing[i] as u64 ^ self.eb.pp[i]);
            }
            return;
        }
        let g = self.g;
        let p = EvP { ing_base: g.ing_base, ws: g.ws, wlen: g.wlen, ukey: g.ukey, ekey: g.ekey, ucap: g.ucap, ecap: g.ecap, ptab: self.tabs.prod.as_ptr() };
        let (okb, spb, bad) = unsafe { ev_vec(&mut *self.eb, n, &p) };
        if bad {
            fallback("ingest_ts bad or out of range");
        }
        if unsafe { SKIP } & 2048 != 0 {
            self.t.prof[7] = self.t.prof[7].wrapping_add(okb ^ spb ^ self.eb.o0[3] ^ self.eb.o1[5] ^ self.eb.ob[7]);
            return;
        }
        unsafe { self.t.ev.push_many(okb, self.eb.ob.as_ptr(), self.eb.o0.as_ptr(), self.eb.o1.as_ptr()) };
        let mut m = spb;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            let r = self.eb.row(i);
            self.ev_one(&r);
        }
    }

    #[inline(never)]
    fn ck_one(&mut self, r: &ClRow) {
        let g = self.g;
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

    #[inline(never)]
    fn ck_batch(&mut self) {
        let n = self.cb.n;
        if n == 0 {
            return;
        }
        self.cb.n = 0;
        self.t.n_ck_rows += n as u64;
        if unsafe { SKIP } & 64 != 0 {
            for i in 0..n {
                self.t.prof[7] = self.t.prof[7].wrapping_add(self.cb.cid[i] ^ self.cb.uid[i] ^ self.cb.ts[i] as u64 ^ self.cb.camp[i]);
            }
            return;
        }
        let g = self.g;
        let p = CkP { cbase: g.cbase, clen: g.clen, ukey: g.ukey, ucap: g.ucap, ctab: self.tabs.camp.as_ptr() };
        let (okb, spb) = unsafe { ck_vec(&mut *self.cb, n, &p) };
        unsafe { self.t.us.push_many(okb, self.cb.ob.as_ptr(), self.cb.o0.as_ptr(), self.cb.o1.as_ptr()) };
        let mut m = spb;
        while m != 0 {
            let i = m.trailing_zeros() as usize;
            m &= m - 1;
            let cb = &*self.cb;
            let r = ClRow { cid: cb.cid[i], uid: cb.uid[i], campaign: cb.camp[i] as u32, ts: cb.ts[i] };
            self.ck_one(&r);
        }
    }
}

struct EvP {
    ing_base: i64,
    ws: i64,
    wlen: u64,
    ukey: u64,
    ekey: u64,
    ucap: u64,
    ecap: u64,
    ptab: *const u8,
}

#[inline(always)]
unsafe fn ev_vec(eb: &mut EvBatch, n: usize, p: &EvP) -> (u64, u64, bool) {
    use std::arch::x86_64::*;
    let mut okb = 0u64;
    let mut spb = 0u64;
    let mut badm = 0u32;
    let s = |v: u64| _mm512_set1_epi64(v as i64);
    let m40 = s(crate::keyrec::MASK);
    let binv = s(crate::keyrec::BINV);
    let ainv = s(crate::keyrec::AINV);
    let ukey = s(p.ukey & crate::keyrec::MASK);
    let ekey = s(p.ekey & crate::keyrec::MASK);
    let lim40 = s(1 << 40);
    let unscr = |y: __m512i, key: __m512i| {
        let x = _mm512_and_si512(mul8(y, binv), m40);
        let x = _mm512_xor_si512(x, _mm512_xor_si512(_mm512_srli_epi64::<17>(x), _mm512_srli_epi64::<34>(x)));
        _mm512_and_si512(mul8(_mm512_xor_si512(x, key), ainv), m40)
    };
    let ing_base = s(p.ing_base as u64);
    let ing_lim = s(ING_MAX * 1000);
    let ts_bad = s(TS_BAD as u64);
    let ws = s(p.ws as u64);
    let wlen = s(p.wlen);
    let empty = s(ID_EMPTY);
    let two = s(2);
    let zero = _mm512_setzero_si512();
    let lo32 = s(0xFFFF_FFFF);
    let p24 = s(0xFF_FFFF);
    let pempty = s(PROD_EMPTY as u64);
    let pbound = s(PBOUND as u64);
    let day = s(DAY_MS as u64);
    let ucap = s(p.ucap);
    let nal = s(NALIAS);
    let ecap = s(p.ecap);
    let m511 = s(511);
    let mff = s(255);
    let slotm = s(BSIZE as u64 - 1);
    let magic = s(274_877_907);
    let ones = s(u64::MAX);
    let mut j = 0;
    while j < n {
        let lm: __mmask8 = if n - j >= 8 { 0xFF } else { ((1u32 << (n - j)) - 1) as u8 };
        let ingr = _mm512_load_si512(eb.ing.as_ptr().add(j) as *const __m512i);
        let ing = _mm512_sub_epi64(ingr, ing_base);
        badm |= (lm & (_mm512_cmpeq_epi64_mask(ingr, ts_bad) | _mm512_cmpge_epu64_mask(ing, ing_lim))) as u32;
        let ing_s = _mm512_srli_epi64::<38>(mulu32_8(ing, magic));
        let ts = _mm512_load_si512(eb.ts.as_ptr().add(j) as *const __m512i);
        let rel = _mm512_sub_epi64(ts, ws);
        let uid = _mm512_load_si512(eb.uid.as_ptr().add(j) as *const __m512i);
        let pp = _mm512_load_si512(eb.pp.as_ptr().add(j) as *const __m512i);
        let kind = _mm512_srli_epi64::<56>(pp);
        let prod = _mm512_and_si512(pp, lo32);
        let price = _mm512_and_si512(_mm512_srli_epi64::<32>(pp), p24);
        let valid = lm
            & _mm512_cmpneq_epi64_mask(uid, empty)
            & _mm512_cmplt_epu64_mask(rel, wlen)
            & _mm512_cmple_epu64_mask(kind, two)
            & _mm512_cmpneq_epi64_mask(prod, pempty)
            & (_mm512_cmpneq_epi64_mask(kind, two) | _mm512_cmpneq_epi64_mask(price, zero))
            & _mm512_cmple_epi64_mask(_mm512_sub_epi64(ingr, ts), day);
        let w = unscr(uid, ukey);
        let user = _mm512_srli_epi64::<9>(w);
        let alias = _mm512_and_si512(w, m511);
        let normal = _mm512_cmplt_epu64_mask(uid, lim40) & _mm512_cmplt_epu64_mask(user, ucap) & _mm512_cmplt_epu64_mask(alias, nal);
        let pm = valid & _mm512_cmpneq_epi64_mask(price, zero);
        let cat = _mm512_and_si512(_mm512_mask_i64gather_epi64::<1>(zero, pm, _mm512_min_epu64(prod, pbound), p.ptab as *const i64), mff);
        let w0v = _mm512_or_si512(
            _mm512_or_si512(_mm512_slli_epi64::<33>(rel), _mm512_slli_epi64::<9>(price)),
            _mm512_or_si512(_mm512_slli_epi64::<4>(cat), alias),
        );
        let w0v = _mm512_mask_blend_epi64(valid, ones, w0v);
        let userv = _mm512_maskz_mov_epi64(valid, user);
        let eid = _mm512_load_si512(eb.eid.as_ptr().add(j) as *const __m512i);
        let v = unscr(eid, ekey);
        let evok = _mm512_cmplt_epu64_mask(eid, lim40) & _mm512_cmplt_epu64_mask(v, ecap);
        let w1v = _mm512_or_si512(_mm512_and_si512(v, slotm), _mm512_or_si512(_mm512_slli_epi64::<16>(ing_s), _mm512_slli_epi64::<37>(userv)));
        let ok = lm & evok & !(valid & !normal);
        _mm512_store_si512(eb.o0.as_mut_ptr().add(j) as *mut __m512i, w0v);
        _mm512_store_si512(eb.o1.as_mut_ptr().add(j) as *mut __m512i, w1v);
        _mm512_store_si512(eb.ob.as_mut_ptr().add(j) as *mut __m512i, _mm512_srli_epi64::<BB>(v));
        okb |= (ok as u64) << j;
        spb |= ((lm & !ok) as u64) << j;
        j += 8;
    }
    (okb, spb, badm != 0)
}

struct CkP {
    cbase: i64,
    clen: u64,
    ukey: u64,
    ucap: u64,
    ctab: *const u8,
}

#[inline(always)]
unsafe fn ck_vec(cb: &mut CkBatch, n: usize, p: &CkP) -> (u64, u64) {
    use std::arch::x86_64::*;
    let mut okb = 0u64;
    let mut spb = 0u64;
    let s = |v: u64| _mm512_set1_epi64(v as i64);
    let m40 = s(crate::keyrec::MASK);
    let binv = s(crate::keyrec::BINV);
    let ainv = s(crate::keyrec::AINV);
    let ukey = s(p.ukey & crate::keyrec::MASK);
    let lim40 = s(1 << 40);
    let cbase = s(p.cbase as u64);
    let clen = s(p.clen);
    let empty = s(ID_EMPTY);
    let cbound = s(CBOUND as u64);
    let mff = s(255);
    let ucap = s(p.ucap);
    let nal = s(NALIAS);
    let m511 = s(511);
    let ulo = s(UBSIZE as u64 - 1);
    let mut j = 0;
    while j < n {
        let lm: __mmask8 = if n - j >= 8 { 0xFF } else { ((1u32 << (n - j)) - 1) as u8 };
        let ts = _mm512_load_si512(cb.ts.as_ptr().add(j) as *const __m512i);
        let rel = _mm512_sub_epi64(ts, cbase);
        let uid = _mm512_load_si512(cb.uid.as_ptr().add(j) as *const __m512i);
        let camp = _mm512_load_si512(cb.camp.as_ptr().add(j) as *const __m512i);
        let inb = lm & _mm512_cmplt_epu64_mask(camp, cbound);
        let ch = _mm512_and_si512(_mm512_mask_i64gather_epi64::<1>(mff, inb, camp, p.ctab as *const i64), mff);
        let keep = inb & _mm512_cmpneq_epi64_mask(uid, empty) & _mm512_cmplt_epu64_mask(rel, clen) & _mm512_cmpneq_epi64_mask(ch, mff);
        let x = _mm512_and_si512(mul8(uid, binv), m40);
        let x = _mm512_xor_si512(x, _mm512_xor_si512(_mm512_srli_epi64::<17>(x), _mm512_srli_epi64::<34>(x)));
        let w = _mm512_and_si512(mul8(_mm512_xor_si512(x, ukey), ainv), m40);
        let u = _mm512_srli_epi64::<9>(w);
        let alias = _mm512_and_si512(w, m511);
        let normal = _mm512_cmplt_epu64_mask(uid, lim40) & _mm512_cmplt_epu64_mask(u, ucap) & _mm512_cmplt_epu64_mask(alias, nal);
        let cid = _mm512_load_si512(cb.cid.as_ptr().add(j) as *const __m512i);
        let cidok = _mm512_cmplt_epu64_mask(cid, lim40);
        let key = _mm512_or_si512(_mm512_slli_epi64::<32>(rel), _mm512_slli_epi64::<4>(camp));
        let o0 = _mm512_or_si512(cid, _mm512_slli_epi64::<43>(_mm512_and_si512(u, ulo)));
        let o1 = _mm512_or_si512(key, alias);
        let ok = keep & normal & cidok;
        _mm512_store_si512(cb.o0.as_mut_ptr().add(j) as *mut __m512i, o0);
        _mm512_store_si512(cb.o1.as_mut_ptr().add(j) as *mut __m512i, o1);
        _mm512_store_si512(cb.ob.as_mut_ptr().add(j) as *mut __m512i, _mm512_srli_epi64::<UB>(u));
        okb |= (ok as u64) << j;
        spb |= ((keep & !ok) as u64) << j;
        j += 8;
    }
    (okb, spb)
}

impl<'a> EventSink for Ctx<'a> {
    #[inline(always)]
    fn event(&mut self, r: &EvRow) {
        let b = &mut *self.eb;
        let i = b.n;
        unsafe {
            *b.eid.get_unchecked_mut(i) = r.eid;
            *b.uid.get_unchecked_mut(i) = r.uid;
            *b.ts.get_unchecked_mut(i) = r.ts;
            *b.ing.get_unchecked_mut(i) = r.ingest;
            *b.pp.get_unchecked_mut(i) = r.product as u64 | (r.price as u64) << 32 | (r.kind as u64) << 56;
        }
        b.n = i + 1;
        if i + 1 == NB {
            self.ev_batch();
        }
    }
}

impl<'a> ClickSink for Ctx<'a> {
    #[inline(always)]
    fn click(&mut self, r: &ClRow) {
        let b = &mut *self.cb;
        let i = b.n;
        unsafe {
            *b.cid.get_unchecked_mut(i) = r.cid;
            *b.uid.get_unchecked_mut(i) = r.uid;
            *b.ts.get_unchecked_mut(i) = r.ts;
            *b.camp.get_unchecked_mut(i) = r.campaign as u64;
        }
        b.n = i + 1;
        if i + 1 == NB {
            self.ck_batch();
        }
    }
}

#[inline(always)]
unsafe fn link_fast(p: *const u8, len: usize) -> Option<(u64, u64)> {
    use std::arch::x86_64::*;
    let v = _mm512_loadu_si512(p as *const __m512i);
    let n = len as u32;
    let cm = _bzhi_u64(_mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b',' as i8)), n);
    let qm = _bzhi_u64(_mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'"' as i8)), n);
    let c0 = _tzcnt_u64(cm) as u32;
    let qa = _bzhi_u64(qm, c0);
    let lb = n.wrapping_sub(c0 + 1);
    let qb = qm.wrapping_shr(c0 + 1);
    let fa = (qa != 0) as u32;
    let fb = (qb != 0) as u32;
    let bad_q = ((qa != 0) & (qa != (1 | 1u64.wrapping_shl(c0.wrapping_sub(1))))) | ((qb != 0) & (qb != (1 | 1u64.wrapping_shl(lb.wrapping_sub(1)))));
    let la = c0.wrapping_sub(2 * fa);
    let lv = lb.wrapping_sub(2 * fb);
    let sa = fa as usize;
    let sb = (c0 + 1 + fb) as usize;
    let bad = (n >= 64) | (_popcnt64(cm as i64) != 1) | bad_q | (la.wrapping_sub(2) > 7) | (lv.wrapping_sub(2) > 7);
    if bad {
        return None;
    }
    if (*p.add(sa) != b'u') | (*p.add(sb) != b'u') {
        return None;
    }
    let (ia, oka) = crate::keyrec::base36_word((p.add(sa + 1) as *const u64).read_unaligned(), la - 1);
    let (ib, okb) = crate::keyrec::base36_word((p.add(sb + 1) as *const u64).read_unaligned(), lv - 1);
    if !(oka & okb) {
        return None;
    }
    Some((ia, ib))
}

#[inline(never)]
fn link_slow(c: &mut Ctx, buf: &[u8], s: usize, e: usize) {
    let g = c.g;
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
}

fn links_chunk(c: &mut Ctx, buf: &[u8]) {
    use std::arch::x86_64::*;
    const LB: usize = 8192;
    let g = c.g;
    let len = buf.len();
    let base = buf.as_ptr();
    let mut ends = [0u32; LB + 64];
    let mut pos = 0usize;
    let mut off = 0usize;
    while off < len {
        let stop = (off + LB).min(len);
        let mut k = 0usize;
        let mut o = off;
        while o < stop {
            let mut m = unsafe { _mm512_cmpeq_epi8_mask(_mm512_loadu_si512(base.add(o) as *const __m512i), _mm512_set1_epi8(b'\n' as i8)) };
            if stop - o < 64 {
                m = unsafe { _bzhi_u64(m, (stop - o) as u32) };
            }
            while m != 0 {
                ends[k] = (o + m.trailing_zeros() as usize) as u32;
                k += 1;
                m &= m - 1;
            }
            o += 64;
        }
        for &e in &ends[..k] {
            let e = e as usize;
            if pos + 64 <= len {
                if let Some((ia, ib)) = unsafe { link_fast(base.add(pos), e - pos) } {
                    pos = e + 1;
                    c.t.n_lk_rows += 1;
                    if ia == ib {
                        continue;
                    }
                    if let (Some((ua, aa)), Some((ub, ab))) = (g.normal(ia), g.normal(ib)) {
                        if ua == ub {
                            let u = ua as u64;
                            c.t.us.push((u >> UB) as usize, 1 << 63 | (u & (UBSIZE as u64 - 1)) << 43 | (aa as u64) << 4 | ab as u64, 0);
                            continue;
                        }
                    }
                    c.t.gen_edges.push((ia, ib));
                    continue;
                }
            }
            link_slow(c, buf, pos, e);
            pos = e + 1;
        }
        off = stop;
    }
    if pos < len {
        link_slow(c, buf, pos, len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn link_fast_matches_scalar() {
        let mut x = 0x1234_5678_9abc_def1u64;
        let mut rnd = |m: u64| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x % m
        };
        let alpha = b"0123456789abcdefghijklmnopqrstuvwxyzU\"\", u_\r0";
        let mut fast = 0;
        for _ in 0..3_000_000 {
            let mut line = Vec::new();
            for side in 0..2 {
                if side == 1 {
                    line.push(b',');
                }
                let q = rnd(4) == 0;
                if q {
                    line.push(b'"');
                }
                if rnd(20) != 0 {
                    line.push(b'u');
                }
                let k = rnd(11);
                for _ in 0..k {
                    let c = if rnd(10) == 0 { alpha[rnd(alpha.len() as u64) as usize] } else { alpha[rnd(36) as usize] };
                    line.push(c);
                }
                if q && rnd(10) != 0 {
                    line.push(b'"');
                }
            }
            if rnd(50) == 0 {
                line.push(b',');
            }
            let end = line.len();
            line.push(b'\n');
            line.resize(end + 80, b'x');
            let r = unsafe { link_fast(line.as_ptr(), end) }.map(|(a, b)| (end, a, b));
            let sc = crate::setup::split2(&line[..end]).and_then(|(a, b)| {
                if a.is_empty() || b.is_empty() {
                    return None;
                }
                let oa = a.as_ptr() as usize - line.as_ptr() as usize;
                let ob = b.as_ptr() as usize - line.as_ptr() as usize;
                Some((crate::k_ref::parse_id(&line, (oa, oa + a.len()), b'u'), crate::k_ref::parse_id(&line, (ob, ob + b.len()), b'u')))
            });
            if let Some((n, ia, ib)) = r {
                fast += 1;
                assert_eq!(n, end, "{:?}", String::from_utf8_lossy(&line[..end]));
                assert_eq!(sc, Some((ia, ib)), "{:?}", String::from_utf8_lossy(&line[..end]));
                assert!(ia < (1 << 40) && ib < (1 << 40));
            }
        }
        assert!(fast > 100_000, "{fast}");
    }

    #[test]
    #[ignore]
    fn link_fast_share() {
        let dir = std::env::var("LINKDIR").unwrap();
        let (mut n, mut f) = (0, 0);
        for e in std::fs::read_dir(dir).unwrap() {
            let mut d = std::fs::read(e.unwrap().path()).unwrap();
            let h = d.iter().position(|&b| b == b'\n').unwrap_or(d.len()) + 1;
            let len = d.len();
            d.resize(len + 128, 0);
            let mut pos = h.min(len);
            while pos < len {
                n += 1;
                let e = pos + d[pos..].iter().position(|&b| b == b'\n').unwrap();
                if unsafe { link_fast(d.as_ptr().add(pos), e - pos) }.is_some() {
                    f += 1;
                }
                pos = e + 1;
            }
        }
        eprintln!("lines {n} fast {f}");
        let files: Vec<Vec<u8>> = std::fs::read_dir(std::env::var("LINKDIR").unwrap()).unwrap().map(|e| {
            let mut d = std::fs::read(e.unwrap().path()).unwrap();
            d.resize(d.len() + 128, 0);
            d
        }).collect();
        let mut best = f64::MAX;
        let mut acc = 0u64;
        for _ in 0..20 {
            let t = std::time::Instant::now();
            for d in &files {
                let len = d.len() - 128;
                let mut pos = d.iter().position(|&b| b == b'\n').unwrap_or(len) + 1;
                while pos < len {
                    let e = pos + d[pos..].iter().position(|&b| b == b'\n').unwrap();
                    if let Some((a, b)) = unsafe { link_fast(d.as_ptr().add(pos), e - pos) } {
                        acc = acc.wrapping_add(crate::keyrec::unscramble(a, 0x1234) ^ crate::keyrec::unscramble(b, 0x1234));
                    }
                    pos = e + 1;
                }
            }
            best = best.min(t.elapsed().as_secs_f64());
        }
        eprintln!("fast loop {:.3} ms ({:.1} ns/line) {acc}", best * 1e3, best * 1e9 / n as f64);
    }

    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn below(&mut self, m: u64) -> u64 {
            self.next() % m
        }
    }

    #[test]
    fn ev_vec_matches_scalar() {
        use crate::keyrec::{scramble, unscramble};
        let mut r = Rng(0x9E37_79B9_7F4A_7C15);
        let mut prod = vec![0u8; PBOUND + 1 + 64];
        for x in prod.iter_mut().take(PBOUND + 1) {
            *x = r.below(31) as u8;
        }
        let mut eb = EvBatch::new();
        let (mut n_ok, mut n_sp, mut n_bad) = (0, 0, 0);
        for round in 0..40_000 {
            let ws = (r.below(1 << 41) as i64) + 8 * DAY_MS;
            let wlen = (1 + r.below(24)) * DAY_MS as u64 - r.below(3);
            let p = EvP {
                ing_base: ws - DAY_MS,
                ws,
                wlen,
                ukey: r.next(),
                ekey: r.next(),
                ucap: 1 + r.below(1 << 26),
                ecap: 1 + r.below(1 << 28),
                ptab: prod.as_ptr(),
            };
            let n = 1 + r.below(64) as usize;
            let mut rows = Vec::new();
            for _ in 0..n {
                let eid = match r.below(8) {
                    0 => ID_ODD | r.below(1 << 40),
                    1 => (1 << 40) + r.below(1 << 42),
                    2 => scramble(p.ecap + r.below(1 << 20), p.ekey),
                    3 => r.below(1 << 40),
                    _ => scramble(r.below(p.ecap), p.ekey),
                };
                let uid = match r.below(10) {
                    0 => ID_EMPTY,
                    1 => ID_ODD | r.below(1 << 40),
                    2 => (1 << 40) + r.below(1 << 42),
                    3 => scramble(r.below(p.ucap) * 512 + 14 + r.below(498), p.ukey),
                    4 => scramble((p.ucap + r.below(100)) * 512 + r.below(14), p.ukey),
                    5 => scramble((1 << 39) + r.below(1000), p.ukey),
                    _ => scramble(r.below(p.ucap) * 512 + r.below(14), p.ukey),
                };
                let ts = match r.below(12) {
                    0 => TS_BAD,
                    1 => TS_HUGE,
                    2 => ws - 1 + r.below(3) as i64,
                    3 => ws + wlen as i64 - 2 + r.below(4) as i64,
                    4 => r.next() as i64,
                    _ => ws + r.below(wlen) as i64,
                };
                let base_ts = if ts == TS_BAD || ts == TS_HUGE { ws + r.below(wlen) as i64 } else { ts };
                let ingest = match r.below(400) {
                    0 => TS_BAD,
                    1 => p.ing_base - 1 - r.below(5) as i64,
                    2 => p.ing_base + (ING_MAX * 1000) as i64 + r.below(5) as i64,
                    _ => match r.below(6) {
                        0 => base_ts + DAY_MS - 1 + r.below(3) as i64,
                        1 => base_ts - r.below(100_000) as i64,
                        _ => base_ts + r.below(3_600_000) as i64,
                    },
                };
                let kind = r.below(4) as u8;
                let product = match r.below(10) {
                    0 => PROD_EMPTY,
                    1 => PROD_ODD,
                    2 => (PBOUND as u64 + r.below(1 << 27)) as u32,
                    3 => PBOUND as u32,
                    _ => r.below(PBOUND as u64) as u32,
                };
                let price = if kind == 2 || r.below(50) == 0 { [0, 1, 10_000_000, r.below(10_000_000) as u32 + 1][r.below(4) as usize] } else { 0 };
                rows.push(EvRow { eid, uid, ts, ingest, kind, product, price });
            }
            for (i, row) in rows.iter().enumerate() {
                eb.eid[i] = row.eid;
                eb.uid[i] = row.uid;
                eb.ts[i] = row.ts;
                eb.ing[i] = row.ingest;
                eb.pp[i] = row.product as u64 | (row.price as u64) << 32 | (row.kind as u64) << 56;
            }
            for i in n..NB {
                eb.ing[i] = TS_BAD;
                eb.eid[i] = r.next();
            }
            let (okb, spb, bad) = unsafe { ev_vec(&mut eb, n, &p) };
            assert_eq!(okb & spb, 0);
            assert_eq!((okb | spb).count_ones() as usize, n, "round {round}");
            let mut any_bad = false;
            for (i, row) in rows.iter().enumerate() {
                assert_eq!(eb.row(i), *row);
                let ing = row.ingest.wrapping_sub(p.ing_base) as u64;
                if row.ingest == TS_BAD || ing >= ING_MAX * 1000 {
                    any_bad = true;
                    continue;
                }
                let ing_s = ing / 1000;
                let rel = row.ts.wrapping_sub(p.ws) as u64;
                let valid = row.uid != ID_EMPTY && rel < p.wlen && row.kind <= 2 && row.product != PROD_EMPTY && (row.kind != 2 || row.price != 0) && row.ingest.wrapping_sub(row.ts) <= DAY_MS;
                let mut w0 = u64::MAX;
                let mut user = 0u64;
                let mut special = false;
                if valid {
                    let mut cat = 0u64;
                    if row.price != 0 {
                        let q = if (row.product as usize) < PBOUND { row.product as usize } else { PBOUND };
                        cat = prod[q] as u64;
                    }
                    let mut alias = 0u64;
                    let mut normal = false;
                    if row.uid < (1 << 40) {
                        let w = unscramble(row.uid, p.ukey);
                        if (w >> 9) < p.ucap && (w & 511) < NALIAS {
                            user = w >> 9;
                            alias = w & 511;
                            normal = true;
                        }
                    }
                    special |= !normal;
                    w0 = rel << 33 | (row.price as u64) << 9 | cat << 4 | alias;
                }
                let mut pushed = None;
                if row.eid < (1 << 40) {
                    let v = unscramble(row.eid, p.ekey);
                    if v < p.ecap {
                        pushed = Some(((v >> BB), w0, (v & (BSIZE as u64 - 1)) | ing_s << 16 | user << 37));
                    }
                }
                special |= pushed.is_none();
                if special {
                    assert!(spb >> i & 1 == 1, "round {round} row {i}: {row:?}");
                    n_sp += 1;
                } else {
                    assert!(okb >> i & 1 == 1, "round {round} row {i}: {row:?}");
                    assert_eq!(Some((eb.ob[i], eb.o0[i], eb.o1[i])), pushed, "round {round} row {i}: {row:?}");
                    n_ok += 1;
                }
            }
            assert_eq!(bad, any_bad, "round {round}");
            n_bad += any_bad as u32;
        }
        assert!(n_ok > 100_000 && n_sp > 10_000 && n_bad > 1000, "{n_ok} {n_sp} {n_bad}");
    }

    #[test]
    fn ck_vec_matches_scalar() {
        use crate::keyrec::{scramble, unscramble};
        let mut r = Rng(0x2545_F491_4F6C_DD1D);
        let mut camp = vec![255u8; CBOUND + 64];
        for x in camp.iter_mut().take(CBOUND) {
            *x = if r.below(4) == 0 { 255 } else { r.below(6) as u8 };
        }
        let mut cb = CkBatch::new();
        let (mut n_ok, mut n_sp, mut n_drop) = (0, 0, 0);
        for round in 0..40_000 {
            let ws = (r.below(1 << 41) as i64) + 8 * DAY_MS;
            let we = ws + ((1 + r.below(24)) * DAY_MS as u64) as i64;
            let p = CkP { cbase: ws - 7 * DAY_MS, clen: (we - ws + 7 * DAY_MS) as u64, ukey: r.next(), ucap: 1 + r.below(1 << 26), ctab: camp.as_ptr() };
            let n = 1 + r.below(64) as usize;
            let mut rows = Vec::new();
            for _ in 0..n {
                let cid = match r.below(8) {
                    0 => ID_ODD | r.below(1 << 40),
                    1 => (1 << 40) + r.below(1 << 42),
                    _ => r.below(1 << 40),
                };
                let uid = match r.below(10) {
                    0 => ID_EMPTY,
                    1 => ID_ODD | r.below(1 << 40),
                    2 => (1 << 40) + r.below(1 << 42),
                    3 => scramble(r.below(p.ucap) * 512 + 14 + r.below(498), p.ukey),
                    4 => scramble((p.ucap + r.below(100)) * 512 + r.below(14), p.ukey),
                    _ => scramble(r.below(p.ucap) * 512 + r.below(14), p.ukey),
                };
                let campaign = match r.below(8) {
                    0 => CAMP_EMPTY,
                    1 => CAMP_ODD,
                    2 => (CBOUND as u64 + r.below(1 << 27)) as u32,
                    _ => r.below(CBOUND as u64) as u32,
                };
                let ts = match r.below(10) {
                    0 => TS_BAD,
                    1 => TS_HUGE,
                    2 => p.cbase - 1 + r.below(3) as i64,
                    3 => we - 2 + r.below(4) as i64,
                    4 => r.next() as i64,
                    _ => p.cbase + r.below(p.clen) as i64,
                };
                rows.push(ClRow { cid, uid, campaign, ts });
            }
            for (i, row) in rows.iter().enumerate() {
                cb.cid[i] = row.cid;
                cb.uid[i] = row.uid;
                cb.ts[i] = row.ts;
                cb.camp[i] = row.campaign as u64;
            }
            for i in n..NB {
                cb.camp[i] = r.below(CBOUND as u64);
                cb.ts[i] = p.cbase;
                cb.uid[i] = r.below(1 << 40);
            }
            let (okb, spb) = unsafe { ck_vec(&mut cb, n, &p) };
            assert_eq!(okb & spb, 0);
            assert_eq!((okb | spb).checked_shr(n as u32).unwrap_or(0), 0);
            for (i, row) in rows.iter().enumerate() {
                let rel = row.ts.wrapping_sub(p.cbase) as u64;
                let keep = !(row.uid == ID_EMPTY || rel >= p.clen || row.campaign as usize >= CBOUND) && camp[row.campaign as usize] != 255;
                if !keep {
                    assert!((okb | spb) >> i & 1 == 0, "round {round} row {i}");
                    n_drop += 1;
                    continue;
                }
                let key = rel << 32 | (row.campaign as u64) << 4;
                let mut pushed = None;
                if row.cid < (1 << 40) && row.uid < (1 << 40) {
                    let w = unscramble(row.uid, p.ukey);
                    if (w >> 9) < p.ucap && (w & 511) < NALIAS {
                        let u = w >> 9;
                        pushed = Some((u >> UB, row.cid | (u & (UBSIZE as u64 - 1)) << 43, key | (w & 511)));
                    }
                }
                match pushed {
                    Some(x) => {
                        assert!(okb >> i & 1 == 1, "round {round} row {i}");
                        assert_eq!((cb.ob[i], cb.o0[i], cb.o1[i]), x);
                        n_ok += 1;
                    }
                    None => {
                        assert!(spb >> i & 1 == 1, "round {round} row {i}");
                        n_sp += 1;
                    }
                }
            }
        }
        assert!(n_ok > 100_000 && n_sp > 10_000 && n_drop > 10_000, "{n_ok} {n_sp} {n_drop}");
    }

    #[test]
    fn push_many_matches_push() {
        let mut r = Rng(0xDEAD_BEEF_1234_5678);
        for &nb in &[1usize, 2, 5, 37, 3000] {
            let a1 = Arena::new(64 << 20);
            let a2 = Arena::new(64 << 20);
            let mut p1 = Part::new(&a1, nb);
            let mut p2 = Part::new(&a2, nb);
            let mut ob = [0u64; 64];
            let mut o0 = [0u64; 64];
            let mut o1 = [0u64; 64];
            for _ in 0..3000 {
                let m = if r.below(3) == 0 { r.next() } else { u64::MAX >> r.below(64) };
                for i in 0..64 {
                    ob[i] = r.below(nb as u64);
                    o0[i] = r.next();
                    o1[i] = r.next();
                }
                let mut mm = m;
                while mm != 0 {
                    let i = mm.trailing_zeros() as usize;
                    mm &= mm - 1;
                    p1.push(ob[i] as usize, o0[i], o1[i]);
                }
                unsafe { p2.push_many(m, ob.as_ptr(), o0.as_ptr(), o1.as_ptr()) };
                let b = r.below(nb as u64) as usize;
                let (x, y) = (r.next(), r.next());
                p1.push(b, x, y);
                p2.push(b, x, y);
            }
            p1.finish();
            p2.finish();
            for b in 0..nb {
                assert_eq!(p1.count(b), p2.count(b));
                let mut v1 = Vec::new();
                let mut v2 = Vec::new();
                p1.for_each(b, |c| v1.extend(c.iter().map(|x| (x.w0, x.w1))));
                p2.for_each(b, |c| v2.extend(c.iter().map(|x| (x.w0, x.w1))));
                assert_eq!(v1, v2, "nb {nb} b {b}");
                assert_eq!(v1.len(), p1.count(b));
            }
        }
    }
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
    let mut eb = EvBatch::new();
    let mut cb = CkBatch::new();
    let tb = tabs(g);
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
        let mut ctx = Ctx { g, t: &mut *t, eb: &mut eb, cb: &mut cb, tabs: tb, tid: tid as u64, base: (f.off + s) as u64, fend: (f.off + f.len) as u64 };
        if unsafe { SKIP } & 512 != 0 {
            let mut x = 0u64;
            let mut i = 0;
            while i < buf.len() {
                x = x.wrapping_add(unsafe { *buf.as_ptr().add(i) } as u64);
                i += 4096;
            }
            t.prof[7] = t.prof[7].wrapping_add(x);
        } else {
            match f.kind {
                Kind::Ev => {
                    if unsafe { SKIP } & 16384 == 0 {
                        <K as Kernel>::events(buf, &mut ctx);
                    }
                    ctx.ev_batch();
                }
                Kind::Ck => {
                    if unsafe { SKIP } & 8192 == 0 {
                        <K as Kernel>::clicks(buf, &mut ctx);
                    }
                    ctx.ck_batch();
                }
                Kind::Lk => {
                    if unsafe { SKIP } & 1024 == 0 {
                        links_chunk(&mut ctx, buf)
                    }
                }
            }
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
