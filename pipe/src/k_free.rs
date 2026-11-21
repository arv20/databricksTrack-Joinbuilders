
use crate::iface::*;
use core::arch::x86_64::*;

#[cfg(not(all(
    target_feature = "avx512bw",
    target_feature = "avx512vl",
    target_feature = "bmi2",
    target_feature = "lzcnt"
)))]
compile_error!("k_free needs -C target-cpu=cascadelake (or newer)");

macro_rules! kernel {
    ($ty:ident, $name:literal, $mode:ident) => {
        pub struct $ty;
        impl Kernel for $ty {
            const NAME: &'static str = $name;
            fn events<S: EventSink>(buf: &[u8], sink: &mut S) {
                unsafe { run_events::<S, $mode>(buf, sink) }
                sink.ev_flush();
            }
            fn clicks<S: ClickSink>(buf: &[u8], sink: &mut S) {
                unsafe { run_clicks::<S, $mode>(buf, sink) }
                sink.ck_flush();
            }
        }
    };
}
kernel!(Free, "free", PIPE);
kernel!(FreeN, "freen", CHAIN);

#[cfg(feature = "stats")]
pub static REASONS: [core::sync::atomic::AtomicU64; 16] = [const { core::sync::atomic::AtomicU64::new(0) }; 16];
#[cfg(feature = "stats")]
pub const REASON_NAMES: [&str; 16] = [
    "ev-careful", "cl-careful", "ids-cold", "ids-malformed", "ts-cold", "ts-malformed", "ingest-cold", "kind-cold",
    "dec-cold", "price-quote", "price-long", "", "ev-slow", "cl-slow", "date-miss", "",
];
#[inline(always)]
fn count(_r: usize) {
    #[cfg(feature = "stats")]
    REASONS[_r].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}


#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct IdShape {
    ctrl: [u8; 16],
    subv: [u8; 16],
    lz: [u8; 16],
    orv: [u64; 2],
}

const fn id_shapes(prefix: u8, dig: usize, chk: usize, allow_empty: bool) -> [IdShape; 128] {
    let mut t = [IdShape { ctrl: [0x80; 16], subv: [0; 16], lz: [0; 16], orv: [0; 2] }; 128];
    let mut raw = 0;
    while raw < 64 {
        let mut q = 0;
        while q < 2 {
            let i = raw * 2 + q;
            let plus = 1 + 2 * q;
            let mut ok = false;
            if raw >= plus + 1 && raw <= plus + 8 {
                let nd = raw - plus;
                let mut j = 8 - nd;
                while j < 8 {
                    t[i].ctrl[dig + j] = (8 - q + j) as u8;
                    t[i].subv[dig + j] = b'0';
                    j += 1;
                }
                t[i].ctrl[chk] = (16 - raw + q) as u8;
                t[i].subv[chk] = prefix;
                if q == 1 {
                    t[i].ctrl[chk + 1] = 15;
                    t[i].subv[chk + 1] = b'"';
                }
                if nd > 1 {
                    t[i].lz[dig + 8 - nd] = 1;
                }
                ok = true;
            } else if allow_empty && raw == 2 * q {
                if q == 1 {
                    t[i].ctrl[chk + 1] = 15;
                    t[i].subv[chk + 1] = b'"';
                }
                t[i].orv[1] = u64::MAX;
                ok = true;
            }
            if !ok {
                t[i].subv[chk] = 1;
            }
            q += 1;
        }
        raw += 1;
    }
    t
}

const fn id_shapes_raw(prefix: u8, dig: usize, chk: usize, allow_empty: bool) -> [IdShape; 64] {
    let both = id_shapes(prefix, dig, chk, allow_empty);
    let mut t = [both[1]; 64];
    let mut raw = 0;
    while raw < 64 {
        let q = if raw == 10 || raw == 11 || (raw == 2 && allow_empty) { 1 } else { 0 };
        t[raw] = both[raw * 2 + q];
        if q == 1 {
            t[raw].ctrl[chk + 2] = (16 - raw) as u8;
            t[raw].subv[chk + 2] = b'"';
        }
        raw += 1;
    }
    t
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct DecShape {
    ctrl: [u8; 16],
    subv: [u8; 16],
    lz: [u8; 16],
    _pad: [u8; 16],
}

const fn dec_shapes(prefix: &[u8]) -> [DecShape; 128] {
    let mut t = [DecShape { ctrl: [0x80; 16], subv: [0; 16], lz: [0; 16], _pad: [0; 16] }; 128];
    let pl = prefix.len();
    let mut raw = 0;
    while raw < 64 {
        let mut q = 0;
        while q < 2 {
            let i = raw * 2 + q;
            let plus = pl + 2 * q;
            if raw >= plus + 1 && raw <= plus + 8 {
                let nd = raw - plus;
                let mut j = 8 - nd;
                while j < 8 {
                    t[i].ctrl[j] = (8 - q + j) as u8;
                    t[i].subv[j] = b'0';
                    j += 1;
                }
                let mut k = 0;
                while k < pl {
                    t[i].ctrl[8 + k] = (16 - raw + q + k) as u8;
                    t[i].subv[8 + k] = prefix[k];
                    k += 1;
                }
                if q == 1 {
                    t[i].ctrl[8 + pl] = 15;
                    t[i].subv[8 + pl] = b'"';
                }
                if nd > 1 {
                    t[i].lz[8 - nd] = 1;
                }
            } else {
                t[i].subv[8] = 1;
            }
            q += 1;
        }
        raw += 1;
    }
    t
}

#[repr(C)]
#[derive(Clone, Copy)]
struct TsA {
    ctrl: [u8; 16],
    base: [u8; 16],
    lim: [u8; 16],
    w1: [i8; 16],
}
#[repr(C)]
#[derive(Clone, Copy)]
struct TsB {
    w2: [i16; 8],
    w3: [i16; 8],
    kmaskv: [u8; 16],
    limv: [i32; 4],
}
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TsC {
    koff: i8,
    soff: i8,
    dmask: i16,
    me: i32,
    mf: [i32; 2],
}

const IDENT: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];
const TSA_INVALID: TsA = TsA { ctrl: IDENT, base: [0; 16], lim: [255; 16], w1: [0; 16] };
const TSB_INVALID: TsB = TsB { w2: [0; 8], w3: [0; 8], kmaskv: [0; 16], limv: [-1, i32::MAX, i32::MAX, i32::MAX] };
const TSC_INVALID: TsC = TsC { koff: -1, soff: -1, dmask: 0, me: 0, mf: [0, 0] };

const fn ts_f12(f2: bool, q: bool) -> (TsA, TsB, TsC) {
    let (mut a, mut b, mut c) = (TSA_INVALID, TSB_INVALID, TSC_INVALID);
    let raw = 20 + 5 * f2 as usize + 2 * q as usize;
    let off = 11 + q as usize;
    let mut i = 0;
    while i < 16 {
        a.ctrl[i] = if off + i < raw { (off + i + 16 - raw) as u8 } else { 0x80 };
        i += 1;
    }
    let bs = *b"00:00:00";
    let l = [2u8, 9, 0, 5, 9, 0, 5, 9];
    let w = [10i8, 1, 0, 10, 1, 0, 10, 1];
    let mut i = 0;
    while i < 8 {
        a.base[i] = bs[i];
        a.lim[i] = l[i];
        a.w1[i] = w[i];
        i += 1;
    }
    let mut next = 9;
    if f2 {
        let b2 = *b"+00:00";
        let l2 = [2u8, 2, 9, 0, 5, 9];
        let w2 = [0i8, 10, 1, 0, 10, 1];
        let mut i = 0;
        while i < 6 {
            a.base[8 + i] = b2[i];
            a.lim[8 + i] = l2[i];
            a.w1[8 + i] = w2[i];
            i += 1;
        }
        next = 14;
        c.mf = [60_000, -60_000];
        c.soff = (off + 8) as i8 - raw as i8;
    } else {
        a.base[8] = b'Z';
        a.lim[8] = 0;
    }
    if q {
        a.base[next] = b'"';
        a.lim[next] = 0;
    }
    b.w2 = [60, 1, 60, 1, 1, 1, 1, 0];
    b.w3 = [60, 1, 60, 1, 0, 0, 0, 0];
    let mut i = if q { 0 } else { 1 };
    while i < 12 {
        b.kmaskv[i] = 0xFF;
        i += 1;
    }
    b.limv = [1430, i32::MAX, 23, i32::MAX];
    c.koff = q as i8 - 1;
    c.dmask = -1;
    c.me = 1000;
    (a, b, c)
}

const fn ts_rows() -> ([TsA; 64], [TsB; 64], [TsC; 64]) {
    let mut ta = [TSA_INVALID; 64];
    let mut tb = [TSB_INVALID; 64];
    let mut tc = [TSC_INVALID; 64];
    let mut n = 1;
    while n <= 16 {
        if n != 15 {
            let (mut a, mut b, mut c) = (TSA_INVALID, TSB_INVALID, TSC_INVALID);
            let mut i = 16 - n;
            while i < 16 {
                a.base[i] = b'0';
                a.lim[i] = 9;
                a.w1[i] = if i % 2 == 0 { 10 } else { 1 };
                i += 1;
            }
            b.w2 = [100, 1, 100, 1, 100, 1, 100, 1];
            b.w3 = [10_000, 1, 10_000, 1, 0, 0, 0, 0];
            b.limv = [i32::MAX; 4];
            c.me = 100_000_000;
            c.mf = [1, 1];
            ta[n] = a;
            tb[n] = b;
            tc[n] = c;
        }
        n += 1;
    }
    let (mut a, mut b, mut c) = (TSA_INVALID, TSB_INVALID, TSC_INVALID);
    a.base[1] = b'"';
    a.lim[1] = 0;
    a.base[15] = b'"';
    a.lim[15] = 0;
    let mut i = 2;
    while i < 15 {
        a.base[i] = b'0';
        a.lim[i] = 9;
        a.w1[i] = if i == 14 { 1 } else if i % 2 == 0 { 10 } else { 1 };
        i += 1;
    }
    b.w2 = [0, 1, 100, 1, 100, 1, 10, 1];
    b.w3 = [10_000, 1, 1000, 1, 0, 0, 0, 0];
    b.limv = [i32::MAX; 4];
    c.me = 10_000_000;
    c.mf = [1, 1];
    ta[15] = a;
    tb[15] = b;
    tc[15] = c;
    let mut k = 0;
    while k < 4 {
        let (f2, q) = (k & 1 == 1, k & 2 == 2);
        let (a, b, c) = ts_f12(f2, q);
        let raw = 20 + 5 * f2 as usize + 2 * q as usize;
        ta[raw] = a;
        tb[raw] = b;
        tc[raw] = c;
        k += 1;
    }
    (ta, tb, tc)
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TjA {
    ctrl: [u8; 32],
    base: [u8; 32],
}
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TjB {
    lim: [u8; 32],
    w1: [i8; 32],
}
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TjC {
    w2: [i16; 16],
    w3: [i16; 16],
}
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TjD {
    limv: [i32; 8],
    kmask2: [u8; 16],
    kmask3: [u8; 16],
}
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TjE {
    koff: i64,
    soff: i64,
    dmask: i64,
    me: i64,
    mf: [i64; 2],
    _pad: [i64; 2],
}

const fn tj_rows() -> ([TjA; 128], [TjB; 128], [TjC; 128], [TjD; 128], [TjE; 128]) {
    let (ta, tb, tc) = ts_rows();
    let mut ja = [TjA { ctrl: [0; 32], base: [0; 32] }; 128];
    let mut jb = [TjB { lim: [0; 32], w1: [0; 32] }; 128];
    let mut jc = [TjC { w2: [0; 16], w3: [0; 16] }; 128];
    let mut jd = [TjD { limv: [0; 8], kmask2: [0; 16], kmask3: [0; 16] }; 128];
    let mut je = [TjE { koff: 0, soff: 0, dmask: 0, me: 0, mf: [0; 2], _pad: [0; 2] }; 128];
    let mut j = 0;
    while j < 128 {
        let rows = [j / 2, 20 + 2 * (j % 2)];
        let mut lane = 0;
        while lane < 2 {
            let (a, b) = (&ta[rows[lane]], &tb[rows[lane]]);
            let mut i = 0;
            while i < 16 {
                ja[j].ctrl[lane * 16 + i] = a.ctrl[i];
                ja[j].base[lane * 16 + i] = a.base[i];
                jb[j].lim[lane * 16 + i] = a.lim[i];
                jb[j].w1[lane * 16 + i] = a.w1[i];
                i += 1;
            }
            let mut i = 0;
            while i < 8 {
                jc[j].w2[lane * 8 + i] = b.w2[i];
                jc[j].w3[lane * 8 + i] = b.w3[i];
                i += 1;
            }
            let mut i = 0;
            while i < 4 {
                jd[j].limv[lane * 4 + i] = b.limv[i];
                i += 1;
            }
            lane += 1;
        }
        jd[j].kmask2 = tb[rows[0]].kmaskv;
        jd[j].kmask3 = tb[rows[1]].kmaskv;
        let c = &tc[rows[0]];
        je[j] = TjE { koff: c.koff as i64, soff: c.soff as i64, dmask: c.dmask as i64, me: c.me as i64, mf: [c.mf[0] as i64, c.mf[1] as i64], _pad: [0; 2] };
        j += 1;
    }
    (ja, jb, jc, jd, je)
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct KindRow {
    cand: [[u8; 16]; 2],
    maskv: [u8; 16],
    k: [u32; 2],
}

const fn kind_row(c0: &[u8], k0: u32, c1: &[u8], k1: u32) -> KindRow {
    let mut r = KindRow { cand: [[0; 16]; 2], maskv: [0; 16], k: [k0, k1] };
    let mut i = 0;
    while i < c0.len() {
        r.cand[0][i] = c0[i];
        r.cand[1][i] = c1[i];
        r.maskv[i] = 0xFF;
        i += 1;
    }
    r
}

const fn kind_rows() -> [KindRow; 64] {
    let mut t = [KindRow { cand: [[b'\n'; 16]; 2], maskv: [0xFF; 16], k: [3, 3] }; 64];
    t[4] = kind_row(b"cart", 1, b"view", 0);
    t[6] = kind_row(b"\"cart\"", 1, b"\"view\"", 0);
    t[8] = kind_row(b"purchase", 2, b"purchase", 2);
    t[10] = kind_row(b"\"purchase\"", 2, b"\"purchase\"", 2);
    t
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct KindRow2 {
    cand: [u8; 16],
    maskv: [u8; 16],
    pp: u64,
    _pad: [u64; 3],
}

const fn kind_rows2() -> [KindRow2; 128] {
    let k = kind_rows();
    let mut t = [KindRow2 { cand: [b'\n'; 16], maskv: [0xFF; 16], pp: 0, _pad: [0; 3] }; 128];
    let mut i = 0;
    while i < 128 {
        let (raw, f) = (i / 2, i % 2);
        t[i].cand = k[raw].cand[f];
        t[i].maskv = k[raw].maskv;
        t[i].pp = (k[raw].k[f] as u64) << 56;
        i += 1;
    }
    t
}

const fn qt_rows() -> [u64; 64] {
    let mut t = [0u64; 64];
    let mut r = 1;
    while r < 64 {
        t[r] = if r == 1 { 3 } else { 1 | 1 << (r - 1) };
        r += 1;
    }
    t
}

const fn pe_rows() -> [u64; 128] {
    let mut t = [PP_SOME; 128];
    t[0] = PP_NONE;
    t[2 * 2 + 1] = PP_NONE;
    t
}

#[repr(C, align(64))]
struct Tables {
    tsa: [TsA; 64],
    tsb: [TsB; 64],
    tsc: [TsC; 64],
    tja: [TjA; 128],
    tjb: [TjB; 128],
    tjc: [TjC; 128],
    tjd: [TjD; 128],
    tje: [TjE; 128],
    kind: [KindRow; 64],
    kind2: [KindRow2; 128],
    qt: [u64; 64],
    pe: [u64; 128],
    eid: [IdShape; 128],
    kid: [IdShape; 128],
    uid: [IdShape; 128],
    eid1: [IdShape; 64],
    kid1: [IdShape; 64],
    uid1: [IdShape; 64],
    prod: [DecShape; 128],
    camp: [DecShape; 128],
}

static T: Tables = Tables {
    tsa: ts_rows().0,
    tsb: ts_rows().1,
    tsc: ts_rows().2,
    tja: tj_rows().0,
    tjb: tj_rows().1,
    tjc: tj_rows().2,
    tjd: tj_rows().3,
    tje: tj_rows().4,
    kind: kind_rows(),
    kind2: kind_rows2(),
    qt: qt_rows(),
    pe: pe_rows(),
    eid: id_shapes(b'e', 0, 8, false),
    kid: id_shapes(b'k', 0, 8, false),
    uid: id_shapes(b'u', 8, 0, true),
    eid1: id_shapes_raw(b'e', 0, 8, false),
    kid1: id_shapes_raw(b'k', 0, 8, false),
    uid1: id_shapes_raw(b'u', 8, 0, true),
    prod: dec_shapes(b"p"),
    camp: dec_shapes(b"c_"),
};

#[repr(C, align(32))]
#[derive(Clone, Copy)]
struct DateEnt {
    key: [u8; 16],
    ms: i64,
    strikes: u64,
}

const _: () = assert!(SCRATCH * 8 == 64 * core::mem::size_of::<DateEnt>() && SCRATCH_INIT == u64::from_le_bytes([b','; 8]));
const _: () = assert!(core::mem::offset_of!(EvBatch, scratch) % 32 == 0 && core::mem::offset_of!(CkBatch, scratch) % 32 == 0);

#[inline(always)]
fn date_idx(dd: u16) -> usize {
    unsafe { _pext_u64(dd as u64, 0x0F03) as usize }
}

#[cold]
#[inline(never)]
unsafe fn date_miss(kp: *const u8, quoted: bool, cache: *mut DateEnt) -> Option<i64> {
    count(14);
    unsafe {
        let k = core::slice::from_raw_parts(kp, 12);
        if quoted && k[0] != b'"' {
            return None;
        }
        let dig = |i: usize| -> Option<i64> { if k[i].is_ascii_digit() { Some((k[i] - b'0') as i64) } else { None } };
        if k[5] != b'-' || k[8] != b'-' || k[11] != b'T' {
            return None;
        }
        let (Some(y0), Some(y1), Some(y2), Some(y3), Some(m0), Some(m1), Some(d0), Some(d1)) =
            (dig(1), dig(2), dig(3), dig(4), dig(6), dig(7), dig(9), dig(10))
        else {
            return None;
        };
        let (y, m, d) = (y0 * 1000 + y1 * 100 + y2 * 10 + y3, m0 * 10 + m1, d0 * 10 + d1);
        if !(1..=12).contains(&m) || d < 1 || d > slow::days_in_month(y, m) {
            return None;
        }
        let ms = slow::days_from_civil(y, m, d) * 86_400_000;
        let ent = &mut *cache.add(date_idx(u16::from_le_bytes([k[9], k[10]])));
        if ent.key[1] == b',' || ent.strikes >= 3 {
            ent.key = [0; 16];
            ent.key[0] = b'"';
            ent.key[1..12].copy_from_slice(&k[1..12]);
            ent.ms = ms;
            ent.strikes = 0;
        } else if ent.key[1..12] != k[1..12] {
            ent.strikes += 1;
        }
        Some(ms)
    }
}


#[inline(always)]
unsafe fn ld(p: *const u8) -> __m128i {
    unsafe { _mm_loadu_si128(p as *const __m128i) }
}
#[inline(always)]
unsafe fn rd16(p: *const u8) -> u16 {
    unsafe { core::ptr::read_unaligned(p as *const u16) }
}
#[inline(always)]
unsafe fn rd64(p: *const u8) -> u64 {
    unsafe { core::ptr::read_unaligned(p as *const u64) }
}
#[inline(always)]
unsafe fn store_hi<const OFF: usize>(p: *mut i64, v: __m128i) {
    unsafe { core::arch::asm!("vmovhps qword ptr [{p} + {off}], {v}", p = in(reg) p, off = const OFF, v = in(xmm_reg) v, options(nostack, preserves_flags)) };
}

#[inline(always)]
fn nz(v: __m128i) -> bool {
    unsafe { _mm_cvtsi128_si64(_mm_or_si128(v, _mm_unpackhi_epi64(v, v))) != 0 }
}

struct Masks {
    c: u64,
    q: u64,
    n: u64,
}

#[inline(always)]
unsafe fn masks(p: *const u8) -> Masks {
    unsafe {
        let v = _mm512_loadu_si512(p as *const __m512i);
        Masks {
            c: _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b',' as i8)),
            q: _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'"' as i8)),
            n: _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'\n' as i8)),
        }
    }
}

#[inline(always)]
unsafe fn ts_date(rs: *const u8, raw: usize, cache: *mut DateEnt) -> (i64, __m128i, *const u8) {
    unsafe {
        let (b, c) = (T.tsb.get_unchecked(raw), T.tsc.get_unchecked(raw));
        let kp = rs.wrapping_offset(c.koff as isize);
        let ent = cache.add(date_idx(rd16(kp.wrapping_add(9))));
        let keybad = _mm_and_si128(_mm_xor_si128(ld(kp), ld(ent as *const u8)), ld(b.kmaskv.as_ptr()));
        ((*ent).ms, keybad, kp)
    }
}

#[inline(always)]
unsafe fn ts_time<const FULL: bool>(rend: *const u8, raw: usize, day_ms: i64) -> (i64, __m128i) {
    unsafe {
        let (a, b, c) = (T.tsa.get_unchecked(raw), T.tsb.get_unchecked(raw), T.tsc.get_unchecked(raw));
        let lane = _mm_shuffle_epi8(ld(rend.wrapping_sub(16)), ld(a.ctrl.as_ptr()));
        let d = _mm_sub_epi8(lane, ld(a.base.as_ptr()));
        let m1 = _mm_maddubs_epi16(d, ld(a.w1.as_ptr() as *const u8));
        let m2 = _mm_madd_epi16(m1, ld(b.w2.as_ptr() as *const u8));
        let m3 = _mm_madd_epi16(_mm_packus_epi32(m2, m2), ld(b.w3.as_ptr() as *const u8));
        let bad = _mm_or_si128(_mm_subs_epu8(d, ld(a.lim.as_ptr())), _mm_cmpgt_epi32(m2, ld(b.limv.as_ptr() as *const u8)));
        let r = _mm_cvtsi128_si64(m3) as u64;
        let val = if FULL {
            let s = *rend.wrapping_offset(c.soff as isize);
            let mf = *c.mf.get_unchecked((s >> 1 & 1) as usize);
            (day_ms & c.dmask as i64) + (r as u32) as i64 * c.me as i64 + (r >> 32) as i64 * mf as i64
        } else {
            day_ms + (r as u32) as i64 * 1000
        };
        (val, bad)
    }
}

#[inline(always)]
unsafe fn ids2(xa: __m128i, sa: &IdShape, xb: __m128i, sb: &IdShape) -> (u64, u64, __m128i) {
    unsafe {
        let (ids, bad) = ids2v(xa, sa, xb, sb);
        (_mm_cvtsi128_si64(ids) as u64, _mm_extract_epi64::<1>(ids) as u64, bad)
    }
}

#[inline(always)]
unsafe fn ids2v(xa: __m128i, sa: &IdShape, xb: __m128i, sb: &IdShape) -> (__m128i, __m128i) {
    unsafe {
        let va = _mm_sub_epi8(_mm_shuffle_epi8(xa, ld(sa.ctrl.as_ptr())), ld(sa.subv.as_ptr()));
        let vb = _mm_sub_epi8(_mm_shuffle_epi8(xb, ld(sb.ctrl.as_ptr())), ld(sb.subv.as_ptr()));
        let d = _mm_blend_epi32::<0b1100>(va, vb);
        let chk = _mm_blend_epi32::<0b1100>(vb, va);
        let e = _mm_sub_epi8(d, _mm_set1_epi8(49));
        let notdig = _mm_subs_epu8(d, _mm_set1_epi8(9));
        let notlet = _mm_subs_epu8(e, _mm_set1_epi8(25));
        let val = _mm_min_epu8(d, _mm_sub_epi8(d, _mm_set1_epi8(39)));
        let lz = _mm_subs_epu8(_mm_or_si128(ld(sa.lz.as_ptr()), ld(sb.lz.as_ptr())), val);
        let bad = _mm_or_si128(_mm_or_si128(chk, lz), _mm_min_epu8(notdig, notlet));
        let m1 = _mm_maddubs_epi16(val, _mm_set1_epi16(0x0124));
        let m2 = _mm_madd_epi16(m1, _mm_set1_epi32(0x0001_0510));
        let ids = _mm_add_epi64(_mm_mul_epu32(m2, _mm_set1_epi64x(1_679_616)), _mm_srli_epi64::<32>(m2));
        (_mm_or_si128(ids, ld(sb.orv.as_ptr() as *const u8)), bad)
    }
}

#[inline(always)]
unsafe fn dec(x: __m128i, s: &DecShape) -> (u32, __m128i) {
    unsafe {
        let v = _mm_sub_epi8(_mm_shuffle_epi8(x, ld(s.ctrl.as_ptr())), ld(s.subv.as_ptr()));
        let lz = _mm_subs_epu8(ld(s.lz.as_ptr()), v);
        let bad = _mm_or_si128(lz, _mm_subs_epu8(v, _mm_set_epi64x(0, 0x0909_0909_0909_0909)));
        let m1 = _mm_maddubs_epi16(v, _mm_set_epi64x(0, 0x010A_010A_010A_010A));
        let m2 = _mm_madd_epi16(m1, _mm_set_epi64x(0, 0x0001_0064_0001_0064));
        let m3 = _mm_madd_epi16(_mm_packus_epi32(m2, m2), _mm_set_epi64x(0, 0x0001_2710));
        (_mm_cvtsi128_si32(m3) as u32, bad)
    }
}

#[inline(always)]
unsafe fn kind_of(rs: *const u8, raw: usize) -> (u32, __m128i) {
    unsafe {
        let kr = T.kind.get_unchecked(raw);
        let f = (*rs.wrapping_add(3) & 1) as usize;
        let bad = _mm_and_si128(_mm_xor_si128(ld(rs), ld(kr.cand.get_unchecked(f).as_ptr())), ld(kr.maskv.as_ptr()));
        (*kr.k.get_unchecked(f), bad)
    }
}

#[inline(always)]
unsafe fn price(end: *const u8, len: usize) -> Option<u32> {
    unsafe {
        if len == 0 {
            return Some(0);
        }
        if len > 8 {
            return None;
        }
        let keep = !0u64 << (8 * (8 - len));
        let y = (rd64(end.wrapping_sub(8)) ^ 0x3030_3030_3030_3030) & keep;
        if (y | y.wrapping_add(0x0606_0606_0606_0606)) & 0xF0F0_F0F0_F0F0_F0F0 != 0 {
            return Some(0);
        }
        let y = y.wrapping_mul(2561) >> 8 & 0x00FF_00FF_00FF_00FF;
        let y = y.wrapping_mul(6_553_601) >> 16 & 0x0000_FFFF_0000_FFFF;
        let v = (y.wrapping_mul(42_949_672_960_001) >> 32) as u32;
        Some(if v.wrapping_sub(1) < 10_000_000 { v } else { 0 })
    }
}


#[cold]
#[inline(never)]
unsafe fn cold_ids(base: *const u8, len: usize, a0: usize, b0: usize, prefix: u8, a1: usize, b1: usize) -> Option<(u64, u64)> {
    count(2);
    let buf = unsafe { core::slice::from_raw_parts(base, len) };
    let (Some(v0), Some(v1)) = (slow::field_value(buf, a0, b0), slow::field_value(buf, a1, b1)) else {
        count(3);
        return None;
    };
    if v0.0 == v0.1 {
        return None;
    }
    Some((slow::parse_id(buf, v0, prefix), slow::parse_id(buf, v1, b'u')))
}

#[cold]
#[inline(never)]
unsafe fn cold_ts(base: *const u8, len: usize, a: usize, b: usize, f1_only: bool) -> Option<i64> {
    count(if f1_only { 6 } else { 4 });
    let buf = unsafe { core::slice::from_raw_parts(base, len) };
    let Some((s, e)) = slow::field_value(buf, a, b) else {
        count(5);
        return None;
    };
    Some(if f1_only { slow::parse_f1(&buf[s..e]) } else { slow::parse_ts(&buf[s..e]) })
}

#[cold]
#[inline(never)]
unsafe fn cold_kind(base: *const u8, len: usize, a: usize, b: usize) -> Option<u32> {
    count(7);
    let buf = unsafe { core::slice::from_raw_parts(base, len) };
    let (s, e) = slow::field_value(buf, a, b)?;
    Some(slow::parse_kind(&buf[s..e]) as u32)
}

#[cold]
#[inline(never)]
unsafe fn cold_dec(base: *const u8, len: usize, a: usize, b: usize, product: bool) -> Option<u32> {
    count(8);
    let buf = unsafe { core::slice::from_raw_parts(base, len) };
    let (s, e) = slow::field_value(buf, a, b)?;
    Some(if product { slow::parse_product(&buf[s..e]) } else { slow::parse_campaign(&buf[s..e]) })
}


const FAIL: usize = usize::MAX;


const OB: usize = 40;

#[inline(always)]
unsafe fn ts_careful<const FULL: bool>(rs: *const u8, raw: usize, cache: *mut DateEnt) -> Option<i64> {
    unsafe {
        let (mut day, keybad, kp) = ts_date(rs, raw, cache);
        if nz(keybad) {
            day = date_miss(kp, T.tsb.get_unchecked(raw).kmaskv[0] != 0, cache)?;
        }
        let (v, bad) = ts_time::<FULL>(rs.wrapping_add(raw), raw, day);
        if nz(bad) { None } else { Some(v) }
    }
}

macro_rules! or_cold {
    ($fast:expr, $cold:expr) => {
        match $fast {
            Some(v) => v,
            None => match $cold {
                Some(v) => v,
                None => return FAIL,
            },
        }
    };
}

#[inline(always)]
unsafe fn ev_core<S: EventSink, const CHAIN: bool>(
    base: *const u8,
    len: usize,
    l: usize,
    e_in: usize,
    cache: *mut DateEnt,
    sink: &mut S,
) -> usize {
    unsafe {
        let p = base.add(l);
        let lb = l + OB;
        let pb = p.wrapping_add(OB);
        let w1 = masks(p);
        let wb = masks(pb);
        let c0 = _tzcnt_u64(w1.c);
        let t = _blsr_u64(w1.c);
        let c1 = _tzcnt_u64(t);
        let c2 = _tzcnt_u64(_blsr_u64(t));
        let m = if CHAIN { _tzcnt_u64(wb.n) } else { e_in.wrapping_sub(lb) as u64 };
        let tb = _bzhi_u64(wb.c, m as u32);
        let c5 = 63 ^ _lzcnt_u64(tb);
        let t = _bzhi_u64(tb, c5 as u32);
        let c4 = 63 ^ _lzcnt_u64(t);
        let c3 = 63 ^ _lzcnt_u64(_bzhi_u64(t, c4 as u32));
        let head = (w1.c << (64 - OB)).count_ones() as u64;
        let tail = tb.count_ones() as u64;
        let nl_head = if CHAIN { w1.n << (64 - OB) } else { 0 };
        if (m > 63) | (nl_head != 0) | (head + tail != 6) | (tail < 3) | (c2 > 62) {
            return FAIL;
        }
        let (c0, c1, c2, c3, c4, c5, m) = (c0 as usize, c1 as usize, c2 as usize, c3 as usize, c4 as usize, c5 as usize, m as usize);
        let e = lb + m;
        let raw6 = m - c5 - 1;
        let pq = _bzhi_u64(wb.q >> (c5 + 1), raw6 as u32);
        let qb = pq & 1;
        if (CHAIN && e >= len) | (pq != qb + qb.wrapping_shl((raw6 as u32).wrapping_sub(1))) {
            return FAIL;
        }
        let (rs2, raw2) = (p.wrapping_add(c1 + 1), c2 - c1 - 1);
        let (rs3, raw3) = (p.wrapping_add(c2 + 1), (OB + c3).wrapping_sub(c2 + 1));
        let row3 = 20 + 2 * (w1.q >> (c2 + 1) & 1) as usize;
        let (rs4, raw4) = (pb.wrapping_add(c3 + 1), c4 - c3 - 1);
        let se = T.eid.get_unchecked(c0 * 2 + (w1.q & 1) as usize);
        let su = T.uid.get_unchecked((c1 - c0 - 1) * 2 + (w1.q >> (c0 + 1) & 1) as usize);
        let sp = T.prod.get_unchecked((c5 - c4 - 1) * 2 + (wb.q >> (c4 + 1) & 1) as usize);
        let xe = ld(p.wrapping_add(c0).wrapping_sub(16));
        let xu = ld(p.wrapping_add(c1).wrapping_sub(16));
        let xp = ld(pb.wrapping_add(c5).wrapping_sub(16));

        let (eid, uid, ts, ingest, kind, product);
        {
            count(0);
            (eid, uid) = {
                let (a, b, bad) = ids2(xe, se, xu, su);
                or_cold!(if nz(bad) { None } else { Some((a, b)) }, cold_ids(base, len, l, l + c0, b'e', l + c0 + 1, l + c1))
            };
            ts = or_cold!(ts_careful::<true>(rs2, raw2, cache), cold_ts(base, len, l + c1 + 1, l + c2, false));
            ingest = or_cold!(
                if raw3 == row3 { ts_careful::<false>(rs3, row3, cache) } else { None },
                cold_ts(base, len, l + c2 + 1, lb + c3, true)
            );
            kind = {
                let (k, bad) = kind_of(rs4, raw4);
                or_cold!(if nz(bad) { None } else { Some(k) }, cold_kind(base, len, lb + c3 + 1, lb + c4))
            };
            product = {
                let (v, bad) = dec(xp, sp);
                or_cold!(if nz(bad) { None } else { Some(v) }, cold_dec(base, len, lb + c4 + 1, lb + c5, true))
            };
        }
        let mut pr = 0;
        if kind == 2 {
            match price(pb.wrapping_add(m - qb as usize), raw6 - 2 * qb as usize) {
                Some(v) => pr = v,
                None => return FAIL,
            }
        }
        sink.event(&EvRow { eid, uid, ts, ingest, kind: kind as u8, product, price: pr });
        e
    }
}

#[derive(Clone, Copy)]
struct EvMasks {
    w1c: u64,
    wbc: u64,
    wbq: u64,
    m: u64,
}

#[inline(always)]
unsafe fn ev_masks(p: *const u8) -> EvMasks {
    unsafe {
        let z1 = _mm512_loadu_si512(p as *const __m512i);
        let zb = _mm512_loadu_si512(p.wrapping_add(OB) as *const __m512i);
        let (comma, quote, newline) = (_mm512_set1_epi8(b',' as i8), _mm512_set1_epi8(b'"' as i8), _mm512_set1_epi8(b'\n' as i8));
        EvMasks {
            w1c: _mm512_cmpeq_epi8_mask(z1, comma),
            wbc: _mm512_cmpeq_epi8_mask(zb, comma),
            wbq: _mm512_cmpeq_epi8_mask(zb, quote),
            m: _tzcnt_u64(_mm512_cmpeq_epi8_mask(zb, newline)),
        }
    }
}
impl EvMasks {
    #[inline(always)]
    fn next(&self, p: *const u8) -> *const u8 {
        p.wrapping_add(OB + 1).wrapping_add(self.m as usize)
    }
    #[inline(always)]
    fn here(self) -> EvMasks {
        EvMasks { w1c: here_k(self.w1c), wbc: here_k(self.wbc), wbq: here_k(self.wbq), m: here(self.m) }
    }
}

#[cold]
#[inline(never)]
unsafe fn ev_cold<S: EventSink>(base: *const u8, len: usize, l: usize, cache: *mut DateEnt, sink: &mut S) -> usize {
    unsafe {
        let r = ev_core::<S, true>(base, len, l, 0, cache, sink);
        if r != FAIL { r + 1 } else { scan_event(core::slice::from_raw_parts(base, len), l, sink) }
    }
}

#[cold]
#[inline(never)]
unsafe fn fix_ts(base: *const u8, len: usize, a: usize, b: usize, cache: *mut DateEnt) -> Option<i64> {
    unsafe {
        if core::slice::from_raw_parts(base.add(a), b - a).contains(&b'\n') {
            return None;
        }
        match ts_careful::<true>(base.add(a), b - a, cache) {
            Some(v) => Some(v),
            None => cold_ts(base, len, a, b, false),
        }
    }
}

#[inline(always)]
unsafe fn ev_fast<S: EventSink>(
    base: *const u8,
    end: *const u8,
    p: *const u8,
    k: EvMasks,
    np: *const u8,
    b: *mut EvBatch,
    w: &mut *mut i64,
    sink: &mut S,
) -> *const u8 {
    unsafe {
        let cache = (*b).scratch.as_mut_ptr() as *mut DateEnt;
        macro_rules! cold {
            () => {{
                (*b).n = (*w).offset_from((*b).ts.as_ptr()) as usize;
                let r = ev_cold(base, end.offset_from(base) as usize, p.offset_from(base) as usize, cache, sink);
                *w = (*b).ts.as_mut_ptr().add((*b).n);
                return base.add(r);
            }};
        }
        let EvMasks { w1c, wbc, wbq, m } = k;
        let pb = p.wrapping_add(OB);
        let pe = pb.wrapping_add(m as usize);
        let c0 = _tzcnt_u64(w1c);
        let t = _blsr_u64(w1c);
        let c1 = _tzcnt_u64(t);
        let c2 = _tzcnt_u64(_blsr_u64(t));
        let tb = _bzhi_u64(wbc, m as u32);
        let z = _lzcnt_u64(tb);
        let r6 = m.wrapping_add(z).wrapping_sub(64);
        let t = tb.wrapping_shl(z as u32) << 1;
        let r5 = _lzcnt_u64(t);
        let t = t.wrapping_shl(r5 as u32) << 1;
        let r4 = _lzcnt_u64(t);
        let i6 = m.wrapping_sub(r6);
        let i5 = i6.wrapping_sub(r5).wrapping_sub(1);
        let i4 = i5.wrapping_sub(r4).wrapping_sub(1);
        let raw3 = (OB as u64 - 2).wrapping_add(i4).wrapping_sub(c2);
        if (m > 63) | (pe >= end) | (c2 > 62) | (r4 > 63) | (raw3 | 2 != 22) {
            cold!();
        }
        let q6 = _bzhi_u64(wbq.wrapping_shr(i6 as u32), r6 as u32);
        let q5 = _bzhi_u64(wbq.wrapping_shr(i5 as u32), r5 as u32);
        if q6.wrapping_mul(q6 ^ *T.qt.get_unchecked(r6 as usize)) | q5.wrapping_mul(q5 ^ *T.qt.get_unchecked(r5 as usize)) != 0 {
            cold!();
        }
        let (c0, c1, c2, r4, r5, r6) = (c0 as usize, c1 as usize, c2 as usize, r4 as usize, r5 as usize, r6 as usize);
        let qb5 = (q5 & 1) as usize;
        let (pp, bad_x) = if r4 >= 8 {
            let qb = (q6 & 1) as usize;
            let Some(pr) = price(pe.wrapping_sub(qb), r6 - 2 * qb) else { cold!() };
            let (prod, bad_p) = dec(ld(pb.wrapping_add(i6 as usize).wrapping_sub(17)), T.prod.get_unchecked(r5 * 2 + qb5));
            (prod as u64 | (pr as u64) << 32 | if pr == 0 { PP_INVALID } else { 0 }, bad_p)
        } else {
            (*T.pe.get_unchecked(r5 * 2 + qb5), _mm_setzero_si128())
        };
        let (se, su) = (T.eid1.get_unchecked(c0), T.uid1.get_unchecked(c1 - c0 - 1));
        let (ids, bad_i) = ids2v(ld(p.wrapping_add(c0).wrapping_sub(16)), se, ld(p.wrapping_add(c1).wrapping_sub(16)), su);
        let rs4 = pb.wrapping_add(i4 as usize);
        let kr = T.kind2.get_unchecked(r4 * 2 + (*rs4.wrapping_add(3) & 1) as usize);
        let bad_kind = _mm_and_si128(_mm_xor_si128(ld(rs4), ld(kr.cand.as_ptr())), ld(kr.maskv.as_ptr()));
        let q3 = (raw3 >> 1 & 1) as usize;
        let j = here(((c2 - c1 - 1) * 2 + q3) as u64) as usize;
        let (ja, jb, jc, jd, je) = (T.tja.get_unchecked(j), T.tjb.get_unchecked(j), T.tjc.get_unchecked(j), T.tjd.get_unchecked(j), T.tje.get_unchecked(j));
        let e2 = p.wrapping_add(c2);
        let kp2 = p.wrapping_add(c1 + 1).wrapping_offset(je.koff as isize);
        let ent2 = cache.add(date_idx(rd16(kp2.wrapping_add(9))));
        let bad_k2 = _mm_and_si128(_mm_xor_si128(ld(kp2), ld(ent2 as *const u8)), ld(jd.kmask2.as_ptr()));
        let kp3 = e2.wrapping_add(q3);
        let ent3 = cache.add(date_idx(rd16(kp3.wrapping_add(9))));
        let bad_k3 = _mm_and_si128(_mm_xor_si128(ld(kp3), ld(ent3 as *const u8)), ld(jd.kmask3.as_ptr()));
        let ld2 = |q: *const u8| _mm256_load_si256(q as *const __m256i);
        let x = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(ld(e2.wrapping_sub(16))), ld(rs4.wrapping_sub(17)));
        let d = _mm256_sub_epi8(_mm256_shuffle_epi8(x, ld2(ja.ctrl.as_ptr())), ld2(ja.base.as_ptr()));
        let m1 = _mm256_maddubs_epi16(d, ld2(jb.w1.as_ptr() as *const u8));
        let m2 = _mm256_madd_epi16(m1, ld2(jc.w2.as_ptr() as *const u8));
        let m3 = _mm256_madd_epi16(_mm256_packus_epi32(m2, m2), ld2(jc.w3.as_ptr() as *const u8));
        let bad_tt = _mm256_or_si256(_mm256_subs_epu8(d, ld2(jb.lim.as_ptr())), _mm256_cmpgt_epi32(m2, ld2(jd.limv.as_ptr() as *const u8)));
        let r = _mm_cvtsi128_si64(_mm256_castsi256_si128(m3)) as u64;
        let sg = *e2.wrapping_offset(je.soff as isize);
        let ts = ((*ent2).ms & je.dmask) + (r as u32) as i64 * je.me + (r >> 32) as i64 * *je.mf.get_unchecked((sg >> 1 & 1) as usize);
        let ingest = (*ent3).ms + (_mm_cvtsi128_si32(_mm256_extracti128_si256::<1>(m3)) as u32) as i64 * 1000;
        let bad = _mm_or_si128(_mm_or_si128(bad_i, bad_x), _mm_or_si128(bad_k3, bad_kind));
        let all = _mm256_or_si256(bad_tt, _mm256_zextsi128_si256(_mm_or_si128(bad, bad_k2)));
        let s = *w;
        const A: usize = BATCH_ALIGN;
        *s = ts;
        *s.byte_add(core::mem::offset_of!(EvBatch, ing)) = ingest;
        *(s.byte_add(core::mem::offset_of!(EvBatch, pp)) as *mut u64) = pp | kr.pp;
        _mm_storel_epi64(s.byte_add(core::mem::offset_of!(EvBatch, eid)) as *mut __m128i, ids);
        store_hi::<{ core::mem::offset_of!(EvBatch, uid) }>(s, ids);
        if _mm256_testz_si256(all, all) == 0 {
            if nz(_mm_or_si128(bad, _mm256_extracti128_si256::<1>(bad_tt))) {
                cold!();
            }
            let l = p.offset_from(base) as usize;
            match fix_ts(base, end.offset_from(base) as usize, l + c1 + 1, l + c2, cache) {
                Some(v) => *s = v,
                None => cold!(),
            }
        }
        *w = s.add(1);
        if (s.add(1) as usize) & (A - 1) == 0 {
            (*b).n = NB;
            sink.ev_flush();
            *w = (*b).ts.as_mut_ptr();
        }
        np
    }
}

#[derive(Clone, Copy)]
struct ClMasks {
    c: u64,
    q: u64,
}

#[inline(always)]
unsafe fn cl_masks(p: *const u8) -> (ClMasks, u64) {
    unsafe {
        let w = masks(p);
        (ClMasks { c: w.c, q: w.q }, _tzcnt_u64(w.n))
    }
}

#[inline(always)]
unsafe fn cl_core<S: ClickSink, const CHAIN: bool, const CAREFUL: bool>(
    base: *const u8,
    len: usize,
    l: usize,
    e_in: usize,
    w: &ClMasks,
    n_in: u64,
    cache: *mut DateEnt,
    sink: &mut S,
) -> usize {
    unsafe {
        let p = base.add(l);
        let c0 = _tzcnt_u64(w.c);
        let t = _blsr_u64(w.c);
        let c1 = _tzcnt_u64(t);
        let t = _blsr_u64(t);
        let c2 = _tzcnt_u64(t);
        let t = _blsr_u64(t);
        let n = if CHAIN { n_in } else { (e_in - l) as u64 };
        if (n > if CHAIN { 63 } else { 64 }) | (c2 >= n) | (_bzhi_u64(t, n as u32) != 0) {
            return FAIL;
        }
        let (c0, c1, c2, n) = (c0 as usize, c1 as usize, c2 as usize, n as usize);
        let e = l + n;
        let (sk, su) = if CAREFUL {
            (T.kid.get_unchecked(c0 * 2 + (w.q & 1) as usize), T.uid.get_unchecked((c1 - c0 - 1) * 2 + (w.q >> (c0 + 1) & 1) as usize))
        } else {
            (T.kid1.get_unchecked(c0), T.uid1.get_unchecked(c1 - c0 - 1))
        };
        let sc = T.camp.get_unchecked((c2 - c1 - 1) * 2 + (w.q >> (c1 + 1) & 1) as usize);
        let xk = ld(p.wrapping_add(c0).wrapping_sub(16));
        let xu = ld(p.wrapping_add(c1).wrapping_sub(16));
        let xc = ld(p.wrapping_add(c2).wrapping_sub(16));
        let (rs3, raw3) = (p.wrapping_add(c2 + 1), n - c2 - 1);
        let (cid, uid, campaign, ts);
        {
            count(1);
            (cid, uid) = {
                let (a, b, bad) = ids2(xk, sk, xu, su);
                or_cold!(if nz(bad) { None } else { Some((a, b)) }, cold_ids(base, len, l, l + c0, b'k', l + c0 + 1, l + c1))
            };
            campaign = {
                let (v, bad) = dec(xc, sc);
                or_cold!(if nz(bad) { None } else { Some(v) }, cold_dec(base, len, l + c1 + 1, l + c2, false))
            };
            ts = or_cold!(ts_careful::<true>(rs3, raw3, cache), cold_ts(base, len, l + c2 + 1, e, false));
        }
        sink.click(&ClRow { cid, uid, campaign, ts });
        e
    }
}

#[derive(Clone, Copy)]
struct CkMasks {
    c: u64,
    q: u64,
    m: u64,
}

#[inline(always)]
unsafe fn ck_masks(p: *const u8) -> CkMasks {
    unsafe {
        let w = masks(p);
        CkMasks { c: w.c, q: w.q, m: _tzcnt_u64(w.n) }
    }
}
impl CkMasks {
    #[inline(always)]
    fn next(&self, p: *const u8) -> *const u8 {
        p.wrapping_add(1).wrapping_add(self.m as usize)
    }
    #[inline(always)]
    fn here(self) -> CkMasks {
        CkMasks { c: here(self.c), q: here(self.q), m: here(self.m) }
    }
}

#[cold]
#[inline(never)]
unsafe fn cl_cold<S: ClickSink>(base: *const u8, len: usize, l: usize, cache: *mut DateEnt, sink: &mut S) -> usize {
    unsafe {
        let (w, nl) = cl_masks(base.add(l));
        let r = cl_core::<S, true, true>(base, len, l, 0, &w, nl, cache, sink);
        if r != FAIL { r + 1 } else { scan_click(core::slice::from_raw_parts(base, len), l, sink) }
    }
}

#[inline(always)]
unsafe fn cl_fast<S: ClickSink>(
    base: *const u8,
    end: *const u8,
    p: *const u8,
    k: CkMasks,
    np: *const u8,
    b: *mut CkBatch,
    w: &mut *mut i64,
    sink: &mut S,
) -> *const u8 {
    unsafe {
        let cache = (*b).scratch.as_mut_ptr() as *mut DateEnt;
        macro_rules! cold {
            () => {{
                (*b).n = (*w).offset_from((*b).ts.as_ptr()) as usize;
                let r = cl_cold(base, end.offset_from(base) as usize, p.offset_from(base) as usize, cache, sink);
                *w = (*b).ts.as_mut_ptr().add((*b).n);
                return base.add(r);
            }};
        }
        let CkMasks { c: wc, q: wq, m: nl } = k;
        let c0 = _tzcnt_u64(wc);
        let t = _blsr_u64(wc);
        let c1 = _tzcnt_u64(t);
        let t = _blsr_u64(t);
        let c2 = _tzcnt_u64(t);
        let t = _blsr_u64(t);
        if (nl > 63) | (c2 >= nl) | (_bzhi_u64(t, nl as u32) != 0) {
            cold!();
        }
        let (c0, c1, c2, nl) = (c0 as usize, c1 as usize, c2 as usize, nl as usize);
        let (sk, su) = (T.kid1.get_unchecked(c0), T.uid1.get_unchecked(c1 - c0 - 1));
        let sc = T.camp.get_unchecked((c2 - c1 - 1) * 2 + (wq >> (c1 + 1) & 1) as usize);
        let (ids, bad_id) = ids2v(ld(p.wrapping_add(c0).wrapping_sub(16)), sk, ld(p.wrapping_add(c1).wrapping_sub(16)), su);
        let (cv, bad_c) = dec(ld(p.wrapping_add(c2).wrapping_sub(16)), sc);
        let (rs3, raw3) = (p.wrapping_add(c2 + 1), nl - c2 - 1);
        let (day, bad_k, _) = ts_date(rs3, raw3, cache);
        let (ts, bad_t) = ts_time::<true>(p.wrapping_add(nl), raw3, day);
        let bad = _mm_or_si128(bad_id, bad_c);
        let s = *w;
        const A: usize = BATCH_ALIGN;
        *s = ts;
        *(s.byte_add(core::mem::offset_of!(CkBatch, camp)) as *mut u64) = cv as u64;
        _mm_storel_epi64(s.byte_add(core::mem::offset_of!(CkBatch, cid)) as *mut __m128i, ids);
        store_hi::<{ core::mem::offset_of!(CkBatch, uid) }>(s, ids);
        if nz(_mm_or_si128(bad, _mm_or_si128(bad_k, bad_t))) {
            if nz(bad) {
                cold!();
            }
            let l = p.offset_from(base) as usize;
            match fix_ts(base, end.offset_from(base) as usize, l + c2 + 1, l + nl, cache) {
                Some(v) => *s = v,
                None => cold!(),
            }
        }
        *w = s.add(1);
        if (s.add(1) as usize) & (A - 1) == 0 {
            (*b).n = NB;
            sink.ck_flush();
            *w = (*b).ts.as_mut_ptr();
        }
        np
    }
}

const PIPE: u8 = 0;
const PFD: usize = 512;
const CHAIN: u8 = 1;

#[inline(always)]
fn here(mut v: u64) -> u64 {
    unsafe { core::arch::asm!("/* {0} */", inout(reg) v, options(nomem, nostack, preserves_flags)) };
    v
}
#[inline(always)]
fn here_k(mut v: u64) -> u64 {
    unsafe { core::arch::asm!("/* {0} */", inout(kreg) v, options(nomem, nostack, preserves_flags)) };
    v
}

#[cold]
#[inline(never)]
fn scan_event<S: EventSink>(buf: &[u8], pos: usize, sink: &mut S) -> usize {
    count(12);
    let e = match buf[pos..].iter().position(|&b| b == b'\n') {
        Some(i) => pos + i,
        None => buf.len(),
    };
    slow::event(buf, pos, e, sink);
    e + 1
}
#[cold]
#[inline(never)]
fn scan_click<S: ClickSink>(buf: &[u8], pos: usize, sink: &mut S) -> usize {
    count(13);
    let e = match buf[pos..].iter().position(|&b| b == b'\n') {
        Some(i) => pos + i,
        None => buf.len(),
    };
    slow::click(buf, pos, e, sink);
    e + 1
}

macro_rules! runner {
    ($name:ident, $sink:ident, $get:ident, $line:ident, $masks:ident, $adv:expr, $scan:ident) => {
        #[inline(never)]
        unsafe fn $name<S: $sink, const MODE: u8>(buf: &[u8], sink: &mut S) {
            unsafe {
                let base = buf.as_ptr();
                let len = buf.len();
                let end = base.add(len);
                let b = sink.$get();
                let mut w = (*b).ts.as_mut_ptr().add((*b).n);
                let mut p = base;
                const REACH: usize = 2 * ($adv + 64) + 1;
                if MODE == PIPE && len >= REACH {
                    let lim = end.sub(REACH);
                    'outer: while p <= lim {
                        let mut k = $masks(p);
                        loop {
                            let np = k.next(p);
                            let kn = $masks(np).here();
                            _mm_prefetch::<_MM_HINT_T0>(p.wrapping_add(PFD) as *const i8);
                            let r = $line(base, end, p, k, np, b, &mut w, sink);
                            p = r;
                            if r != np {
                                continue 'outer;
                            }
                            if p > lim {
                                break 'outer;
                            }
                            k = kn;
                        }
                    }
                }
                let mut pos = p.offset_from(base) as usize;
                while pos + 64 <= len {
                    let p = base.add(pos);
                    let k = $masks(p);
                    pos = $line(base, end, p, k, k.next(p), b, &mut w, sink).offset_from(base) as usize;
                }
                (*b).n = w.offset_from((*b).ts.as_ptr()) as usize;
                while pos < len {
                    pos = $scan(buf, pos, sink);
                }
            }
        }
    };
}
runner!(run_events, EventSink, ev, ev_fast, ev_masks, OB, scan_event);
runner!(run_clicks, ClickSink, ck, cl_fast, ck_masks, 0, scan_click);

mod slow {
    use crate::iface::*;

    pub fn field_value(buf: &[u8], s: usize, e: usize) -> Option<(usize, usize)> {
        let raw = &buf[s..e];
        let quotes = raw.iter().filter(|&&b| b == b'"').count();
        if quotes == 0 {
            return Some((s, e));
        }
        if quotes == 2 && raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
            return Some((s + 1, e - 1));
        }
        None
    }

    fn split_line<const N: usize>(buf: &[u8], start: usize, end: usize) -> Option<[(usize, usize); N]> {
        if start == end {
            return None;
        }
        let mut out = [(0usize, 0usize); N];
        let mut count = 0;
        let mut field_start = start;
        let mut i = start;
        loop {
            if i == end || buf[i] == b',' {
                if count == N {
                    return None;
                }
                out[count] = field_value(buf, field_start, i)?;
                count += 1;
                if i == end {
                    break;
                }
                field_start = i + 1;
            }
            i += 1;
        }
        if count != N {
            return None;
        }
        Some(out)
    }

    pub fn parse_id(buf: &[u8], span: (usize, usize), prefix: u8) -> u64 {
        let (s, e) = span;
        let v = &buf[s..e];
        if v.is_empty() {
            return ID_EMPTY;
        }
        let digits = &v[1..];
        let canonical = v[0] == prefix
            && (1..=8).contains(&digits.len())
            && digits.iter().all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
            && !(digits[0] == b'0' && digits.len() > 1);
        if !canonical {
            return odd_id(s, v.len());
        }
        let mut value: u64 = 0;
        for &b in digits {
            let d = if b.is_ascii_digit() { b - b'0' } else { b - b'a' + 10 };
            value = value * 36 + d as u64;
        }
        value
    }

    fn canonical_decimal(d: &[u8]) -> Option<u32> {
        if !(1..=8).contains(&d.len()) || !d.iter().all(|b| b.is_ascii_digit()) {
            return None;
        }
        if d[0] == b'0' && d.len() > 1 {
            return None;
        }
        Some(d.iter().fold(0u32, |a, &b| a * 10 + (b - b'0') as u32))
    }

    pub fn parse_product(v: &[u8]) -> u32 {
        if v.is_empty() {
            return PROD_EMPTY;
        }
        if v[0] != b'p' {
            return PROD_ODD;
        }
        canonical_decimal(&v[1..]).unwrap_or(PROD_ODD)
    }

    pub fn parse_campaign(v: &[u8]) -> u32 {
        if v.is_empty() {
            return CAMP_EMPTY;
        }
        if !v.starts_with(b"c_") {
            return CAMP_ODD;
        }
        canonical_decimal(&v[2..]).unwrap_or(CAMP_ODD)
    }

    pub fn parse_kind(v: &[u8]) -> u8 {
        match v {
            b"view" => KIND_VIEW,
            b"cart" => KIND_CART,
            b"purchase" => KIND_PURCHASE,
            _ => KIND_OTHER,
        }
    }

    fn parse_price(v: &[u8]) -> u32 {
        if v.is_empty() || !v.iter().all(|b| b.is_ascii_digit()) {
            return 0;
        }
        let first_nonzero = v.iter().position(|&b| b != b'0').unwrap_or(v.len());
        let significant = &v[first_nonzero..];
        if significant.len() > 8 {
            return 0;
        }
        let value = significant.iter().fold(0u32, |a, &b| a * 10 + (b - b'0') as u32);
        if (1..=10_000_000).contains(&value) { value } else { 0 }
    }

    fn is_leap(y: i64) -> bool {
        y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
    }

    pub fn days_in_month(y: i64, m: i64) -> i64 {
        match m {
            1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
            4 | 6 | 9 | 11 => 30,
            2 => if is_leap(y) { 29 } else { 28 },
            _ => 0,
        }
    }

    pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
        let y = if m <= 2 { y - 1 } else { y };
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        era * 146_097 + doe - 719_468
    }

    fn digits_value(v: &[u8]) -> Option<i64> {
        if v.iter().all(|b| b.is_ascii_digit()) {
            Some(v.iter().fold(0i64, |a, &b| a * 10 + (b - b'0') as i64))
        } else {
            None
        }
    }

    fn civil_ms(v: &[u8]) -> Option<i64> {
        if v.len() != 19 || v[4] != b'-' || v[7] != b'-' || v[10] != b'T' || v[13] != b':' || v[16] != b':' {
            return None;
        }
        let y = digits_value(&v[0..4])?;
        let mo = digits_value(&v[5..7])?;
        let d = digits_value(&v[8..10])?;
        let h = digits_value(&v[11..13])?;
        let mi = digits_value(&v[14..16])?;
        let s = digits_value(&v[17..19])?;
        if !(1..=12).contains(&mo) || d < 1 || d > days_in_month(y, mo) || h > 23 || mi > 59 || s > 59 {
            return None;
        }
        Some(days_from_civil(y, mo, d) * 86_400_000 + ((h * 60 + mi) * 60 + s) * 1000)
    }

    pub fn parse_f1(v: &[u8]) -> i64 {
        if v.len() == 20 && v[19] == b'Z' {
            if let Some(ms) = civil_ms(&v[..19]) {
                return ms;
            }
        }
        TS_BAD
    }

    pub fn parse_ts(v: &[u8]) -> i64 {
        if !v.is_empty() && v.iter().all(|b| b.is_ascii_digit()) {
            let mut value: u128 = 0;
            for &b in v {
                value = (value * 10 + (b - b'0') as u128).min(u64::MAX as u128);
            }
            return value.min(i64::MAX as u128) as i64;
        }
        if v.len() == 20 {
            return parse_f1(v);
        }
        if v.len() == 25 && (v[19] == b'+' || v[19] == b'-') && v[22] == b':' {
            let Some(local) = civil_ms(&v[..19]) else { return TS_BAD };
            let (Some(oh), Some(om)) = (digits_value(&v[20..22]), digits_value(&v[23..25])) else {
                return TS_BAD;
            };
            if oh > 23 || om > 59 {
                return TS_BAD;
            }
            let offset = (oh * 60 + om) * 60_000;
            return if v[19] == b'+' { local - offset } else { local + offset };
        }
        TS_BAD
    }

    #[cold]
    #[inline(never)]
    pub fn event<S: EventSink>(buf: &[u8], start: usize, end: usize, sink: &mut S) {
        let Some(v) = split_line::<7>(buf, start, end) else { return };
        if v[0].0 == v[0].1 {
            return;
        }
        let val = |i: usize| &buf[v[i].0..v[i].1];
        let kind = parse_kind(val(4));
        sink.event(&EvRow {
            eid: parse_id(buf, v[0], b'e'),
            uid: parse_id(buf, v[1], b'u'),
            ts: parse_ts(val(2)),
            ingest: parse_f1(val(3)),
            kind,
            product: parse_product(val(5)),
            price: if kind == KIND_PURCHASE { parse_price(val(6)) } else { 0 },
        });
    }

    #[cold]
    #[inline(never)]
    pub fn click<S: ClickSink>(buf: &[u8], start: usize, end: usize, sink: &mut S) {
        let Some(v) = split_line::<4>(buf, start, end) else { return };
        if v[0].0 == v[0].1 {
            return;
        }
        let val = |i: usize| &buf[v[i].0..v[i].1];
        sink.click(&ClRow {
            cid: parse_id(buf, v[0], b'k'),
            uid: parse_id(buf, v[1], b'u'),
            campaign: parse_campaign(val(2)),
            ts: parse_ts(val(3)),
        });
    }
}
