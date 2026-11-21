//! ymm: CSV parse kernel for events and clicks, AVX2 + AVX-512BW/VL mask compares on 256-bit
//! vectors only (the compares are inline asm, so LLVM cannot fuse them into zmm).
//! Exactness is defined by k_ref.rs. Every line the fast path cannot prove it handles goes to
//! a per-line copy of the reference code (module "slow" at the end).
//!
//! Two stages per 32 KB chunk:
//! 1. Per 64-byte block: comma and quote bit masks (stored), newline positions (flattened).
//!    Masks also cover 256 bytes before and after the chunk, so a line's windows never miss.
//! 2. Per line (its start and end come from the newline list, so lines are independent):
//!    57-bit windows of the comma/quote masks at the line start and after the 3rd comma,
//!    field spans, then a branch-free decode of all fields, and one branch: row or slow path.
//!
//! Fast-path conditions (else: slow path for that line):
//! - the line ends with b'\n' (the buffer's final line without one takes the slow path);
//! - the line starts at most 256 bytes before its chunk;
//! - events: 6 commas, the 3rd within the first 57 bytes; the bytes after the 3rd comma up
//!   to the b'\n' fewer than 57;
//!   clicks: 3 commas, line shorter than 64 bytes;
//! - every quote is a whole-field quote: each field is quote-free or b'"' value b'"' with no
//!   other quote (see quotes_ok);
//! - event_id / click_id non-empty;
//! - ts is not an all-digit field longer than 16 bytes;
//! - events with event_type exactly "purchase": price field at most 8 bytes;
//! - each F1/F2-shaped ts / ingest_ts finds its "YYYY-MM-" in the 16-slot month cache (a miss
//!   fills the slot in the cold path, if the month is valid, and that line goes slow).
//! Inside the fast path every value is exact, including odd and empty IDs, bad timestamps,
//! odd products / campaigns and bad prices.
//! Needs -C target-cpu=cascadelake (AVX2, AVX-512F/BW/VL, BMI1/2). No VBMI, no zmm.
#![allow(unsafe_op_in_unsafe_fn)]

use crate::iface::*;
use core::arch::asm;
use core::arch::x86_64::*;
use core::hint::select_unpredictable;

#[cfg(not(all(target_feature = "avx2", target_feature = "avx512bw", target_feature = "avx512vl", target_feature = "bmi2")))]
compile_error!("k_ymm needs -C target-cpu=cascadelake (AVX2, AVX-512BW/VL, BMI2)");

pub struct Ymm;

impl Kernel for Ymm {
    const NAME: &'static str = "ymm";
    fn events<S: EventSink>(buf: &[u8], sink: &mut S) {
        unsafe { events_impl(buf, sink) }
    }
    fn clicks<S: ClickSink>(buf: &[u8], sink: &mut S) {
        unsafe { clicks_impl(buf, sink) }
    }
}

#[inline(always)]
unsafe fn ld64(p: *const u8) -> u64 {
    (p as *const u64).read_unaligned()
}
#[inline(always)]
unsafe fn ld16(p: *const u8) -> u16 {
    (p as *const u16).read_unaligned()
}
#[inline(always)]
unsafe fn ld256(p: *const u8) -> __m256i {
    _mm256_loadu_si256(p as *const __m256i)
}
#[inline(always)]
unsafe fn splat(b: u8) -> __m256i {
    _mm256_set1_epi8(b as i8)
}

/// 64-bit equality mask of the 64 bytes in (a, b) against c (2 ymm compares, kunpckdq, kmovq).
#[inline(always)]
unsafe fn m64(a: __m256i, b: __m256i, c: __m256i) -> u64 {
    let r: u64;
    asm!(
        "vpcmpeqb {k1}, {a}, {c}",
        "vpcmpeqb {k2}, {b}, {c}",
        "kunpckdq {k1}, {k2}, {k1}",
        "kmovq {r}, {k1}",
        a = in(ymm_reg) a, b = in(ymm_reg) b, c = in(ymm_reg) c,
        k1 = out(kreg) _, k2 = out(kreg) _, r = out(reg) r,
        options(pure, nomem, nostack, preserves_flags)
    );
    r
}

/// Quote check for the fields of one window: t = terminator mask (one bit at each field's end,
/// relative to the window), c = t without the last terminator, q = quote mask below the last
/// terminator. True iff every quote is the first or last byte of its field, a field starts with
/// a quote exactly when it ends with one, and no field is a lone quote. The add carries each
/// field-start quote to its field's terminator.
#[inline(always)]
fn quotes_ok(t: u64, c: u64, q: u64) -> bool {
    let qs = q & ((c << 1) | 1); // quotes at field starts
    let qe = q & (t >> 1); // quotes at field ends
    let sqt = (!t).wrapping_add(qs) & t;
    ((sqt ^ (qe << 1)) | (q ^ qs ^ qe)) == 0
}

// ---------------------------------------------------------------------------------------
// Month cache: "YYYY-MM-" (8 bytes, little-endian u64) -> epoch ms of each day of the month.
const DBAD: i64 = i64::MIN;
const NDC: usize = 64;
struct DCache {
    key: [u64; NDC],
    days: [[i64; 32]; NDC], // days[i][d] = ms of day d (1..=month length); DBAD elsewhere
}
impl DCache {
    fn new() -> Self {
        DCache { key: [0; NDC], days: [[DBAD; 32]; NDC] }
    }
}
/// Slot: bits of the month's ones digit (4), the month's tens digit (1) and the year's ones
/// digit (1), so all 24 months of two consecutive years get distinct slots.
#[inline(always)]
fn didx(key: u64) -> usize {
    unsafe { _pext_u64(key, 0x000F_0100_0100_0000) as usize }
}
/// Fills the slot for key if key is dddd-dd- with month 01..12.
#[cold]
#[inline(never)]
fn dfill(dc: &mut DCache, key: u64) {
    let b = key.to_le_bytes();
    let dg = |x: u8| x.is_ascii_digit();
    if !(dg(b[0]) && dg(b[1]) && dg(b[2]) && dg(b[3]) && b[4] == b'-' && dg(b[5]) && dg(b[6]) && b[7] == b'-') {
        return;
    }
    let v = |x: u8| (x - b'0') as i64;
    let y = v(b[0]) * 1000 + v(b[1]) * 100 + v(b[2]) * 10 + v(b[3]);
    let m = v(b[5]) * 10 + v(b[6]);
    if !(1..=12).contains(&m) {
        return;
    }
    let i = didx(key);
    dc.key[i] = key;
    let dim = slow::days_in_month(y, m);
    let d1 = slow::days_from_civil(y, m, 1);
    for d in 0..32i64 {
        dc.days[i][d as usize] = if d >= 1 && d <= dim { (d1 + d - 1) * 86_400_000 } else { DBAD };
    }
}

const fn bytes32(s: &[u8], fill: u8) -> [u8; 32] {
    let mut a = [fill; 32];
    let mut i = 0;
    while i < s.len() {
        a[i] = s[i];
        i += 1;
    }
    a
}
/// XOR pattern / limit: (t ^ PATX) <= LIMX byte-wise holds exactly at digit positions holding
/// a digit and at literal positions holding the literal.
const PATX: [u8; 32] = bytes32(b"0000-00-00T00:00:00Z00:00", 0);
const LIMX: [u8; 32] = {
    let mut a = [0xFFu8; 32];
    let mut i = 0;
    while i < 25 {
        a[i] = if PATX[i] == b'0' { 9 } else { 0 };
        i += 1;
    }
    a
};
const F1BITS: u32 = 0xF_FFFF; // bytes 0..=19
const F2BITS: u32 = 0x7_FFFF | (0x1F << 20); // bytes 0..=18 and 20..=24 (19 is the sign)
/// Required bits of (shape mask | digit mask << 32) per ts length: F3 for 1..=16, F1 for 20,
/// F2 for 25; every other length can match no form (all bits required).
const TSHAPE: [u64; 32] = {
    let mut t = [u64::MAX; 32];
    let mut n = 1;
    while n <= 16 {
        t[n] = ((1u64 << n) - 1) << 32;
        n += 1;
    }
    t[20] = F1BITS as u64;
    t[25] = F2BITS as u64;
    t
};
const Z: u8 = 0x80;
/// Digit pairs of ts bytes 8..24 (lane 0) and ingest bytes 8..24 (lane 1), local indices.
/// lane 0 u16: hh, mm, ss, 0, DD, 0, oh, 10*om_tens; lane 1 u16: hh, mm, ss, 0, DD, 0, 0, 0.
const SHUF: [u8; 32] = [
    3, 4, 6, 7, 9, 10, Z, Z, 0, 1, Z, Z, 12, 13, 15, Z, //
    3, 4, 6, 7, 9, 10, Z, Z, 0, 1, Z, Z, Z, Z, Z, Z,
];
const W10: [u8; 32] = [10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1, 10, 1];
const FF: u16 = 0xFFFF;
const LIM16: [u16; 16] = [23, 59, 59, FF, 31, FF, 23, 50, 23, 59, 59, FF, 31, FF, FF, FF];
const TSL1: u32 = 0b0001_0111; // ts lanes checked for F1: hh, mm, ss, DD
const TSL2: u32 = 0b1101_0111; // + oh, om tens
const INGL: u32 = 0b0001_0111 << 8;
const TRNG: [u32; 32] = {
    let mut t = [0u32; 32];
    t[20] = TSL1;
    t[25] = TSL2;
    t
};
const WT: [i16; 16] = [3600, 60, 1, 0, 1, 0, 60, 1, 3600, 60, 1, 0, 1, 0, 0, 0];

#[inline(always)]
unsafe fn cv(a: &[u8; 32]) -> __m256i {
    _mm256_loadu_si256(a.as_ptr() as *const __m256i)
}

/// Decimal value of the bytes ending at b, n <= 16 of them; leading positions are zeroed.
#[inline(always)]
unsafe fn dec16(b: *const u8, n: usize) -> i64 {
    let u = _mm_loadu_si128(b.sub(16) as *const __m128i);
    let d = _mm_sub_epi8(u, _mm_set1_epi8(b'0' as i8));
    let keep = (0xFFFF_0000u32.wrapping_shr(n.min(16) as u32)) as u16;
    let d = _mm_maskz_mov_epi8(keep, d);
    let a = _mm_maddubs_epi16(d, _mm_set1_epi16(0x010A));
    let b = _mm_madd_epi16(a, _mm_set1_epi32(0x0001_0064));
    let c = _mm_packus_epi32(b, b);
    let e = _mm_madd_epi16(c, _mm_set1_epi32(0x0001_2710));
    let x = _mm_cvtsi128_si64(e) as u64;
    ((x & 0xFFFF_FFFF) * 100_000_000 + (x >> 32)) as i64
}

/// Shape mask: bit i set iff byte i matches "dddd-dd-ddTdd:dd:ddZdd:dd" (digit or literal).
#[inline(always)]
unsafe fn shape(t: __m256i) -> u32 {
    _mm256_cmple_epu8_mask(_mm256_xor_si256(t, cv(&PATX)), cv(&LIMX))
}

/// Civil parts of ts at ta (lane 0) and of a second F1 field at ia (lane 1):
/// (ts seconds of day, ts day, ts offset ms (unsigned), second's seconds, second's day, range-bad mask).
#[inline(always)]
unsafe fn civil2(ta: *const u8, ia: *const u8) -> (i64, usize, i64, i64, usize, u32) {
    let u = _mm256_inserti128_si256::<1>(_mm256_castsi128_si256(_mm_loadu_si128(ta.add(8) as *const __m128i)), _mm_loadu_si128(ia.add(8) as *const __m128i));
    let q = _mm256_maddubs_epi16(_mm256_shuffle_epi8(_mm256_sub_epi8(u, splat(b'0')), cv(&SHUF)), cv(&W10));
    let rbad = _mm256_cmpgt_epu16_mask(q, _mm256_loadu_si256(LIM16.as_ptr() as *const __m256i)) as u32;
    let r = _mm256_madd_epi16(q, _mm256_loadu_si256(WT.as_ptr() as *const __m256i));
    let rl = _mm256_castsi256_si128(r);
    let rh = _mm256_extracti128_si256::<1>(r);
    let x0 = _mm_cvtsi128_si64(rl) as u64;
    let x1 = _mm_extract_epi64::<1>(rl) as u64;
    let y0 = _mm_cvtsi128_si64(rh) as u64;
    let y1 = _mm_extract_epi64::<1>(rh) as u64;
    let t_secs = ((x0 & 0xFFFF_FFFF) + (x0 >> 32)) as i64;
    let t_dd = (x1 & 31) as usize;
    let t_off = ((x1 >> 32) as i64 + (*ta.add(24)).wrapping_sub(b'0') as i64) * 60_000;
    let i_secs = ((y0 & 0xFFFF_FFFF) + (y0 >> 32)) as i64;
    let i_dd = (y1 & 31) as usize;
    (t_secs, t_dd, t_off, i_secs, i_dd, rbad)
}

/// ts value from its parts. Returns (ts, needs the slow path).
#[inline(always)]
unsafe fn ts_val(ta: *const u8, tn: usize, tb: *const u8, t_secs: i64, t_dd: usize, t_off: i64, rbad: u32, dc: &DCache) -> (i64, bool) {
    let t = ld256(ta);
    let tm = shape(t);
    let dm = _mm256_cmple_epu8_mask(_mm256_sub_epi8(t, splat(b'0')), splat(9));
    let ti = tn.min(31);
    let have = tm as u64 | ((dm as u64) << 32);
    let need = *TSHAPE.get_unchecked(ti);
    let sh = (have & need) == need;
    let civ = (tn == 20) | (tn == 25);
    let f2 = tn == 25;
    let sign = *ta.add(19);
    let sgn = (sign.wrapping_sub(b'+') & 0xFD) == 0;
    let lm = _bzhi_u32(u32::MAX, tn as u32);
    let long = (tn > 16) & ((dm & lm) == lm);
    let key = ld64(ta);
    let slot = didx(key);
    let hit = *dc.key.get_unchecked(slot) == key;
    let day = *dc.days.get_unchecked(slot).get_unchecked(t_dd);
    let off = select_unpredictable(sign == b'+', -t_off, t_off);
    let off = select_unpredictable(f2, off, 0);
    let f3 = dec16(tb, tn);
    let tv = select_unpredictable(civ, day.wrapping_add(t_secs * 1000 + off), f3);
    let ok = sh & ((rbad & *TRNG.get_unchecked(ti)) == 0) & (!civ | (day != DBAD)) & (!f2 | sgn);
    (select_unpredictable(ok, tv, TS_BAD), long | (civ & sh & !hit))
}

/// ingest_ts (F1 only) value. Returns (value, needs the slow path).
#[inline(always)]
unsafe fn f1_val(ia: *const u8, inn: usize, i_secs: i64, i_dd: usize, rbad: u32, dc: &DCache) -> (i64, bool) {
    let im = shape(ld256(ia));
    let g1 = (inn == 20) & ((im & F1BITS) == F1BITS);
    let key = ld64(ia);
    let slot = didx(key);
    let hit = *dc.key.get_unchecked(slot) == key;
    let day = *dc.days.get_unchecked(slot).get_unchecked(i_dd);
    let ok = g1 & ((rbad & INGL) == 0) & (day != DBAD);
    (select_unpredictable(ok, day.wrapping_add(i_secs * 1000), TS_BAD), g1 & !hit)
}

/// Four right-aligned digit strings in one ymm: lanes 0, 1 base 36 ([0-9a-z]), lanes 2, 3
/// base 10. Lane i: the k_i <= 8 bytes ending at e_i (k_i > 8 gives garbage; callers reject
/// such lengths themselves). Returns the values and a 4-bit mask of bad lanes: a byte outside
/// the lane's alphabet, or (lanes 0..=2) a leading b'0' with k > 1.
#[inline(always)]
unsafe fn quad(e: [*const u8; 4], k: [usize; 4]) -> ([u64; 4], u32) {
    let x = _mm256_set_epi64x(ld64(e[3].sub(8)) as i64, ld64(e[2].sub(8)) as i64, ld64(e[1].sub(8)) as i64, ld64(e[0].sub(8)) as i64);
    let m = (0xFF00u32.wrapping_shr(k[0] as u32) & 0xFF)
        | (0xFF_0000u32.wrapping_shr(k[1] as u32) & 0xFF00)
        | (0xFF00_0000u32.wrapping_shr(k[2] as u32) & 0xFF_0000)
        | ((0xFF_0000_0000u64.wrapping_shr(k[3] as u32) as u32) & 0xFF00_0000);
    let d0 = _mm256_sub_epi8(x, splat(b'0'));
    let isd = _mm256_cmple_epu8_mask(d0, splat(9));
    let xl = _mm256_sub_epi8(x, splat(b'a'));
    let isl = _mm256_cmple_epu8_mask(xl, splat(25));
    let z = _mm256_cmpeq_epi8_mask(x, splat(b'0'));
    let okb = isd | (isl & 0x0000_FFFF);
    let val = _mm256_mask_blend_epi8(isd, _mm256_add_epi8(xl, splat(10)), d0);
    let val = _mm256_maskz_mov_epi8(m, val);
    let first = m & !((m << 1) & 0xFEFE_FEFE);
    let bad = (m & !okb) | (z & first & 0x007F_7F7F);
    let w1 = _mm256_set_epi64x(0x010A_010A_010A_010A, 0x010A_010A_010A_010A, 0x0124_0124_0124_0124, 0x0124_0124_0124_0124);
    let w2 = _mm256_set_epi64x(0x0001_0064_0001_0064, 0x0001_0064_0001_0064, 0x0001_0510_0001_0510, 0x0001_0510_0001_0510);
    let w3 = _mm256_set_epi64x(10_000, 10_000, 1_679_616, 1_679_616);
    let a = _mm256_maddubs_epi16(val, w1);
    let b = _mm256_madd_epi16(a, w2);
    let c = _mm256_add_epi64(_mm256_mul_epu32(b, w3), _mm256_srli_epi64(b, 32));
    let hi = _mm256_extracti128_si256::<1>(c);
    let lo = _mm256_castsi256_si128(c);
    let v = [
        _mm_cvtsi128_si64(lo) as u64,
        _mm_extract_epi64::<1>(lo) as u64,
        _mm_cvtsi128_si64(hi) as u64,
        _mm_extract_epi64::<1>(hi) as u64,
    ];
    let lb = ((bad & 0xFF != 0) as u32)
        | (((bad & 0xFF00 != 0) as u32) << 1)
        | (((bad & 0xFF_0000 != 0) as u32) << 2)
        | (((bad & 0xFF00_0000 != 0) as u32) << 3);
    (v, lb)
}

#[inline(always)]
fn id_sel(n: usize, val: u64, canon: bool, off: usize) -> u64 {
    let odd = if n == 0 { ID_EMPTY } else { odd_id(off, n) };
    if canon { val } else { odd }
}

// ---------------------------------------------------------------------------------------
// Stage 1.

const CH: usize = 1 << 15; // chunk bytes
const LB: usize = 256; // mask look-behind bytes
const LA: usize = 256; // mask look-ahead bytes
const NW: usize = (LB + CH + LA) / 64; // mask words per chunk

struct Stage1 {
    cm: Vec<u64>,
    qm: Vec<u64>,
    nl: Vec<u32>,
}
impl Stage1 {
    fn new() -> Self {
        Stage1 { cm: vec![0; NW + 2], qm: vec![0; NW + 2], nl: vec![0; CH + 16] }
    }
}

/// Comma, quote and newline masks of the 64-byte block at b (bytes past len masked out).
#[inline(always)]
unsafe fn block(buf: &[u8], b: isize) -> (u64, u64, u64) {
    let len = buf.len();
    if b < 0 || b as usize >= len {
        return (0, 0, 0);
    }
    let b = b as usize;
    let p = buf.as_ptr().add(b);
    let (v0, v1) = (ld256(p), ld256(p.add(32)));
    let (mut c, mut q, mut n) = (m64(v0, v1, splat(b',')), m64(v0, v1, splat(b'"')), m64(v0, v1, splat(b'\n')));
    if b + 64 > len {
        let m = _bzhi_u64(u64::MAX, (len - b) as u32);
        c &= m;
        q &= m;
        n &= m;
    }
    (c, q, n)
}

/// Masks for [cs - LB, ce + LA) into st.cm / st.qm, newline positions in [cs, ce) into st.nl
/// (relative to cs).
/// Returns the newline count.
#[inline(always)]
unsafe fn stage1<const SLOTS: usize>(buf: &[u8], cs: usize, ce: usize, st: &mut Stage1) -> usize {
    let cmp = st.cm.as_mut_ptr();
    let qmp = st.qm.as_mut_ptr();
    let nlp = st.nl.as_mut_ptr();
    let lo = cs as isize - LB as isize;
    let mut nn = 0usize;
    let nb = (ce - cs).div_ceil(64);
    let mut w = 0usize;
    while w < LB / 64 {
        let (c, q, _) = block(buf, lo + 64 * w as isize);
        *cmp.add(w) = c;
        *qmp.add(w) = q;
        w += 1;
    }
    let mut b = cs;
    for _ in 0..nb {
        let (c, q, n) = block(buf, b as isize);
        *cmp.add(w) = c;
        *qmp.add(w) = q;
        let mut m = n;
        for k in 0..SLOTS {
            *nlp.add(nn + k) = (b - cs) as u32 + m.trailing_zeros();
            m = _blsr_u64(m);
        }
        let cnt = n.count_ones() as usize;
        if cnt > SLOTS {
            let mut k = nn + SLOTS;
            while m != 0 {
                *nlp.add(k) = (b - cs) as u32 + m.trailing_zeros();
                m = _blsr_u64(m);
                k += 1;
            }
        }
        nn += cnt;
        w += 1;
        b += 64;
    }
    let wend = (LB + 64 * nb + LA) / 64;
    while w < wend {
        let (c, q, _) = block(buf, lo + 64 * w as isize);
        *cmp.add(w) = c;
        *qmp.add(w) = q;
        w += 1;
    }
    *cmp.add(w) = 0;
    *qmp.add(w) = 0;
    *cmp.add(w + 1) = 0;
    *qmp.add(w + 1) = 0;
    nn
}

/// 64 bits of a mask array from bit offset bit; bits from 57 on may be zero instead of real.
#[inline(always)]
unsafe fn win(a: *const u64, bit: usize) -> u64 {
    ld64((a as *const u8).add(bit >> 3)) >> (bit & 7)
}

/// Exactly the 64 bits of a mask array from bit offset bit.
#[inline(always)]
unsafe fn win64(a: *const u64, bit: usize) -> u64 {
    let p = (a as *const u8).add(bit >> 3);
    let s = (bit & 7) as u32;
    (ld64(p) >> s) | ((ld64(p.add(8)) << 1) << (63 - s))
}

// ---------------------------------------------------------------------------------------
// Events.

#[cold]
#[inline(never)]
unsafe fn ev_cold<S: EventSink>(buf: &[u8], p: usize, e: usize, ta: *const u8, ia: *const u8, dc: &mut DCache, sink: &mut S) {
    if !ta.is_null() {
        dfill(dc, ld64(ta));
        dfill(dc, ld64(ia));
    }
    slow::event_line(buf, p, e, sink);
}

/// General exact decode of a well-framed event line (comma positions c[0..6] relative to p,
/// all quotes whole-field): odd / empty IDs, bad timestamps, odd products, bad prices.
/// Cold: the hot loop calls it only when the strict decode does not apply.
#[cold]
#[inline(never)]
unsafe fn ev_general<S: EventSink>(buf: &[u8], p: usize, e: usize, cpk: u64, dc: &mut DCache, sink: &mut S) {
    let lp = buf.as_ptr().add(p);
    let er = e - p;
    let c = |i: u32| ((cpk >> (8 * i)) & 0xFF) as usize;
    let (c0, c1, c2, c3, c4, c5) = (c(0), c(1), c(2), c(3), c(4), c(5));
    let sp = |s: usize, t: usize| -> (usize, usize, usize) {
        let q = (*lp.add(s) == b'"') as usize;
        (s + q, t - q, t - s - 2 * q)
    };
    let (a0, b0, n0) = sp(0, c0);
    let (a1, b1, n1) = sp(c0 + 1, c1);
    let (a2, b2, n2) = sp(c1 + 1, c2);
    let (a3, _, n3) = sp(c2 + 1, c3);
    let (a4, _, n4) = sp(c3 + 1, c4);
    let (a5, b5, n5) = sp(c4 + 1, c5);
    let (_, b6, n6) = sp(c5 + 1, er);
    let ta = lp.add(a2);
    let ia = lp.add(a3);
    let (t_secs, t_dd, t_off, i_secs, i_dd, rbad) = civil2(ta, ia);
    let (ts, fb_t) = ts_val(ta, n2, lp.add(b2), t_secs, t_dd, t_off, rbad, dc);
    let (ing, fb_i) = f1_val(ia, n3, i_secs, i_dd, rbad, dc);
    let x4 = ld64(lp.add(a4));
    let is_p = (n4 == 8) & (x4 == u64::from_le_bytes(*b"purchase"));
    let is_v = (n4 == 4) & (x4 as u32 == u32::from_le_bytes(*b"view"));
    let is_c = (n4 == 4) & (x4 as u32 == u32::from_le_bytes(*b"cart"));
    let kind = 3 - 3 * (is_v as u8) - 2 * (is_c as u8) - (is_p as u8);
    let (vals, lb) = quad([lp.add(b0), lp.add(b1), lp.add(b5), lp.add(b6)], [n0.wrapping_sub(1), n1.wrapping_sub(1), n5.wrapping_sub(1), n6]);
    if fb_t | fb_i | (is_p & (n6 > 8)) | (n0 == 0) {
        ev_cold(buf, p, e, ta, ia, dc, sink);
        return;
    }
    let c_e = (n0.wrapping_sub(2) < 8) & (*lp.add(a0) == b'e') & (lb & 1 == 0);
    let c_u = (n1.wrapping_sub(2) < 8) & (*lp.add(a1) == b'u') & (lb & 2 == 0);
    let c_p = (n5.wrapping_sub(2) < 8) & (*lp.add(a5) == b'p') & (lb & 4 == 0);
    let pv = vals[3];
    let price = if is_p & (n6 != 0) & (lb & 8 == 0) & (pv.wrapping_sub(1) < 10_000_000) { pv as u32 } else { 0 };
    let row = EvRow {
        eid: id_sel(n0, vals[0], c_e, p + a0),
        uid: id_sel(n1, vals[1], c_u, p + a1),
        ts,
        ingest: ing,
        kind,
        product: if c_p { vals[2] as u32 } else if n5 == 0 { PROD_EMPTY } else { PROD_ODD },
        price,
    };
    sink.event(&row);
}

/// Strict ts: a valid F1 / F2 / F3 (<= 16 digits) with a cached month. (value, ok)
#[inline(always)]
unsafe fn ts_strict(ta: *const u8, tn: usize, tb: *const u8, t_secs: i64, t_dd: usize, t_off: i64, rbad: u32, dc: &DCache) -> (i64, bool) {
    let t = ld256(ta);
    let tm = shape(t);
    let dm = _mm256_cmple_epu8_mask(_mm256_sub_epi8(t, splat(b'0')), splat(9));
    let ti = tn.min(31);
    let have = tm as u64 | ((dm as u64) << 32);
    let need = *TSHAPE.get_unchecked(ti);
    let civ = (tn == 20) | (tn == 25);
    let sign = *ta.add(19);
    let sgn = (tn != 25) | ((sign.wrapping_sub(b'+') & 0xFD) == 0);
    let key = ld64(ta);
    let slot = didx(key);
    let day = *dc.days.get_unchecked(slot).get_unchecked(t_dd);
    let cok = !civ | ((*dc.key.get_unchecked(slot) == key) & (day != DBAD));
    let off = select_unpredictable(sign == b'+', -t_off, t_off);
    let off = select_unpredictable(tn == 25, off, 0);
    let f3 = dec16(tb, tn);
    let tv = select_unpredictable(civ, day.wrapping_add(t_secs * 1000 + off), f3);
    let ok = ((have & need) == need) & ((rbad & *TRNG.get_unchecked(ti)) == 0) & cok & sgn;
    (tv, ok)
}

/// Strict ingest_ts: a valid F1 with a cached month. (value, ok)
#[inline(always)]
unsafe fn f1_strict(ia: *const u8, inn: usize, i_secs: i64, i_dd: usize, rbad: u32, dc: &DCache) -> (i64, bool) {
    let im = shape(ld256(ia));
    let key = ld64(ia);
    let slot = didx(key);
    let day = *dc.days.get_unchecked(slot).get_unchecked(i_dd);
    let ok = (inn == 20) & ((im & F1BITS) == F1BITS) & ((rbad & INGL) == 0) & (*dc.key.get_unchecked(slot) == key) & (day != DBAD);
    (day.wrapping_add(i_secs * 1000), ok)
}

/// event_type bytes mask by length: only lengths 4 (view, cart) and 8 (purchase) can match.
const KMASK: [u64; 10] = [0, 0, 0, 0, 0xFFFF_FFFF, 0, 0, 0, u64::MAX, 0];

#[inline(never)]
unsafe fn events_impl<S: EventSink>(buf: &[u8], sink: &mut S) {
    let len = buf.len();
    let base = buf.as_ptr();
    let mut dc = DCache::new();
    let mut st = Stage1::new();
    let mut ls = 0usize; // start of the next line
    let mut cs = 0usize;
    let nul = core::ptr::null::<u8>();
    while cs < len {
        let ce = (cs + CH).min(len);
        let nn = stage1::<2>(buf, cs, ce, &mut st);
        let cmb = st.cm.as_ptr();
        let qmb = st.qm.as_ptr();
        for i in 0..nn {
            let e = cs + *st.nl.get_unchecked(i) as usize;
            let p = ls;
            ls = e + 1;
            if p + LB < cs {
                ev_cold(buf, p, e, nul, nul, &mut dc, sink);
                continue;
            }
            let off = p + LB - cs;
            let er = e - p;
            let lp = base.add(p);
            // window A: fields 0..=2
            let ca = win(cmb, off);
            let qa = win(qmb, off);
            let c0 = ca.trailing_zeros() as usize;
            let r = _blsr_u64(ca);
            let c1 = r.trailing_zeros() as usize;
            let r = _blsr_u64(r);
            let c2 = r.trailing_zeros() as usize; // 64 if fewer than 3 commas
            let o = c2 + 1;
            let ta_ = _bzhi_u64(ca, o as u32);
            let ok_a = (c2 < er) & (c2 < 57) & quotes_ok(ta_, ta_ ^ (1u64 << (c2 & 63)), _bzhi_u64(qa, c2 as u32));
            // window B: fields 3..=6, bits [o, er]
            let eb = er.wrapping_sub(o);
            let cb = _bzhi_u64(win(cmb, off + o), eb as u32);
            let qb = _bzhi_u64(win(qmb, off + o), eb as u32);
            let tb = cb | (1u64 << (eb & 63));
            let ok_b = (eb < 57) & (cb.count_ones() == 3) & quotes_ok(tb, cb, qb);
            if !(ok_a & ok_b & (c0 != 0)) {
                ev_cold(buf, p, e, nul, nul, &mut dc, sink);
                continue;
            }
            let c3 = o + cb.trailing_zeros() as usize;
            let r = _blsr_u64(cb);
            let c4 = o + r.trailing_zeros() as usize;
            let r = _blsr_u64(r);
            let c5 = o + r.trailing_zeros() as usize;
            // quote bit per field: bit i of qf = field i starts (and so ends) with a quote
            let qf = _pext_u64(qa, (ta_ << 1) | 1) & 7 | (_pext_u64(qb, (cb << 1) | 1) << 3);
            let q = |i: u32| ((qf >> i) & 1) as usize;
            // value spans [a, b) relative to lp, n = b - a (quotes_ok guarantees b >= a)
            let (a0, b0) = (q(0), c0 - q(0));
            let (a1, b1) = (c0 + 1 + q(1), c1 - q(1));
            let (a2, b2) = (c1 + 1 + q(2), c2 - q(2));
            let (a3, b3) = (o + q(3), c3 - q(3));
            let (a4, b4) = (c3 + 1 + q(4), c4 - q(4));
            let (a5, b5) = (c4 + 1 + q(5), c5 - q(5));
            let b6 = er - q(6);
            let (n0, n1, n2, n3, n4, n5, n6) = (b0 - a0, b1 - a1, b2 - a2, b3 - a3, b4 - a4, b5 - a5, b6 - (c5 + 1 + q(6)));
            let (vals, lb) = quad([lp.add(b0), lp.add(b1), lp.add(b5), lp.add(b6)], [n0.wrapping_sub(1), n1.wrapping_sub(1), n5.wrapping_sub(1), n6]);
            let ta = lp.add(a2);
            let ia = lp.add(a3);
            let (t_secs, t_dd, t_off, i_secs, i_dd, rbad) = civil2(ta, ia);
            let (ts, ok_t) = ts_strict(ta, n2, lp.add(b2), t_secs, t_dd, t_off, rbad, &dc);
            let (ing, ok_i) = f1_strict(ia, n3, i_secs, i_dd, rbad, &dc);
            let x4 = ld64(lp.add(a4)) & *KMASK.get_unchecked(n4.min(9));
            let is_p = x4 == u64::from_le_bytes(*b"purchase");
            let is_v = x4 == u32::from_le_bytes(*b"view") as u64;
            let is_c = x4 == u32::from_le_bytes(*b"cart") as u64;
            let kind = 3 - 3 * (is_v as u8) - 2 * (is_c as u8) - (is_p as u8);
            let pv = vals[3];
            let row = EvRow {
                eid: vals[0],
                uid: vals[1],
                ts,
                ingest: ing,
                kind,
                product: vals[2] as u32,
                price: select_unpredictable(is_p, pv as u32, 0),
            };
            // strict: canonical IDs and product, known kind, valid priced purchase
            let lens = (n0.wrapping_sub(2) | n1.wrapping_sub(2) | n5.wrapping_sub(2)) < 8;
            let pre = (*lp.add(a0) == b'e') & (*lp.add(a1) == b'u') & (*lp.add(a5) == b'p');
            let pok = !is_p | ((n6.wrapping_sub(1) < 8) & (lb & 8 == 0) & (pv.wrapping_sub(1) < 10_000_000));
            let strict = ok_t & ok_i & lens & pre & (lb & 7 == 0) & (kind != 3) & pok;
            if !strict {
                let cpk = (c0 | c1 << 8 | c2 << 16 | c3 << 24 | c4 << 32 | c5 << 40) as u64;
                ev_general(buf, p, e, cpk, &mut dc, sink);
                continue;
            }
            sink.event(&row);
        }
        cs = ce;
    }
    if ls < len {
        slow::event_line(buf, ls, len, sink);
    }
}

// ---------------------------------------------------------------------------------------
// Clicks.

#[cold]
#[inline(never)]
unsafe fn cl_cold<S: ClickSink>(buf: &[u8], p: usize, e: usize, ta: *const u8, dc: &mut DCache, sink: &mut S) {
    if !ta.is_null() {
        dfill(dc, ld64(ta));
    }
    slow::click_line(buf, p, e, sink);
}

/// General exact decode of a well-framed click line (comma positions c[0..3] relative to p,
/// all quotes whole-field). Cold: the hot loop calls it only when the strict decode fails.
#[cold]
#[inline(never)]
unsafe fn cl_general<S: ClickSink>(buf: &[u8], p: usize, e: usize, cpk: u64, dc: &mut DCache, sink: &mut S) {
    let lp = buf.as_ptr().add(p);
    let er = e - p;
    let c = |i: u32| ((cpk >> (8 * i)) & 0xFF) as usize;
    let (c0, c1, c2) = (c(0), c(1), c(2));
    let sp = |s: usize, t: usize| -> (usize, usize, usize) {
        let q = (*lp.add(s) == b'"') as usize;
        (s + q, t - q, t - s - 2 * q)
    };
    let (a0, b0, n0) = sp(0, c0);
    let (a1, b1, n1) = sp(c0 + 1, c1);
    let (a2, b2, n2) = sp(c1 + 1, c2);
    let (a3, b3, n3) = sp(c2 + 1, er);
    let ta = lp.add(a3);
    let (t_secs, t_dd, t_off, _, _, rbad) = civil2(ta, ta);
    let (ts, fb_t) = ts_val(ta, n3, lp.add(b3), t_secs, t_dd, t_off, rbad, dc);
    let (vals, lb) = quad([lp.add(b0), lp.add(b1), lp.add(b2), lp.add(8)], [n0.wrapping_sub(1), n1.wrapping_sub(1), n2.wrapping_sub(2), 0]);
    if fb_t | (n0 == 0) {
        cl_cold(buf, p, e, ta, dc, sink);
        return;
    }
    let c_k = (n0.wrapping_sub(2) < 8) & (*lp.add(a0) == b'k') & (lb & 1 == 0);
    let c_u = (n1.wrapping_sub(2) < 8) & (*lp.add(a1) == b'u') & (lb & 2 == 0);
    let c_c = (n2.wrapping_sub(3) < 8) & (ld16(lp.add(a2)) == u16::from_le_bytes(*b"c_")) & (lb & 4 == 0);
    let row = ClRow {
        cid: id_sel(n0, vals[0], c_k, p + a0),
        uid: id_sel(n1, vals[1], c_u, p + a1),
        campaign: if c_c { vals[2] as u32 } else if n2 == 0 { CAMP_EMPTY } else { CAMP_ODD },
        ts,
    };
    sink.click(&row);
}

#[inline(never)]
unsafe fn clicks_impl<S: ClickSink>(buf: &[u8], sink: &mut S) {
    let len = buf.len();
    let base = buf.as_ptr();
    let mut dc = DCache::new();
    let mut st = Stage1::new();
    let mut ls = 0usize;
    let mut cs = 0usize;
    let nul = core::ptr::null::<u8>();
    while cs < len {
        let ce = (cs + CH).min(len);
        let nn = stage1::<3>(buf, cs, ce, &mut st);
        let cmb = st.cm.as_ptr();
        let qmb = st.qm.as_ptr();
        for i in 0..nn {
            let e = cs + *st.nl.get_unchecked(i) as usize;
            let p = ls;
            ls = e + 1;
            if p + LB < cs {
                cl_cold(buf, p, e, nul, &mut dc, sink);
                continue;
            }
            let off = p + LB - cs;
            let er = e - p;
            let lp = base.add(p);
            let cm = _bzhi_u64(win64(cmb, off), er as u32);
            let qm = _bzhi_u64(win64(qmb, off), er as u32);
            let c0 = cm.trailing_zeros() as usize;
            let r = _blsr_u64(cm);
            let c1 = r.trailing_zeros() as usize;
            let r = _blsr_u64(r);
            let c2 = r.trailing_zeros() as usize;
            let tm = cm | (1u64 << (er & 63));
            let ok = (er < 64) & (cm.count_ones() == 3) & quotes_ok(tm, cm, qm) & (c0 != 0);
            if !ok {
                cl_cold(buf, p, e, nul, &mut dc, sink);
                continue;
            }
            let qf = _pext_u64(qm, (cm << 1) | 1);
            let q = |i: u32| ((qf >> i) & 1) as usize;
            let (a0, b0) = (q(0), c0 - q(0));
            let (a1, b1) = (c0 + 1 + q(1), c1 - q(1));
            let (a2, b2) = (c1 + 1 + q(2), c2 - q(2));
            let (a3, b3) = (c2 + 1 + q(3), er - q(3));
            let (n0, n1, n2, n3) = (b0 - a0, b1 - a1, b2 - a2, b3 - a3);
            let (vals, lb) = quad([lp.add(b0), lp.add(b1), lp.add(b2), lp.add(8)], [n0.wrapping_sub(1), n1.wrapping_sub(1), n2.wrapping_sub(2), 0]);
            let ta = lp.add(a3);
            let (t_secs, t_dd, t_off, _, _, rbad) = civil2(ta, ta);
            let (ts, ok_t) = ts_strict(ta, n3, lp.add(b3), t_secs, t_dd, t_off, rbad, &dc);
            let row = ClRow { cid: vals[0], uid: vals[1], campaign: vals[2] as u32, ts };
            let lens = (n0.wrapping_sub(2) | n1.wrapping_sub(2) | n2.wrapping_sub(3)) < 8;
            let pre = (*lp.add(a0) == b'k') & (*lp.add(a1) == b'u') & (ld16(lp.add(a2)) == u16::from_le_bytes(*b"c_"));
            if !(ok_t & lens & pre & (lb & 7 == 0)) {
                let cpk = (c0 | c1 << 8 | c2 << 16) as u64;
                cl_general(buf, p, e, cpk, &mut dc, sink);
                continue;
            }
            sink.click(&row);
        }
        cs = ce;
    }
    if ls < len {
        slow::click_line(buf, ls, len, sink);
    }
}

// ---------------------------------------------------------------------------------------
/// Per-line slow path: a copy of the reference (k_ref.rs) for one line buf[s..e].
mod slow {
    use crate::iface::*;

    fn field_value(buf: &[u8], s: usize, e: usize) -> Option<(usize, usize)> {
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

    fn parse_id(buf: &[u8], span: (usize, usize), prefix: u8) -> u64 {
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

    fn parse_product(v: &[u8]) -> u32 {
        if v.is_empty() {
            return PROD_EMPTY;
        }
        if v[0] != b'p' {
            return PROD_ODD;
        }
        canonical_decimal(&v[1..]).unwrap_or(PROD_ODD)
    }

    fn parse_campaign(v: &[u8]) -> u32 {
        if v.is_empty() {
            return CAMP_EMPTY;
        }
        if !v.starts_with(b"c_") {
            return CAMP_ODD;
        }
        canonical_decimal(&v[2..]).unwrap_or(CAMP_ODD)
    }

    fn parse_kind(v: &[u8]) -> u8 {
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

    fn parse_f1(v: &[u8]) -> i64 {
        if v.len() == 20 && v[19] == b'Z' {
            if let Some(ms) = civil_ms(&v[..19]) {
                return ms;
            }
        }
        TS_BAD
    }

    fn parse_ts(v: &[u8]) -> i64 {
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
    pub fn event_line<S: EventSink>(buf: &[u8], start: usize, end: usize, sink: &mut S) {
        let Some(v) = split_line::<7>(buf, start, end) else { return };
        if v[0].0 == v[0].1 {
            return;
        }
        let val = |i: usize| &buf[v[i].0..v[i].1];
        let kind = parse_kind(val(4));
        let row = EvRow {
            eid: parse_id(buf, v[0], b'e'),
            uid: parse_id(buf, v[1], b'u'),
            ts: parse_ts(val(2)),
            ingest: parse_f1(val(3)),
            kind,
            product: parse_product(val(5)),
            price: if kind == KIND_PURCHASE { parse_price(val(6)) } else { 0 },
        };
        sink.event(&row);
    }

    #[cold]
    #[inline(never)]
    pub fn click_line<S: ClickSink>(buf: &[u8], start: usize, end: usize, sink: &mut S) {
        let Some(v) = split_line::<4>(buf, start, end) else { return };
        if v[0].0 == v[0].1 {
            return;
        }
        let val = |i: usize| &buf[v[i].0..v[i].1];
        let row = ClRow {
            cid: parse_id(buf, v[0], b'k'),
            uid: parse_id(buf, v[1], b'u'),
            campaign: parse_campaign(val(2)),
            ts: parse_ts(val(3)),
        };
        sink.click(&row);
    }
}
