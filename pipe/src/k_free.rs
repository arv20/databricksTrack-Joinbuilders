
use crate::iface::*;
use core::arch::x86_64::*;
use core::cell::UnsafeCell;
use core::mem::MaybeUninit;

#[cfg(not(all(
    target_feature = "avx512bw",
    target_feature = "avx512vl",
    target_feature = "bmi2",
    target_feature = "lzcnt"
)))]
compile_error!("k_free needs -C target-cpu=cascadelake (or newer)");

macro_rules! kernel {
    ($ty:ident, $name:literal, $wide:literal, $chain:literal) => {
        pub struct $ty;
        impl Kernel for $ty {
            const NAME: &'static str = $name;
            fn events<S: EventSink>(buf: &[u8], sink: &mut S) {
                unsafe { run_events::<S, $wide, $chain>(buf, sink) }
            }
            fn clicks<S: ClickSink>(buf: &[u8], sink: &mut S) {
                unsafe { run_clicks::<S, $wide, $chain>(buf, sink) }
            }
        }
    };
}
const fn env_u32(s: Option<&str>, d: u32) -> u32 {
    match s {
        None => d,
        Some(s) => {
            let b = s.as_bytes();
            let mut v = 0;
            let mut i = 0;
            while i < b.len() {
                v = v * 10 + (b[i] - b'0') as u32;
                i += 1;
            }
            v
        }
    }
}
const STAGE: u32 = env_u32(option_env!("FREE_STAGE"), 99);

kernel!(Free, "free", true, false);
kernel!(Free256, "free256", false, false);
kernel!(FreeC, "freec", true, true);
kernel!(FreeC256, "freec256", false, true);

#[cfg(feature = "stats")]
pub static REASONS: [core::sync::atomic::AtomicU64; 16] = [const { core::sync::atomic::AtomicU64::new(0) }; 16];
#[cfg(feature = "stats")]
pub const REASON_NAMES: [&str; 16] = [
    "ev-struct1", "ev-struct2", "ids-cold", "ids-malformed", "ts-cold", "ts-malformed", "ingest-cold", "kind-cold",
    "dec-cold", "price-quote", "price-long", "cl-struct", "line-slow-ev", "line-slow-cl", "date-miss", "",
];
#[inline(always)]
fn count(_r: usize) {
    #[cfg(feature = "stats")]
    REASONS[_r].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}


const fn pow(b: u64, e: usize) -> u64 {
    let mut r = 1;
    let mut i = 0;
    while i < e {
        r *= b;
        i += 1;
    }
    r
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct IdShape {
    ctrl: [u8; 16],
    subv: [u8; 16],
    min: u64,
    or: u64,
    _pad: [u8; 16],
}

const fn id_shapes(prefix: u8, dig: usize, chk: usize, allow_empty: bool) -> [IdShape; 128] {
    let mut t = [IdShape { ctrl: [0x80; 16], subv: [0; 16], min: 0, or: 0, _pad: [0; 16] }; 128];
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
                t[i].min = if nd == 1 { 0 } else { pow(36, nd - 1) };
                ok = true;
            } else if allow_empty && raw == 2 * q {
                if q == 1 {
                    t[i].ctrl[chk + 1] = 15;
                    t[i].subv[chk + 1] = b'"';
                }
                t[i].or = u64::MAX;
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

static EID: [IdShape; 128] = id_shapes(b'e', 0, 8, false);
static KID: [IdShape; 128] = id_shapes(b'k', 0, 8, false);
static UID: [IdShape; 128] = id_shapes(b'u', 8, 0, true);

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct DecShape {
    ctrl: [u8; 16],
    subv: [u8; 16],
    min: u32,
    _pad: [u8; 28],
}

const fn dec_shapes(prefix: &[u8]) -> [DecShape; 128] {
    let mut t = [DecShape { ctrl: [0x80; 16], subv: [0; 16], min: 0, _pad: [0; 28] }; 128];
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
                t[i].min = if nd == 1 { 0 } else { pow(10, nd - 1) as u32 };
            } else {
                t[i].subv[8] = 1;
            }
            q += 1;
        }
        raw += 1;
    }
    t
}

static PROD: [DecShape; 128] = dec_shapes(b"p");
static CAMP: [DecShape; 128] = dec_shapes(b"c_");

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct TsRow {
    base: [u8; 16],
    lim: [u8; 16],
    w1: [i8; 16],
    w2: [i16; 8],
    w3: [i16; 8],
    off: i32,
    kq: i32,
    kmask: u32,
    dmask: i32,
    me: i32,
    limv: [u32; 4],
    mf: [i32; 2],
}

const fn ts_f12(f2: bool, q: bool) -> TsRow {
    let mut r = TS_INVALID;
    let b = *b"00:00:00";
    let l = [2u8, 9, 0, 5, 9, 0, 5, 9];
    let w = [10i8, 1, 0, 10, 1, 0, 10, 1];
    let mut i = 0;
    while i < 8 {
        r.base[i] = b[i];
        r.lim[i] = l[i];
        r.w1[i] = w[i];
        i += 1;
    }
    let mut next = 9;
    if f2 {
        let b2 = *b"+00:00";
        let l2 = [2u8, 2, 9, 0, 5, 9];
        let w2 = [0i8, 10, 1, 0, 10, 1];
        let mut i = 0;
        while i < 6 {
            r.base[8 + i] = b2[i];
            r.lim[8 + i] = l2[i];
            r.w1[8 + i] = w2[i];
            i += 1;
        }
        next = 14;
        r.mf = [60_000, -60_000];
    } else {
        r.base[8] = b'Z';
        r.lim[8] = 0;
    }
    if q {
        r.base[next] = b'"';
        r.lim[next] = 0;
    }
    r.w2 = [60, 1, 60, 1, 1, 1, 1, 0];
    r.w3 = [60, 1, 60, 1, 0, 0, 0, 0];
    r.off = 11 + q as i32;
    r.kq = q as i32;
    r.kmask = 0x0FFE | q as u32;
    r.dmask = -1;
    r.me = 1000;
    r.limv = [86_400, 1440, u32::MAX, u32::MAX];
    r
}

const TS_INVALID: TsRow = TsRow {
    base: [0; 16],
    lim: [255; 16],
    w1: [0; 16],
    w2: [0; 8],
    w3: [0; 8],
    off: 0,
    kq: 0,
    kmask: 0,
    dmask: 0,
    me: 0,
    limv: [0, 0, u32::MAX, u32::MAX],
    mf: [0, 0],
};

const fn ts_rows() -> [TsRow; 64] {
    let mut t = [TS_INVALID; 64];
    let mut n = 1;
    while n <= 16 {
        if n != 15 {
            let mut r = TS_INVALID;
            let mut i = 16 - n;
            while i < 16 {
                r.base[i] = b'0';
                r.lim[i] = 9;
                r.w1[i] = if i % 2 == 0 { 10 } else { 1 };
                i += 1;
            }
            r.w2 = [100, 1, 100, 1, 100, 1, 100, 1];
            r.w3 = [10_000, 1, 10_000, 1, 0, 0, 0, 0];
            r.off = n as i32 - 16;
            r.limv = [u32::MAX; 4];
            r.me = 100_000_000;
            r.mf = [1, 1];
            t[n] = r;
        }
        n += 1;
    }
    let mut r = TS_INVALID;
    r.base[1] = b'"';
    r.lim[1] = 0;
    r.base[15] = b'"';
    r.lim[15] = 0;
    let mut i = 2;
    while i < 15 {
        r.base[i] = b'0';
        r.lim[i] = 9;
        r.w1[i] = if i == 14 { 1 } else if i % 2 == 0 { 10 } else { 1 };
        i += 1;
    }
    r.w2 = [0, 1, 100, 1, 100, 1, 10, 1];
    r.w3 = [10_000, 1, 1000, 1, 0, 0, 0, 0];
    r.off = -1;
    r.limv = [u32::MAX; 4];
    r.me = 10_000_000;
    r.mf = [1, 1];
    t[15] = r;
    t[20] = ts_f12(false, false);
    t[22] = ts_f12(false, true);
    t[25] = ts_f12(true, false);
    t[27] = ts_f12(true, true);
    t
}

static TS: [TsRow; 64] = ts_rows();

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct KindRow {
    a: [u8; 16],
    b: [u8; 16],
    mask: u32,
    k: [u32; 2],
}

const fn put(dst: &mut [u8; 16], s: &[u8]) {
    let mut i = 0;
    while i < s.len() {
        dst[i] = s[i];
        i += 1;
    }
}

const fn kind_rows() -> [KindRow; 64] {
    let mut t = [KindRow { a: [b'\n'; 16], b: [b'\n'; 16], mask: 1, k: [3, 3] }; 64];
    put(&mut t[4].a, b"view");
    put(&mut t[4].b, b"cart");
    t[4].mask = 0xF;
    put(&mut t[6].a, b"\"view\"");
    put(&mut t[6].b, b"\"cart\"");
    t[6].mask = 0x3F;
    put(&mut t[8].a, b"purchase");
    put(&mut t[8].b, b"purchase");
    t[8].mask = 0xFF;
    put(&mut t[10].a, b"\"purchase\"");
    put(&mut t[10].b, b"\"purchase\"");
    t[10].mask = 0x3FF;
    t[4].k = [0, 1];
    t[6].k = [0, 1];
    t[8].k = [2, 2];
    t[10].k = [2, 2];
    t
}

static KIND: [KindRow; 64] = kind_rows();

#[repr(C, align(32))]
#[derive(Clone, Copy)]
struct DateEnt {
    key: [u8; 16],
    ms: i64,
    strikes: u64,
}

thread_local! {
    static DATES: UnsafeCell<[DateEnt; 64]> =
        const { UnsafeCell::new([DateEnt { key: [b','; 16], ms: 0, strikes: 0 }; 64]) };
}

#[inline(always)]
fn date_idx(dd: u16) -> usize {
    unsafe { _pext_u64(dd as u64, 0x0F03) as usize }
}

#[cold]
#[inline(never)]
unsafe fn date_miss(kp: *const u8, kmask: u32, cache: *mut DateEnt) -> Option<i64> {
    count(14);
    unsafe {
        let k = core::slice::from_raw_parts(kp, 12);
        if kmask & 1 != 0 && k[0] != b'"' {
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

struct Masks {
    c: u64,
    q: u64,
    n: u64,
}

#[inline(always)]
unsafe fn masks<const WIDE: bool>(p: *const u8) -> Masks {
    unsafe {
        if WIDE {
            let v = _mm512_loadu_si512(p as *const __m512i);
            Masks {
                c: _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b',' as i8)),
                q: _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'"' as i8)),
                n: _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi8(b'\n' as i8)),
            }
        } else {
            let a = _mm256_loadu_si256(p as *const __m256i);
            let b = _mm256_loadu_si256(p.wrapping_add(32) as *const __m256i);
            let m = |ch: u8| {
                let x = _mm256_set1_epi8(ch as i8);
                _mm256_cmpeq_epi8_mask(a, x) as u64 | (_mm256_cmpeq_epi8_mask(b, x) as u64) << 32
            };
            Masks { c: m(b','), q: m(b'"'), n: m(b'\n') }
        }
    }
}

#[inline(always)]
unsafe fn ts_decode<const FULL: bool>(rs: *const u8, raw: usize, cache: *mut DateEnt) -> Option<i64> {
    unsafe {
        let row = TS.get_unchecked(raw);
        let kp = rs.wrapping_offset(row.kq as isize - 1);
        let ent = cache.add(date_idx(rd16(kp.wrapping_add(9))));
        let neq = _mm_cmpneq_epi8_mask(ld(kp), ld(ent as *const u8)) as u32 & row.kmask;
        let day_ms = if STAGE == 9 || STAGE == 10 {
            0
        } else if neq != 0 {
            match date_miss(kp, row.kmask, cache) {
                Some(v) => v,
                None => return None,
            }
        } else {
            (*ent).ms
        };
        let lp = rs.wrapping_offset(row.off as isize);
        let d = _mm_sub_epi8(ld(lp), ld(row.base.as_ptr()));
        let m1 = _mm_maddubs_epi16(d, ld(row.w1.as_ptr() as *const u8));
        let m2 = _mm_madd_epi16(m1, ld(row.w2.as_ptr() as *const u8));
        let m3 = _mm_madd_epi16(_mm_packus_epi32(m2, m2), ld(row.w3.as_ptr() as *const u8));
        let bad = _mm_cmpgt_epu8_mask(d, ld(row.lim.as_ptr())) as u32 | _mm_cmpge_epu32_mask(m3, ld(row.limv.as_ptr() as *const u8)) as u32;
        if bad != 0 {
            return None;
        }
        let r = _mm_cvtsi128_si64(m3) as u64;
        let date_ms = day_ms & row.dmask as i64;
        if FULL && STAGE != 8 && STAGE != 10 {
            let mf = *row.mf.get_unchecked((*lp.wrapping_add(8) >> 1 & 1) as usize);
            Some(date_ms + (r as u32) as i64 * row.me as i64 + (r >> 32) as i64 * mf as i64)
        } else {
            Some(date_ms + (r as u32) as i64 * 1000)
        }
    }
}

#[inline(always)]
unsafe fn ids2(xa: __m128i, sa: &IdShape, xb: __m128i, sb: &IdShape) -> Option<(u64, u64)> {
    unsafe {
        let va = _mm_sub_epi8(_mm_shuffle_epi8(xa, ld(sa.ctrl.as_ptr())), ld(sa.subv.as_ptr()));
        let vb = _mm_sub_epi8(_mm_shuffle_epi8(xb, ld(sb.ctrl.as_ptr())), ld(sb.subv.as_ptr()));
        let dig = _mm_blend_epi32::<0b1100>(va, vb);
        let chk = _mm_blend_epi32::<0b1100>(vb, va);
        let kd = _mm_cmple_epu8_mask(dig, _mm_set1_epi8(9));
        let e = _mm_sub_epi8(dig, _mm_set1_epi8(49));
        let ke = _mm_cmple_epu8_mask(e, _mm_set1_epi8(25));
        if (_mm_testz_si128(chk, chk) == 0) | (_kortestc_mask16_u8(kd, ke) == 0) {
            return None;
        }
        let val = _mm_mask_add_epi8(dig, ke, e, _mm_set1_epi8(10));
        let m1 = _mm_maddubs_epi16(val, _mm_set1_epi16(0x0124));
        let m2 = _mm_madd_epi16(m1, _mm_set1_epi32(0x0001_0510));
        let ids = _mm_add_epi64(_mm_mul_epu32(m2, _mm_set1_epi64x(1_679_616)), _mm_srli_epi64::<32>(m2));
        let a = _mm_cvtsi128_si64(ids) as u64;
        let b = _mm_extract_epi64::<1>(ids) as u64;
        if (a < sa.min) | (b < sb.min) {
            return None;
        }
        Some((a, b | sb.or))
    }
}

#[inline(always)]
unsafe fn dec(x: __m128i, s: &DecShape) -> Option<u32> {
    unsafe {
        let v = _mm_sub_epi8(_mm_shuffle_epi8(x, ld(s.ctrl.as_ptr())), ld(s.subv.as_ptr()));
        let lim = _mm_set_epi64x(0, 0x0909_0909_0909_0909);
        if _mm_cmpgt_epu8_mask(v, lim) != 0 {
            return None;
        }
        let m1 = _mm_maddubs_epi16(v, _mm_set_epi64x(0, 0x010A_010A_010A_010A));
        let m2 = _mm_madd_epi16(m1, _mm_set_epi64x(0, 0x0001_0064_0001_0064));
        let m3 = _mm_madd_epi16(_mm_packus_epi32(m2, m2), _mm_set_epi64x(0, 0x0001_2710));
        let val = _mm_cvtsi128_si32(m3) as u32;
        if val < s.min {
            return None;
        }
        Some(val)
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

macro_rules! or_fail {
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

const OB: usize = 40;

#[inline(always)]
unsafe fn ev_line<S: EventSink, const WIDE: bool, const CHAIN: bool>(
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
        let w1 = masks::<WIDE>(p);
        let wb = masks::<WIDE>(pb);
        let c0 = _tzcnt_u64(w1.c);
        let t = _blsr_u64(w1.c);
        let c1 = _tzcnt_u64(t);
        let t = _blsr_u64(t);
        let c2 = _tzcnt_u64(t);
        let rest1 = _blsr_u64(t);
        let after_c2 = ((!0u64).wrapping_shl(c2 as u32 + 1) >> OB) | (!0u64 << (64 - OB));
        let t = wb.c & after_c2;
        let c3 = _tzcnt_u64(t);
        let t = _blsr_u64(t);
        let c4 = _tzcnt_u64(t);
        let t = _blsr_u64(t);
        let c5 = _tzcnt_u64(t);
        let restb = _blsr_u64(t);
        let (m, head) = if CHAIN { (_tzcnt_u64(wb.n), w1.n | rest1) } else { (e_in.wrapping_sub(lb) as u64, rest1) };
        if (c2 > 62) | ((head << (64 - OB)) != 0) | (m > 63) | (c5 >= m) | (_bzhi_u64(restb, m as u32) != 0) {
            count(0);
            return FAIL;
        }
        let (c0, c1, c2, c3, c4, c5, m) = (c0 as usize, c1 as usize, c2 as usize, c3 as usize, c4 as usize, c5 as usize, m as usize);
        let e = lb + m;
        if CHAIN && e >= len {
            count(1);
            return FAIL;
        }
        if STAGE == 0 {
            sink.event(&EvRow { eid: c0 as u64, uid: c1 as u64, ts: c3 as i64, ingest: c4 as i64, kind: 0, product: c5 as u32, price: 0 });
            return e;
        }
        let se = EID.get_unchecked(c0 * 2 + (w1.q & 1) as usize);
        let su = UID.get_unchecked((c1 - c0 - 1) * 2 + (w1.q >> (c0 + 1) & 1) as usize);
        let (eid, uid) = or_fail!(
            ids2(ld(p.wrapping_add(c0).wrapping_sub(16)), se, ld(p.wrapping_add(c1).wrapping_sub(16)), su),
            cold_ids(base, len, l, l + c0, b'e', l + c0 + 1, l + c1)
        );
        if STAGE == 1 {
            sink.event(&EvRow { eid, uid, ts: c3 as i64, ingest: c4 as i64, kind: 0, product: c5 as u32, price: 0 });
            return e;
        }
        let ts = or_fail!(ts_decode::<true>(p.wrapping_add(c1 + 1), c2 - c1 - 1, cache), cold_ts(base, len, l + c1 + 1, l + c2, false));
        if STAGE == 2 {
            sink.event(&EvRow { eid, uid, ts, ingest: c4 as i64, kind: 0, product: c5 as u32, price: 0 });
            return e;
        }
        let raw3 = (OB + c3).wrapping_sub(c2 + 1);
        let ingest = or_fail!(
            if raw3 | 2 == 22 { ts_decode::<false>(p.wrapping_add(c2 + 1), raw3, cache) } else { None },
            cold_ts(base, len, l + c2 + 1, lb + c3, true)
        );
        if STAGE == 3 {
            sink.event(&EvRow { eid, uid, ts, ingest, kind: 0, product: c5 as u32, price: 0 });
            return e;
        }
        let kr = KIND.get_unchecked(c4 - c3 - 1);
        let xk = ld(pb.wrapping_add(c3 + 1));
        let f = (_mm_cmpneq_epi8_mask(xk, ld(kr.a.as_ptr())) as u32 & kr.mask != 0) as usize;
        let nb = _mm_cmpneq_epi8_mask(xk, ld((kr as *const KindRow as *const u8).add(16 * f))) as u32 & kr.mask;
        let kind = if nb != 0 {
            match cold_kind(base, len, lb + c3 + 1, lb + c4) {
                Some(v) => v,
                None => return FAIL,
            }
        } else {
            *kr.k.get_unchecked(f)
        };
        if STAGE == 4 {
            sink.event(&EvRow { eid, uid, ts, ingest, kind: kind as u8, product: c5 as u32, price: 0 });
            return e;
        }
        let sp = PROD.get_unchecked((c5 - c4 - 1) * 2 + (wb.q >> (c4 + 1) & 1) as usize);
        let product = or_fail!(dec(ld(pb.wrapping_add(c5).wrapping_sub(16)), sp), cold_dec(base, len, lb + c4 + 1, lb + c5, true));
        let raw6 = m - c5 - 1;
        let pq = _bzhi_u64(wb.q >> (c5 + 1), raw6 as u32);
        let qb = pq & 1;
        if pq != qb + qb.wrapping_shl((raw6 as u32).wrapping_sub(1)) {
            count(9);
            return FAIL;
        }
        let mut pr = 0;
        if kind == 2 {
            match price(pb.wrapping_add(m - qb as usize), raw6 - 2 * qb as usize) {
                Some(v) => pr = v,
                None => {
                    count(10);
                    return FAIL;
                }
            }
        }
        sink.event(&EvRow { eid, uid, ts, ingest, kind: kind as u8, product, price: pr });
        e
    }
}

#[inline(always)]
unsafe fn cl_line<S: ClickSink, const WIDE: bool, const CHAIN: bool>(
    base: *const u8,
    len: usize,
    l: usize,
    e_in: usize,
    cache: *mut DateEnt,
    sink: &mut S,
) -> usize {
    unsafe {
        let p = base.add(l);
        let w = masks::<WIDE>(p);
        let c0 = _tzcnt_u64(w.c);
        let t = _blsr_u64(w.c);
        let c1 = _tzcnt_u64(t);
        let t = _blsr_u64(t);
        let c2 = _tzcnt_u64(t);
        let t = _blsr_u64(t);
        let n = if CHAIN { _tzcnt_u64(w.n) } else { (e_in - l) as u64 };
        if (n > if CHAIN { 63 } else { 64 }) | (c2 >= n) | (_bzhi_u64(t, n as u32) != 0) {
            count(11);
            return FAIL;
        }
        let (c0, c1, c2, n) = (c0 as usize, c1 as usize, c2 as usize, n as usize);
        let e = l + n;
        let sk = KID.get_unchecked(c0 * 2 + (w.q & 1) as usize);
        let su = UID.get_unchecked((c1 - c0 - 1) * 2 + (w.q >> (c0 + 1) & 1) as usize);
        let (cid, uid) = or_fail!(
            ids2(ld(p.wrapping_add(c0).wrapping_sub(16)), sk, ld(p.wrapping_add(c1).wrapping_sub(16)), su),
            cold_ids(base, len, l, l + c0, b'k', l + c0 + 1, l + c1)
        );
        let sc = CAMP.get_unchecked((c2 - c1 - 1) * 2 + (w.q >> (c1 + 1) & 1) as usize);
        let campaign = or_fail!(dec(ld(p.wrapping_add(c2).wrapping_sub(16)), sc), cold_dec(base, len, l + c1 + 1, l + c2, false));
        let ts = or_fail!(ts_decode::<true>(p.wrapping_add(c2 + 1), n - c2 - 1, cache), cold_ts(base, len, l + c2 + 1, e, false));
        sink.click(&ClRow { cid, uid, campaign, ts });
        e
    }
}

const CHUNK: usize = 8192;

macro_rules! runner {
    ($name:ident, $sink:ident, $line:ident, $slow:path, $scan:ident, $reason:literal) => {
        #[cold]
        #[inline(never)]
        fn $scan<S: $sink>(buf: &[u8], pos: usize, sink: &mut S) -> usize {
            count($reason);
            let e = match buf[pos..].iter().position(|&b| b == b'\n') {
                Some(i) => pos + i,
                None => buf.len(),
            };
            $slow(buf, pos, e, sink);
            e + 1
        }

        #[inline(never)]
        unsafe fn $name<S: $sink, const WIDE: bool, const CHAIN: bool>(buf: &[u8], sink: &mut S) {
            unsafe {
                let base = buf.as_ptr();
                let len = buf.len();
                let cache = DATES.with(|c| c.get()) as *mut DateEnt;
                let mut pos = 0usize;
                if CHAIN {
                    while pos + 64 <= len {
                        let r = $line::<S, WIDE, true>(base, len, pos, 0, cache, sink);
                        pos = if r == FAIL { $scan(buf, pos, sink) } else { r + 1 };
                    }
                    while pos < len {
                        pos = $scan(buf, pos, sink);
                    }
                    return;
                }
                let mut ends_buf = MaybeUninit::<[u32; CHUNK + 2]>::uninit();
                let ends = ends_buf.as_mut_ptr() as *mut u32;
                let mut off = 0usize;
                while off < len {
                    let cstart = off;
                    let stop = if len - off > CHUNK { off + CHUNK } else { len };
                    let mut cnt = 0usize;
                    while off < stop {
                        let mut m = masks::<WIDE>(base.add(off)).n;
                        if stop - off < 64 {
                            m &= (1u64 << (stop - off)) - 1;
                        }
                        let rel = (off - cstart) as u32;
                        let m2 = _blsr_u64(m);
                        *ends.add(cnt) = rel + _tzcnt_u64(m) as u32;
                        *ends.add(cnt + 1) = rel + _tzcnt_u64(m2) as u32;
                        let c = _popcnt64(m as i64) as usize;
                        if c > 2 {
                            let mut m3 = _blsr_u64(m2);
                            let mut k = cnt + 2;
                            while m3 != 0 {
                                *ends.add(k) = rel + _tzcnt_u64(m3) as u32;
                                k += 1;
                                m3 = _blsr_u64(m3);
                            }
                        }
                        cnt += c;
                        off += 64;
                    }
                    for i in 0..cnt {
                        let e = cstart + *ends.add(i) as usize;
                        if $line::<S, WIDE, false>(base, len, pos, e, cache, sink) == FAIL {
                            count($reason);
                            $slow(buf, pos, e, sink);
                        }
                        pos = e + 1;
                    }
                }
                if pos < len {
                    count($reason);
                    $slow(buf, pos, len, sink);
                }
            }
        }
    };
}
runner!(run_events, EventSink, ev_line, slow::event, scan_event, 12);
runner!(run_clicks, ClickSink, cl_line, slow::click, scan_click, 13);

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
