#![allow(unexpected_cfgs)]
//! k_zmm: exact CSV parse kernel (AVX-512 F/BW/VL/DQ + BMI, no gathers, no per-byte loop),
//! checked row by row against k_ref. Integration: copy this file; it needs only iface.rs and
//! the pub parse functions of k_ref.rs (used for slow lines and field fallbacks).
//!
//! Three passes per 8 KB chunk of buf:
//! 1. Block pass (64-byte blocks): masks of b',', b'\n', b'"'. All separator (b',' | b'\n')
//!    positions are compressed (vpcompressd) into one i32 stream, each entry
//!    ((POS_OFF + pos - chunk) << 3) | closing-quote-before | opening-quote-after << 1 |
//!    is-newline << 2. The quote rule is checked per block with bit masks (S = field starts,
//!    F = field last bytes, O = quotes on S, Cl = quotes on F; bad = a quote on neither, on
//!    both, or an opening quote whose field does not end with a closing one, via the carry
//!    of !sep + O); bad bit positions go to a second stream.
//! 2. Line pass. FAST PATH CONDITION: the next N stream entries (N = 7 events, 4 clicks)
//!    have the newline tag on the N-th entry only, and no bad bit lies in [line start, that
//!    b'\n']. Exactly the lines k_ref accepts as well-formed pass this test. Fast lines put
//!    their N tagged entries in a 16-line batch. Every other line (wrong field count, bad
//!    quotes, empty line, the last line without b'\n', a line spanning a chunk with > 64
//!    separators) is SLOW: it keeps its place in the batch and k_ref code parses it.
//! 3. Batch pass (16 lines): value spans from the tags (16 lanes), one scalar window copy
//!    per field (16 or 32 bytes ending at the value end), then 8-lane decoders: Ids (prefix +
//!    1..=8 of [0-9a-z], no leading '0', base 36 via maddubs/madd), event_type, product /
//!    campaign (canonical decimal; exact, never falls back), price (purchase only; len 1..=8
//!    exact), ts and ingest_ts: F1 / F2 with full proleptic Gregorian arithmetic for years
//!    0000..=9999 and F3 of 1..=16 digits. FIELD FALLBACK (k_ref function on the k_ref span of
//!    that field): non-canonical or empty-prefix Ids, ts not in those shapes, ingest not F1,
//!    price longer than 8 bytes. Rows go to the sink in line order.

use crate::iface::*;
use crate::k_ref::{parse_campaign, parse_f1, parse_id, parse_kind, parse_price, parse_product, parse_ts, split_line};
use core::arch::x86_64::*;

#[cfg(zmm_stats)]
pub static STATS: [core::sync::atomic::AtomicU64; 8] = [const { core::sync::atomic::AtomicU64::new(0) }; 8];
#[inline(always)]
#[allow(unused_variables)]
fn stat(i: usize) {
    #[cfg(zmm_stats)]
    STATS[i].fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}
#[cfg(zmm_stats)]
pub fn dump_stats(tag: &str) {
    let v: Vec<u64> = STATS.iter().map(|a| a.swap(0, core::sync::atomic::Ordering::Relaxed)).collect();
    eprintln!("STATS {tag} lines_slow={} id_slow={} ts_slow={} f1_slow={} prod_slow={} camp_slow={} date_miss={} fast={}", v[0], v[1], v[2], v[3], v[4], v[5], v[6], v[7]);
}


pub struct Zmm;

impl Kernel for Zmm {
    const NAME: &'static str = "zmm";
    fn events<S: EventSink>(buf: &[u8], sink: &mut S) {
        unsafe { events_impl(buf, sink) }
    }
    fn clicks<S: ClickSink>(buf: &[u8], sink: &mut S) {
        unsafe { clicks_impl(buf, sink) }
    }
}

const CHUNK: usize = 8192;
const SEP_CAP: usize = CHUNK + 128;
const B: usize = 16;
const POS_OFF: i32 = 1 << 27;

/// (separators, newlines, quotes) masks of the 64 bytes at blk; bytes at or past n are 0.
#[inline(always)]
unsafe fn masks_at(p: *const u8, blk: usize, n: usize) -> (u64, u64, u64) {
    unsafe {
        let z = _mm512_loadu_si512(p.add(blk) as *const _);
        let c = _mm512_cmpeq_epi8_mask(z, _mm512_set1_epi8(b',' as i8));
        let nl = _mm512_cmpeq_epi8_mask(z, _mm512_set1_epi8(b'\n' as i8));
        let q = _mm512_cmpeq_epi8_mask(z, _mm512_set1_epi8(b'"' as i8));
        let rem = n - blk;
        if rem < 64 {
            let vm = (1u64 << rem) - 1;
            return ((c | nl) & vm, nl & vm, q & vm);
        }
        (c | nl, nl, q)
    }
}

/// Column batch of up to B lines (positions are tagged, chunk-relative + POS_OFF).
#[repr(C, align(64))]
struct Batch<const N: usize> {
    rows: [[i32; 8]; B],
    v0: [i32; B],
    start: [usize; B],
    end: [usize; B],
    n: usize,
    slow: u32,
}

/// The block and line passes; calls flush(batch, cb) whenever the batch is full and at the
/// end of every chunk (batch positions are relative to the chunk start cb).
/// Separator stream entries: ((POS_OFF + pos - cb) << 3) | closing-quote-before | opening-quote-after << 1 | newline << 2.
#[inline(always)]
unsafe fn drive<const N: usize>(buf: &[u8], bt: &mut Batch<N>, mut flush: impl FnMut(&mut Batch<N>, usize)) {
    unsafe {
        let n = buf.len();
        let p = buf.as_ptr();
        let mut sep_m = core::mem::MaybeUninit::<[i32; SEP_CAP]>::uninit();
        let mut bad_m = core::mem::MaybeUninit::<[usize; SEP_CAP]>::uninit();
        let sep = sep_m.as_mut_ptr() as *mut i32;
        let bad = bad_m.as_mut_ptr() as *mut usize;
        let iota8 = _mm512_slli_epi32::<3>(_mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15));
        let mut start = 0usize;
        let mut blk = 0usize;
        let mut nsep = 0usize; // carried-over separators of a partial line
        let mut nbad = 0usize;
        let mut force_slow = false;
        let mut prev_sep_top = 1u64; // buffer start counts as after a separator
        let mut prev_cl_top = 0u64;
        let mut carry = 0u8;
        bt.n = 0;
        bt.slow = 0;
        while blk < n {
            let cb = blk;
            let stop = (blk + CHUNK).min(n);
            let base0 = (POS_OFF as isize - cb as isize) as i32;
            while blk < stop {
                let (sp, m, q) = masks_at(p, blk, n);
                let (ns0, nq0) = if blk + 64 < n {
                    let b = *p.add(blk + 64);
                    ((b == b',' || b == b'\n') as u64, (b == b'"') as u64)
                } else {
                    (0, 0)
                };
                let f = (sp >> 1) | (ns0 << 63);
                let s = (sp << 1) | prev_sep_top;
                prev_sep_top = sp >> 63;
                let o = q & s;
                let cl = q & f;
                let cl1 = (cl << 1) | prev_cl_top;
                prev_cl_top = cl >> 63;
                let oa = (o >> 1) | ((nq0 & (sp >> 63)) << 63);
                let mut sum = 0u64;
                carry = _addcarry_u64(carry, !sp, o, &mut sum);
                let mut b = (q & !(o | cl)) | (o & cl) | ((sum & sp) ^ cl1);
                if b != 0 {
                    while b != 0 {
                        *bad.add(nbad) = blk + b.trailing_zeros() as usize;
                        nbad += 1;
                        b &= b.wrapping_sub(1);
                    }
                }
                let base = _mm512_add_epi32(iota8, _mm512_set1_epi32((base0 + blk as i32) << 3));
                let tags = _mm512_or_si512(
                    _mm512_or_si512(_mm512_maskz_mov_epi8(cl1, _mm512_set1_epi8(1)), _mm512_maskz_mov_epi8(oa, _mm512_set1_epi8(2))),
                    _mm512_maskz_mov_epi8(m, _mm512_set1_epi8(4)),
                );
                let c1 = (sp as u16).count_ones() as usize;
                let c2 = c1 + ((sp >> 16) as u16).count_ones() as usize;
                let c3 = c2 + ((sp >> 32) as u16).count_ones() as usize;
                let o0 = sep.add(nsep);
                macro_rules! piece {
                    ($k:literal, $off:expr) => {{
                        let t = _mm512_cvtepu8_epi32(_mm512_extracti32x4_epi32::<$k>(tags));
                        let idx = _mm512_add_epi32(_mm512_add_epi32(base, _mm512_set1_epi32(128 * $k)), t);
                        _mm512_storeu_si512(o0.add($off) as *mut _, _mm512_maskz_compress_epi32((sp >> (16 * $k)) as u16, idx));
                    }};
                }
                piece!(0, 0);
                piece!(1, c1);
                piece!(2, c2);
                piece!(3, c3);
                nsep += sp.count_ones() as usize;
                blk += 64;
            }
            blk = stop;
            *bad.add(nbad) = usize::MAX; // sentinel
            // line pass
            let mut j = 0usize;
            let mut bi = 0usize;
            let rel0 = POS_OFF as isize - cb as isize;
            let abs = |x: i32| ((x >> 3) as isize - rel0) as usize;
            loop {
                let li = bt.n;
                if !force_slow && j + N <= nsep {
                    let row = _mm256_loadu_si256(sep.add(j) as *const __m256i);
                    let nlk = _mm256_test_epi32_mask(row, _mm256_set1_epi32(4)) & ((1u8 << N) - 1);
                    let e = abs(*sep.add(j + N - 1));
                    if nlk == 1 << (N - 1) && *bad.add(bi) > e {
                        _mm256_store_si256(bt.rows.as_mut_ptr().add(li) as *mut __m256i, row);
                        *bt.v0.get_unchecked_mut(li) = (start as isize + rel0) as i32 + (*p.add(start) == b'"') as i32;
                        *bt.start.get_unchecked_mut(li) = start;
                        *bt.end.get_unchecked_mut(li) = e;
                        j += N;
                        start = e + 1;
                        bt.n = li + 1;
                        if li + 1 == B {
                            flush(bt, cb);
                            bt.n = 0;
                            bt.slow = 0;
                        }
                        continue;
                    }
                }
                // slow line, or the line does not end in this chunk
                let mut idx = j;
                while idx < nsep && *sep.add(idx) & 4 == 0 {
                    idx += 1;
                }
                if idx == nsep {
                    break;
                }
                let e = abs(*sep.add(idx));
                force_slow = false;
                bt.slow |= 1 << li;
                let safe = ((start as isize + rel0) as i32) << 3;
                _mm256_store_si256(bt.rows.as_mut_ptr().add(li) as *mut __m256i, _mm256_set1_epi32(safe));
                *bt.v0.get_unchecked_mut(li) = safe >> 3;
                while *bad.add(bi) <= e {
                    bi += 1;
                }
                *bt.start.get_unchecked_mut(li) = start;
                *bt.end.get_unchecked_mut(li) = e;
                j = idx + 1;
                start = e + 1;
                bt.n = li + 1;
                if li + 1 == B {
                    flush(bt, cb);
                    bt.n = 0;
                    bt.slow = 0;
                }
            }
            if bt.n > 0 {
                flush(bt, cb);
                bt.n = 0;
                bt.slow = 0;
            }
            // carry over the partial line's separators (re-based) and bad bits
            let r = nsep - j;
            let rb = nbad - bi;
            let dd = (stop - cb) as i32;
            let mut ok = r <= 64 && rb <= 64;
            if ok {
                for t in 0..r {
                    let x = *sep.add(j + t);
                    if (x >> 3) < dd + 4096 {
                        ok = false;
                    }
                    *sep.add(t) = x - (dd << 3);
                }
            }
            if ok {
                for t in 0..rb {
                    *bad.add(t) = *bad.add(bi + t);
                }
                nsep = r;
                nbad = rb;
            } else {
                force_slow = true;
                nsep = 0;
                nbad = 0;
            }
        }
        if start < n {
            // last line without b'\n'
            bt.n = 1;
            bt.slow = 1;
            bt.start[0] = start;
            bt.end[0] = n;
            flush(bt, 0);
        }
    }
}

/// 16 rows of 8 dwords -> 8 columns of 16 dwords (24 vpermt2d).
#[inline(always)]
unsafe fn transpose16x8(rows: &[[i32; 8]; B]) -> [__m512i; 8] {
    unsafe {
        let z: [__m512i; 8] = core::array::from_fn(|m| _mm512_load_si512(rows.as_ptr().add(2 * m) as *const _));
        // level 1: rows 4m..4m+3, cols 0..3 (s=0) / 4..7 (s=1), column-major
        let i1a = _mm512_setr_epi32(0, 8, 16, 24, 1, 9, 17, 25, 2, 10, 18, 26, 3, 11, 19, 27);
        let i1b = _mm512_setr_epi32(4, 12, 20, 28, 5, 13, 21, 29, 6, 14, 22, 30, 7, 15, 23, 31);
        let p: [[__m512i; 2]; 4] = core::array::from_fn(|m| [
            _mm512_permutex2var_epi32(z[2 * m], i1a, z[2 * m + 1]),
            _mm512_permutex2var_epi32(z[2 * m], i1b, z[2 * m + 1]),
        ]);
        // level 2: rows 8h..8h+7, columns (2c, 2c+1) of half s
        let i2a = _mm512_setr_epi32(0, 1, 2, 3, 16, 17, 18, 19, 4, 5, 6, 7, 20, 21, 22, 23);
        let i2b = _mm512_setr_epi32(8, 9, 10, 11, 24, 25, 26, 27, 12, 13, 14, 15, 28, 29, 30, 31);
        let q: [[__m512i; 4]; 2] = core::array::from_fn(|h| [
            _mm512_permutex2var_epi32(p[2 * h][0], i2a, p[2 * h + 1][0]),
            _mm512_permutex2var_epi32(p[2 * h][0], i2b, p[2 * h + 1][0]),
            _mm512_permutex2var_epi32(p[2 * h][1], i2a, p[2 * h + 1][1]),
            _mm512_permutex2var_epi32(p[2 * h][1], i2b, p[2 * h + 1][1]),
        ]);
        // level 3: column c = rows 0..7 from q[0], rows 8..15 from q[1]
        let i3a = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 16, 17, 18, 19, 20, 21, 22, 23);
        let i3b = _mm512_setr_epi32(8, 9, 10, 11, 12, 13, 14, 15, 24, 25, 26, 27, 28, 29, 30, 31);
        core::array::from_fn(|c| {
            let t = c / 2;
            if c % 2 == 0 {
                _mm512_permutex2var_epi32(q[0][t], i3a, q[1][t])
            } else {
                _mm512_permutex2var_epi32(q[0][t], i3b, q[1][t])
            }
        })
    }
}

/// Value spans of the batch: vs[k], ve[k] (16 lanes, tagged-relative positions untagged).
#[inline(always)]
unsafe fn spans<const N: usize>(bt: &Batch<N>) -> ([__m512i; N], [__m512i; N]) {
    unsafe {
        let one = _mm512_set1_epi32(1);
        let mut vs = [_mm512_setzero_si512(); N];
        let mut ve = [_mm512_setzero_si512(); N];
        vs[0] = _mm512_loadu_si512(bt.v0.as_ptr() as *const _);
        let cols = transpose16x8(&bt.rows);
        for k in 0..N {
            let f = cols[k];
            let pos = _mm512_srli_epi32::<3>(f);
            let cl = _mm512_and_si512(f, one);
            let oa = _mm512_and_si512(_mm512_srli_epi32::<1>(f), one);
            ve[k] = _mm512_sub_epi32(pos, cl);
            if k + 1 < N {
                vs[k + 1] = _mm512_add_epi32(_mm512_add_epi32(pos, one), oa);
            }
        }
        (vs, ve)
    }
}

#[inline(always)]
unsafe fn lo8(v: __m512i) -> __m256i {
    unsafe { _mm512_castsi512_si256(v) }
}
#[inline(always)]
unsafe fn hi8(v: __m512i) -> __m256i {
    unsafe { _mm512_extracti64x4_epi64::<1>(v) }
}

/// Digit-byte mask: the top n bytes of each qword (n in 1..=8; other n: garbage, caller masks).
#[inline(always)]
unsafe fn keep_mask(n: __m512i) -> __mmask64 {
    unsafe {
        let sh = _mm512_slli_epi64::<3>(_mm512_sub_epi64(_mm512_set1_epi64(8), n));
        _mm512_movepi8_mask(_mm512_sllv_epi64(_mm512_set1_epi64(-1), sh))
    }
}

/// Lanes with any byte set in mask k.
#[inline(always)]
unsafe fn any_byte(k: __mmask64) -> __mmask8 {
    unsafe {
        let v = _mm512_movm_epi8(k);
        _mm512_test_epi64_mask(v, v)
    }
}

/// 8 Ids: lo = 8 bytes ending at the value end, hi = 8 bytes at the value start, len =
/// value length. Returns (values, canonical lanes).
#[inline(always)]
unsafe fn ids8(lo: __m512i, hi: __m512i, len: __m512i, prefix: u8) -> (__m512i, __mmask8) {
    unsafe {
        let one = _mm512_set1_epi64(1);
        let n = _mm512_sub_epi64(len, one);
        let nok = _mm512_cmplt_epu64_mask(_mm512_sub_epi64(n, one), _mm512_set1_epi64(8));
        let keep = keep_mask(n);
        let d = _mm512_sub_epi8(lo, _mm512_set1_epi8(b'0' as i8));
        let l = _mm512_sub_epi8(lo, _mm512_set1_epi8(b'a' as i8));
        let isd = _mm512_cmplt_epu8_mask(d, _mm512_set1_epi8(10));
        let isl = _mm512_cmplt_epu8_mask(l, _mm512_set1_epi8(26));
        let bad = any_byte(keep & !(isd | isl));
        let v = _mm512_mask_blend_epi8(isd, _mm512_add_epi8(l, _mm512_set1_epi8(10)), d);
        let v = _mm512_maskz_mov_epi8(keep, v);
        let a = _mm512_maddubs_epi16(v, _mm512_set1_epi16(0x0124));
        let b = _mm512_madd_epi16(a, _mm512_set1_epi32(0x0001_0510));
        let val = _mm512_add_epi64(_mm512_mul_epu32(b, _mm512_set1_epi64(1_679_616)), _mm512_srli_epi64::<32>(b));
        let ff = _mm512_set1_epi64(0xFF);
        let pre = _mm512_cmpeq_epi64_mask(_mm512_and_si512(hi, ff), _mm512_set1_epi64(prefix as i64));
        let b1 = _mm512_and_si512(_mm512_srli_epi64::<8>(hi), ff);
        let lz = _mm512_cmpneq_epi64_mask(b1, _mm512_set1_epi64(b'0' as i64)) | _mm512_cmpeq_epi64_mask(n, one);
        (val, nok & !bad & pre & lz)
    }
}

/// 8 canonical decimals of 1..=8 digits after a prefix of plen bytes (hi = 8 bytes at the
/// value start holds the prefix): (values, canonical lanes). pre_ok: prefix matched.
#[inline(always)]
unsafe fn dec8v(lo: __m512i, hi: __m512i, len: __m512i, plen: i64, pre_ok: __mmask8) -> (__m512i, __mmask8) {
    unsafe {
        let one = _mm512_set1_epi64(1);
        let n = _mm512_sub_epi64(len, _mm512_set1_epi64(plen));
        let nok = _mm512_cmplt_epu64_mask(_mm512_sub_epi64(n, one), _mm512_set1_epi64(8));
        let keep = keep_mask(n);
        let d = _mm512_sub_epi8(lo, _mm512_set1_epi8(b'0' as i8));
        let isd = _mm512_cmplt_epu8_mask(d, _mm512_set1_epi8(10));
        let bad = any_byte(keep & !isd);
        let v = _mm512_maskz_mov_epi8(keep, d);
        let a = _mm512_maddubs_epi16(v, _mm512_set1_epi16(0x010A));
        let b = _mm512_madd_epi16(a, _mm512_set1_epi32(0x0001_0064));
        let val = _mm512_add_epi64(_mm512_mul_epu32(b, _mm512_set1_epi64(10_000)), _mm512_srli_epi64::<32>(b));
        let b1 = _mm512_and_si512(_mm512_srlv_epi64(hi, _mm512_set1_epi64(8 * plen)), _mm512_set1_epi64(0xFF));
        let lz = _mm512_cmpneq_epi64_mask(b1, _mm512_set1_epi64(b'0' as i64)) | _mm512_cmpeq_epi64_mask(n, one);
        (val, nok & !bad & lz & pre_ok)
    }
}

/// 8 prices (digits only, leading zeros allowed, 1..=10^7), len 0..=8; lanes with len > 8
/// are reported in the second mask (caller falls back).
#[inline(always)]
unsafe fn price8(lo: __m512i, len: __m512i) -> (__m512i, __mmask8) {
    unsafe {
        let one = _mm512_set1_epi64(1);
        let nok = _mm512_cmplt_epu64_mask(_mm512_sub_epi64(len, one), _mm512_set1_epi64(8));
        let keep = keep_mask(len);
        let d = _mm512_sub_epi8(lo, _mm512_set1_epi8(b'0' as i8));
        let isd = _mm512_cmplt_epu8_mask(d, _mm512_set1_epi8(10));
        let bad = any_byte(keep & !isd);
        let v = _mm512_maskz_mov_epi8(keep, d);
        let a = _mm512_maddubs_epi16(v, _mm512_set1_epi16(0x010A));
        let b = _mm512_madd_epi16(a, _mm512_set1_epi32(0x0001_0064));
        let val = _mm512_add_epi64(_mm512_mul_epu32(b, _mm512_set1_epi64(10_000)), _mm512_srli_epi64::<32>(b));
        let inr = _mm512_cmplt_epu64_mask(_mm512_sub_epi64(val, one), _mm512_set1_epi64(10_000_000));
        let good = nok & !bad & inr;
        let long = _mm512_cmpgt_epu64_mask(len, _mm512_set1_epi64(8));
        (_mm512_maskz_mov_epi64(good, val), long)
    }
}


/// Byte b (0..8) of each qword lane.
#[inline(always)]
unsafe fn byte_at(v: __m512i, b: u32) -> __m512i {
    unsafe { _mm512_and_si512(_mm512_srlv_epi64(v, _mm512_set1_epi64(8 * b as i64)), _mm512_set1_epi64(0xFF)) }
}
#[inline(always)]
fn rep8(x: u8) -> u64 {
    (x as u64) * 0x0101_0101_0101_0101
}
/// Lanes whose bytes are digits where dig has a bit and equal tmpl where sep has a bit
/// (dig, sep: per-lane byte masks, replicated to all 8 lanes).
#[inline(always)]
unsafe fn shape_ok(v: __m512i, dig: u8, sep: u8, tmpl: u64) -> __mmask8 {
    unsafe {
        let dk = _mm512_cmplt_epu8_mask(_mm512_sub_epi8(v, _mm512_set1_epi8(b'0' as i8)), _mm512_set1_epi8(10));
        let sk = _mm512_cmpeq_epi8_mask(v, _mm512_set1_epi64(tmpl as i64));
        let good = (dk & rep8(dig)) | (sk & rep8(sep));
        !any_byte(!good & rep8(dig | sep))
    }
}
/// Byte mask: 0xFF in byte i for every bit i of m.
const fn bytemask(m: u8) -> i64 {
    let mut x = 0u64;
    let mut i = 0;
    while i < 8 {
        if m >> i & 1 != 0 {
            x |= 0xFF << (8 * i);
        }
        i += 1;
    }
    x as i64
}
/// x*10 + (x >> 8) per qword (two adjacent digit bytes -> their 2-digit value).
#[inline(always)]
unsafe fn pair10(d: __m512i) -> __m512i {
    unsafe { _mm512_add_epi64(_mm512_add_epi64(_mm512_slli_epi64::<3>(d), _mm512_slli_epi64::<1>(d)), _mm512_srli_epi64::<8>(d)) }
}

/// F1 "YYYY-MM-DDTHH:MM:SSZ" (f2 = false) or F2 "YYYY-MM-DDTHH:MM:SS+HH:MM" (f2 = true,
/// g3 = bytes 24..32) from the qwords at value offsets 0, 8, 16. Full proleptic Gregorian
/// arithmetic for years 0000..=9999 (same as k_ref). Returns (epoch ms, valid lanes).
#[inline(always)]
unsafe fn civ8(g0: __m512i, g1: __m512i, g2: __m512i, g3: __m512i, f2: bool) -> (__m512i, __mmask8) {
    unsafe { if f2 { civ8m::<true>(g0, g1, g2, g3, 0xFF) } else { civ8m::<false>(g0, g1, g2, g3, 0) } }
}

/// civ8 with a per-lane F2 mask (other lanes F1).
#[inline(always)]
unsafe fn civ8m<const F2: bool>(g0: __m512i, g1: __m512i, g2: __m512i, g3: __m512i, f2m: __mmask8) -> (__m512i, __mmask8) {
    unsafe {
        let z = _mm512_set1_epi64(0x3030_3030_3030_3030);
        let ok0 = shape_ok(g0, 0x6F, 0x90, u64::from_le_bytes(*b"0000-00-"));
        let ok1 = shape_ok(g1, 0xDB, 0x24, u64::from_le_bytes(*b"00T00:00"));
        let ok2f2 = if F2 {
            // ":SS?HH:M" with ? = '+' or '-'
            let k = shape_ok(g2, 0xB6, 0x41, u64::from_le_bytes(*b":00+00:0"));
            let sg = byte_at(g2, 3);
            let pm = _mm512_cmpeq_epi64_mask(sg, _mm512_set1_epi64(b'+' as i64)) | _mm512_cmpeq_epi64_mask(sg, _mm512_set1_epi64(b'-' as i64));
            let d3 = _mm512_sub_epi64(_mm512_and_si512(g3, _mm512_set1_epi64(0xFF)), _mm512_set1_epi64(b'0' as i64));
            k & pm & _mm512_cmplt_epu64_mask(d3, _mm512_set1_epi64(10))
        } else {
            0
        };
        let ok2f1 = shape_ok(g2, 0x06, 0x09, u64::from_le_bytes(*b":00Z0000"));
        let ok2 = (ok2f2 & f2m) | (ok2f1 & !f2m);
        // digit bytes only (separator bytes zeroed so pair10 cannot carry)
        const M0: i64 = bytemask(0x6F);
        const M1: i64 = bytemask(0xDB);
        const M2: i64 = bytemask(0xB6);
        let v0 = pair10(_mm512_and_si512(_mm512_sub_epi8(g0, z), _mm512_set1_epi64(M0)));
        let v1 = pair10(_mm512_and_si512(_mm512_sub_epi8(g1, z), _mm512_set1_epi64(M1)));
        let d2 = _mm512_and_si512(_mm512_sub_epi8(g2, z), _mm512_set1_epi64(M2));
        let v2 = pair10(d2);
        let yhi = byte_at(v0, 0);
        let ylo = byte_at(v0, 2);
        let m = byte_at(v0, 5);
        let dd = byte_at(v1, 0);
        let hh = byte_at(v1, 3);
        let mi = byte_at(v1, 6);
        let ss = byte_at(v2, 1);
        let one = _mm512_set1_epi64(1);
        let mok = _mm512_cmplt_epu64_mask(_mm512_sub_epi64(m, one), _mm512_set1_epi64(12));
        // days in month (index m, 0..=15), leap February added below
        let dim_lo = _mm512_setr_epi64(0, 31, 28, 31, 30, 31, 30, 31);
        let dim_hi = _mm512_setr_epi64(31, 30, 31, 30, 31, 0, 0, 0);
        let mi16 = _mm512_and_si512(m, _mm512_set1_epi64(15));
        let dim = _mm512_permutex2var_epi64(dim_lo, mi16, dim_hi);
        let ylo4 = _mm512_test_epi64_mask(ylo, _mm512_set1_epi64(3));
        let yhi4 = _mm512_test_epi64_mask(yhi, _mm512_set1_epi64(3));
        let ylo0 = _mm512_cmpeq_epi64_mask(ylo, _mm512_setzero_si512());
        let leap = (!ylo4 & !ylo0) | (ylo0 & !yhi4);
        let feb = _mm512_cmpeq_epi64_mask(m, _mm512_set1_epi64(2));
        let dim = _mm512_mask_add_epi64(dim, leap & feb, dim, one);
        let dok = _mm512_cmplt_epu64_mask(_mm512_sub_epi64(dd, one), dim);
        let tok = _mm512_cmple_epu64_mask(hh, _mm512_set1_epi64(23)) & _mm512_cmple_epu64_mask(mi, _mm512_set1_epi64(59)) & _mm512_cmple_epu64_mask(ss, _mm512_set1_epi64(59));
        // days since 1970-01-01
        let y = _mm512_add_epi64(_mm512_mul_epu32(yhi, _mm512_set1_epi64(100)), ylo);
        let jf = _mm512_cmple_epu64_mask(m, _mm512_set1_epi64(2));
        let y2 = _mm512_add_epi64(_mm512_mask_sub_epi64(y, jf, y, one), _mm512_set1_epi64(400));
        let c100 = _mm512_srli_epi64::<19>(_mm512_mul_epu32(y2, _mm512_set1_epi64(5243)));
        let doy_lo = _mm512_setr_epi64(0, 306, 337, 0, 31, 61, 92, 122);
        let doy_hi = _mm512_setr_epi64(153, 184, 214, 245, 275, 0, 0, 0);
        let doy = _mm512_permutex2var_epi64(doy_lo, mi16, doy_hi);
        let days = _mm512_add_epi64(_mm512_mul_epu32(y2, _mm512_set1_epi64(365)), _mm512_srli_epi64::<2>(y2));
        let days = _mm512_sub_epi64(days, c100);
        let days = _mm512_add_epi64(days, _mm512_srli_epi64::<2>(c100));
        let days = _mm512_add_epi64(_mm512_add_epi64(days, doy), dd);
        let days = _mm512_sub_epi64(days, _mm512_set1_epi64(1 + 719_468 + 146_097));
        let secs = _mm512_add_epi64(_mm512_add_epi64(_mm512_mul_epu32(hh, _mm512_set1_epi64(3600)), _mm512_mul_epu32(mi, _mm512_set1_epi64(60))), ss);
        let ms = _mm512_add_epi64(_mm512_mul_epi32(days, _mm512_set1_epi64(86_400_000)), _mm512_mul_epu32(secs, _mm512_set1_epi64(1000)));
        let mut ok = ok0 & ok1 & ok2 & mok & dok & tok;
        let mut val = ms;
        if F2 {
            let oh = byte_at(v2, 4);
            let om = _mm512_add_epi64(_mm512_mul_epu32(byte_at(d2, 7), _mm512_set1_epi64(10)), _mm512_sub_epi64(_mm512_and_si512(g3, _mm512_set1_epi64(0xFF)), _mm512_set1_epi64(b'0' as i64)));
            let ook = _mm512_cmple_epu64_mask(oh, _mm512_set1_epi64(23)) & _mm512_cmple_epu64_mask(om, _mm512_set1_epi64(59));
            ok &= ook | !f2m;
            let off = _mm512_mul_epu32(_mm512_add_epi64(_mm512_mul_epu32(oh, _mm512_set1_epi64(60)), om), _mm512_set1_epi64(60_000));
            let plus = _mm512_cmpeq_epi64_mask(byte_at(g2, 3), _mm512_set1_epi64(b'+' as i64));
            let v2 = _mm512_mask_sub_epi64(_mm512_add_epi64(ms, off), plus, ms, off);
            val = _mm512_mask_mov_epi64(ms, f2m, v2);
        }
        (val, ok)
    }
}

/// 8 F3 values: a = 8 bytes ending 8 before the value end, b = 8 bytes ending at it.
#[inline(always)]
unsafe fn f3_8(a: __m512i, b: __m512i, len: __m512i) -> (__m512i, __mmask8) {
    unsafe {
        let eight = _mm512_set1_epi64(8);
        let nb = _mm512_min_epu64(len, eight);
        let na = _mm512_sub_epi64(len, nb);
        let ka = keep_mask(na);
        let kb = keep_mask(nb);
        let da = _mm512_sub_epi8(a, _mm512_set1_epi8(b'0' as i8));
        let db = _mm512_sub_epi8(b, _mm512_set1_epi8(b'0' as i8));
        let ten = _mm512_set1_epi8(10);
        let bad = any_byte((ka & !_mm512_cmplt_epu8_mask(da, ten)) | (kb & !_mm512_cmplt_epu8_mask(db, ten)));
        let w1 = _mm512_set1_epi16(0x010A);
        let w2 = _mm512_set1_epi32(0x0001_0064);
        let t = _mm512_set1_epi64(10_000);
        let va = _mm512_madd_epi16(_mm512_maddubs_epi16(_mm512_maskz_mov_epi8(ka, da), w1), w2);
        let va = _mm512_add_epi64(_mm512_mul_epu32(va, t), _mm512_srli_epi64::<32>(va));
        let vb = _mm512_madd_epi16(_mm512_maddubs_epi16(_mm512_maskz_mov_epi8(kb, db), w1), w2);
        let vb = _mm512_add_epi64(_mm512_mul_epu32(vb, t), _mm512_srli_epi64::<32>(vb));
        let v = _mm512_add_epi64(_mm512_mul_epu32(va, _mm512_set1_epi64(100_000_000)), vb);
        let lok = _mm512_cmplt_epu64_mask(_mm512_sub_epi64(len, _mm512_set1_epi64(1)), _mm512_set1_epi64(16));
        (v, lok & !bad)
    }
}

/// 128-bit funnel: bytes s/8.. of the 16-byte value [a (low 8 bytes) : b (high 8 bytes)],
/// per qword lane; s in bits, 0..=128 (counts out of range shift to 0).
#[inline(always)]
unsafe fn funnel(a: __m512i, b: __m512i, s: __m512i) -> __m512i {
    unsafe {
        let s64 = _mm512_set1_epi64(64);
        let r = _mm512_or_si512(_mm512_srlv_epi64(a, s), _mm512_sllv_epi64(b, _mm512_sub_epi64(s64, s)));
        _mm512_or_si512(r, _mm512_srlv_epi64(b, _mm512_sub_epi64(s, s64)))
    }
}
#[inline(always)]
unsafe fn funnel_c<const S: u32, const T: u32>(a: __m512i, b: __m512i) -> __m512i {
    unsafe { _mm512_or_si512(_mm512_srli_epi64::<S>(a), _mm512_slli_epi64::<T>(b)) }
}

/// Lanes 8h..8h+8 of 16-byte windows: (bytes 0..8, bytes 8..16) per lane.
#[inline(always)]
unsafe fn deint16(w: *const [u8; 16], h: usize) -> (__m512i, __m512i) {
    unsafe {
        let z0 = _mm512_loadu_si512(w.add(8 * h) as *const _);
        let z1 = _mm512_loadu_si512(w.add(8 * h + 4) as *const _);
        let ia = _mm512_setr_epi64(0, 2, 4, 6, 8, 10, 12, 14);
        let ib = _mm512_setr_epi64(1, 3, 5, 7, 9, 11, 13, 15);
        (_mm512_permutex2var_epi64(z0, ia, z1), _mm512_permutex2var_epi64(z0, ib, z1))
    }
}

/// Lanes 8h..8h+8 of 32-byte windows: the 4 qwords per lane.
#[inline(always)]
unsafe fn deint32(w: *const [u8; 32], h: usize) -> [__m512i; 4] {
    unsafe {
        let z: [__m512i; 4] = core::array::from_fn(|i| _mm512_loadu_si512(w.add(8 * h + 2 * i) as *const _));
        let e = _mm512_setr_epi64(0, 4, 8, 12, 1, 5, 9, 13);
        let o = _mm512_setr_epi64(2, 6, 10, 14, 3, 7, 11, 15);
        let te0 = _mm512_permutex2var_epi64(z[0], e, z[1]);
        let to0 = _mm512_permutex2var_epi64(z[0], o, z[1]);
        let te1 = _mm512_permutex2var_epi64(z[2], e, z[3]);
        let to1 = _mm512_permutex2var_epi64(z[2], o, z[3]);
        let lo = _mm512_setr_epi64(0, 1, 2, 3, 8, 9, 10, 11);
        let hi = _mm512_setr_epi64(4, 5, 6, 7, 12, 13, 14, 15);
        [
            _mm512_permutex2var_epi64(te0, lo, te1),
            _mm512_permutex2var_epi64(te0, hi, te1),
            _mm512_permutex2var_epi64(to0, lo, to1),
            _mm512_permutex2var_epi64(to0, hi, to1),
        ]
    }
}

/// ts from 32-byte windows ending at the value end (qwords w), lengths len, live lanes kh.
#[inline(always)]
unsafe fn ts8w(w: &[__m512i; 4], len: __m512i, kh: u8) -> (__m512i, __mmask8) {
    unsafe {
        let is20 = _mm512_cmpeq_epi64_mask(len, _mm512_set1_epi64(20)) & kh;
        let is25 = _mm512_cmpeq_epi64_mask(len, _mm512_set1_epi64(25)) & kh;
        let is3 = _mm512_cmplt_epu64_mask(_mm512_sub_epi64(len, _mm512_set1_epi64(1)), _mm512_set1_epi64(16)) & kh;
        let g0 = _mm512_mask_blend_epi64(is25, funnel_c::<32, 32>(w[1], w[2]), funnel_c::<56, 8>(w[0], w[1]));
        let g1 = _mm512_mask_blend_epi64(is25, funnel_c::<32, 32>(w[2], w[3]), funnel_c::<56, 8>(w[1], w[2]));
        let g2 = _mm512_mask_blend_epi64(is25, _mm512_srli_epi64::<32>(w[3]), funnel_c::<56, 8>(w[2], w[3]));
        let (vc, kc) = civ8m::<true>(g0, g1, g2, _mm512_srli_epi64::<56>(w[3]), is25);
        let (v3, k3) = f3_8(w[2], w[3], len);
        let kc = kc & (is20 | is25);
        let k3 = k3 & is3;
        (_mm512_mask_mov_epi64(v3, kc, vc), kc | k3)
    }
}

/// F1 from 32-byte windows.
#[inline(always)]
unsafe fn f1_8w(w: &[__m512i; 4], len: __m512i, kh: u8) -> (__m512i, __mmask8) {
    unsafe {
        let is20 = _mm512_cmpeq_epi64_mask(len, _mm512_set1_epi64(20)) & kh;
        let (v, k) = civ8(funnel_c::<32, 32>(w[1], w[2]), funnel_c::<32, 32>(w[2], w[3]), _mm512_srli_epi64::<32>(w[3]), w[3], false);
        (v, k & is20)
    }
}

#[repr(C, align(64))]
struct EvWin {
    id0: [[u8; 16]; B],
    id1: [[u8; 16]; B],
    prod: [[u8; 16]; B],
    ts: [[u8; 32]; B],
    ing: [[u8; 32]; B],
    kind: [u64; B],
    price: [u64; B],
    ve: [[i32; B]; 7],
}

#[repr(C, align(64))]
struct ClWin {
    id0: [[u8; 16]; B],
    id1: [[u8; 16]; B],
    camp: [[u8; 16]; B],
    ts: [[u8; 32]; B],
    ve: [[i32; B]; 4],
}

#[inline(always)]
unsafe fn cp16(dst: *mut [u8; 16], src: *const u8) {
    unsafe { _mm_store_si128(dst as *mut __m128i, _mm_loadu_si128(src.sub(16) as *const __m128i)) }
}
#[inline(always)]
unsafe fn cp32(dst: *mut [u8; 32], src: *const u8) {
    unsafe { _mm256_store_si256(dst as *mut __m256i, _mm256_loadu_si256(src.sub(32) as *const __m256i)) }
}

#[repr(C, align(64))]
struct EvOut {
    eid: [u64; B],
    uid: [u64; B],
    kind: [u64; B],
    prod: [u64; B],
    price: [u64; B],
    fb: [u64; B],
    ts: [u64; B],
    ing: [u64; B],
    vs: [[i32; B]; 2],
    len: [[i32; B]; 2],
    len0: [i32; B],
}

#[cold]
#[inline(never)]
fn ev_fix(buf: &[u8], start: usize, end: usize, fb: u64, row: &mut EvRow) {
    stat(if fb & 8 != 0 { 4 } else if fb & 16 != 0 { 5 } else { 1 });
    let v = split_line::<7>(buf, start, end).unwrap();
    if fb & 1 != 0 {
        row.eid = slow_id(buf, v[0].0, v[0].1, b'e');
    }
    if fb & 2 != 0 {
        row.uid = slow_id(buf, v[1].0, v[1].1, b'u');
    }
    if fb & 4 != 0 {
        row.price = parse_price(&buf[v[6].0..v[6].1]);
    }
    if fb & 8 != 0 {
        row.ts = slow_ts(&buf[v[2].0..v[2].1]);
    }
    if fb & 16 != 0 {
        row.ingest = slow_f1(&buf[v[3].0..v[3].1]);
    }
}

#[inline(never)]
#[target_feature(enable = "avx512f,avx512bw,avx512vl,avx512dq,bmi1,bmi2,popcnt")]
unsafe fn events_impl<S: EventSink>(buf: &[u8], sink: &mut S) {
    unsafe {
        let p = buf.as_ptr();
        let mut bt: Batch<7> = core::mem::zeroed();
        let mut out: EvOut = core::mem::zeroed();
        let mut win: EvWin = core::mem::zeroed();
        drive::<7>(buf, &mut bt, |bt, cb| {
            let nb = bt.n;
            let mut special: u32 = bt.slow;
            let live = ((1u32 << nb) - 1) & !bt.slow;
            let base = p.wrapping_add(cb).wrapping_sub(POS_OFF as usize);
            if live != 0 {
                let (vs, ve) = spans::<7>(bt);
                let len: [__m512i; 7] = core::array::from_fn(|k| _mm512_sub_epi32(ve[k], vs[k]));
                for k in 0..7 {
                    _mm512_storeu_si512(win.ve[k].as_mut_ptr() as *mut _, ve[k]);
                }
                for i in 0..nb {
                    let at = |k: usize| base.wrapping_add(*win.ve[k].get_unchecked(i) as usize);
                    cp16(win.id0.as_mut_ptr().add(i), at(0));
                    cp16(win.id1.as_mut_ptr().add(i), at(1));
                    cp32(win.ts.as_mut_ptr().add(i), at(2));
                    cp32(win.ing.as_mut_ptr().add(i), at(3));
                    *win.kind.get_unchecked_mut(i) = ld64(at(4).sub(8));
                    cp16(win.prod.as_mut_ptr().add(i), at(5));
                    *win.price.get_unchecked_mut(i) = ld64(at(6).sub(8));
                }
                for h in 0..2 {
                    let kh = (live >> (8 * h)) as u8;
                    if kh == 0 {
                        continue;
                    }
                    let half = |v: __m512i| if h == 0 { lo8(v) } else { hi8(v) };
                    let l64 = |k: usize| _mm512_cvtepi32_epi64(half(len[k]));
                    let s64 = |l: __m512i| _mm512_slli_epi64::<3>(_mm512_sub_epi64(_mm512_set1_epi64(16), l));
                    // ids
                    let el = l64(0);
                    let (a, b) = deint16(win.id0.as_ptr(), h);
                    let (eid, ok0) = ids8(b, funnel(a, b, s64(el)), el, b'e');
                    let ul = l64(1);
                    let (a, b) = deint16(win.id1.as_ptr(), h);
                    let (uid, ok1) = ids8(b, funnel(a, b, s64(ul)), ul, b'u');
                    let uempty = _mm512_cmpeq_epi64_mask(ul, _mm512_setzero_si512());
                    let uid = _mm512_mask_mov_epi64(uid, uempty, _mm512_set1_epi64(-1));
                    // kind
                    let kw = _mm512_loadu_si512(win.kind.as_ptr().add(8 * h) as *const _);
                    let kl = l64(4);
                    let w4 = _mm512_srli_epi64::<32>(kw);
                    let is4 = _mm512_cmpeq_epi64_mask(kl, _mm512_set1_epi64(4));
                    let view = is4 & _mm512_cmpeq_epi64_mask(w4, _mm512_set1_epi64(u32::from_le_bytes(*b"view") as i64));
                    let cart = is4 & _mm512_cmpeq_epi64_mask(w4, _mm512_set1_epi64(u32::from_le_bytes(*b"cart") as i64));
                    let pur = _mm512_cmpeq_epi64_mask(kl, _mm512_set1_epi64(8)) & _mm512_cmpeq_epi64_mask(kw, _mm512_set1_epi64(i64::from_le_bytes(*b"purchase")));
                    let kind = _mm512_mask_mov_epi64(_mm512_set1_epi64(3), view, _mm512_setzero_si512());
                    let kind = _mm512_mask_mov_epi64(kind, cart, _mm512_set1_epi64(1));
                    let kind = _mm512_mask_mov_epi64(kind, pur, _mm512_set1_epi64(2));
                    // product
                    let pl = l64(5);
                    let (a, b) = deint16(win.prod.as_ptr(), h);
                    let ph = funnel(a, b, s64(pl));
                    let ppre = _mm512_cmpeq_epi64_mask(_mm512_and_si512(ph, _mm512_set1_epi64(0xFF)), _mm512_set1_epi64(b'p' as i64));
                    let (pv, pok) = dec8v(b, ph, pl, 1, ppre);
                    let prod = _mm512_mask_mov_epi64(_mm512_set1_epi64(PROD_ODD as i64), pok, pv);
                    let prod = _mm512_mask_mov_epi64(prod, _mm512_cmpeq_epi64_mask(pl, _mm512_setzero_si512()), _mm512_set1_epi64(PROD_EMPTY as i64));
                    // price (purchases only)
                    let kp = pur & kh;
                    let (pr, plong) = price8(_mm512_loadu_si512(win.price.as_ptr().add(8 * h) as *const _), l64(6));
                    let price = _mm512_maskz_mov_epi64(kp, pr);
                    // timestamps
                    let (tsv, tok) = ts8w(&deint32(win.ts.as_ptr(), h), l64(2), kh);
                    let (igv, iok) = f1_8w(&deint32(win.ing.as_ptr(), h), l64(3), kh);
                    let fb = _mm512_maskz_mov_epi64(!ok0, _mm512_set1_epi64(1));
                    let fb = _mm512_mask_or_epi64(fb, !ok1 & !uempty, fb, _mm512_set1_epi64(2));
                    let fb = _mm512_mask_or_epi64(fb, kp & plong, fb, _mm512_set1_epi64(4));
                    let fb = _mm512_mask_or_epi64(fb, !tok, fb, _mm512_set1_epi64(8));
                    let fb = _mm512_mask_or_epi64(fb, !iok, fb, _mm512_set1_epi64(16));
                    let o = 8 * h;
                    _mm512_storeu_si512(out.eid.as_mut_ptr().add(o) as *mut _, eid);
                    _mm512_storeu_si512(out.uid.as_mut_ptr().add(o) as *mut _, uid);
                    _mm512_storeu_si512(out.kind.as_mut_ptr().add(o) as *mut _, kind);
                    _mm512_storeu_si512(out.prod.as_mut_ptr().add(o) as *mut _, prod);
                    _mm512_storeu_si512(out.price.as_mut_ptr().add(o) as *mut _, price);
                    _mm512_storeu_si512(out.fb.as_mut_ptr().add(o) as *mut _, fb);
                    _mm512_storeu_si512(out.ts.as_mut_ptr().add(o) as *mut _, tsv);
                    _mm512_storeu_si512(out.ing.as_mut_ptr().add(o) as *mut _, igv);
                    special |= (_mm512_test_epi64_mask(fb, fb) as u32) << o;
                }
                special |= _mm512_cmpeq_epi32_mask(len[0], _mm512_setzero_si512()) as u32;
                _mm512_storeu_si512(out.len0.as_mut_ptr() as *mut _, len[0]);
            }
            for i in 0..nb {
                let mut row = EvRow { eid: out.eid[i], uid: out.uid[i], ts: out.ts[i] as i64, ingest: out.ing[i] as i64, kind: out.kind[i] as u8, product: out.prod[i] as u32, price: out.price[i] as u32 };
                if special >> i & 1 == 0 {
                    sink.event(&row);
                    continue;
                }
                if bt.slow >> i & 1 != 0 {
                    slow_event(buf, bt.start[i], bt.end[i], sink);
                    continue;
                }
                if out.len0[i] == 0 {
                    continue; // empty event_id
                }
                if out.fb[i] != 0 {
                    ev_fix(buf, bt.start[i], bt.end[i], out.fb[i], &mut row);
                }
                sink.event(&row);
            }
        });
        #[cfg(zmm_stats)]
        dump_stats("events");
    }
}

#[repr(C, align(64))]
struct ClOut {
    cid: [u64; B],
    uid: [u64; B],
    camp: [u64; B],
    fb: [u64; B],
    ts: [u64; B],
    len0: [i32; B],
}

#[cold]
#[inline(never)]
fn cl_fix(buf: &[u8], start: usize, end: usize, fb: u64, row: &mut ClRow) {
    let v = split_line::<4>(buf, start, end).unwrap();
    if fb & 1 != 0 {
        row.cid = slow_id(buf, v[0].0, v[0].1, b'k');
    }
    if fb & 2 != 0 {
        row.uid = slow_id(buf, v[1].0, v[1].1, b'u');
    }
    if fb & 8 != 0 {
        row.ts = slow_ts(&buf[v[3].0..v[3].1]);
    }
}

#[inline(never)]
#[target_feature(enable = "avx512f,avx512bw,avx512vl,avx512dq,bmi1,bmi2,popcnt")]
unsafe fn clicks_impl<S: ClickSink>(buf: &[u8], sink: &mut S) {
    unsafe {
        let p = buf.as_ptr();
        let mut bt: Batch<4> = core::mem::zeroed();
        let mut out: ClOut = core::mem::zeroed();
        let mut win: ClWin = core::mem::zeroed();
        drive::<4>(buf, &mut bt, |bt, cb| {
            let nb = bt.n;
            let mut special: u32 = bt.slow;
            let live = ((1u32 << nb) - 1) & !bt.slow;
            let base = p.wrapping_add(cb).wrapping_sub(POS_OFF as usize);
            if live != 0 {
                let (vs, ve) = spans::<4>(bt);
                let len: [__m512i; 4] = core::array::from_fn(|k| _mm512_sub_epi32(ve[k], vs[k]));
                for k in 0..4 {
                    _mm512_storeu_si512(win.ve[k].as_mut_ptr() as *mut _, ve[k]);
                }
                for i in 0..nb {
                    let at = |k: usize| base.wrapping_add(*win.ve[k].get_unchecked(i) as usize);
                    cp16(win.id0.as_mut_ptr().add(i), at(0));
                    cp16(win.id1.as_mut_ptr().add(i), at(1));
                    cp16(win.camp.as_mut_ptr().add(i), at(2));
                    cp32(win.ts.as_mut_ptr().add(i), at(3));
                }
                for h in 0..2 {
                    let kh = (live >> (8 * h)) as u8;
                    if kh == 0 {
                        continue;
                    }
                    let half = |v: __m512i| if h == 0 { lo8(v) } else { hi8(v) };
                    let l64 = |k: usize| _mm512_cvtepi32_epi64(half(len[k]));
                    let s64 = |l: __m512i| _mm512_slli_epi64::<3>(_mm512_sub_epi64(_mm512_set1_epi64(16), l));
                    let el = l64(0);
                    let (a, b) = deint16(win.id0.as_ptr(), h);
                    let (cid, ok0) = ids8(b, funnel(a, b, s64(el)), el, b'k');
                    let ul = l64(1);
                    let (a, b) = deint16(win.id1.as_ptr(), h);
                    let (uid, ok1) = ids8(b, funnel(a, b, s64(ul)), ul, b'u');
                    let uempty = _mm512_cmpeq_epi64_mask(ul, _mm512_setzero_si512());
                    let uid = _mm512_mask_mov_epi64(uid, uempty, _mm512_set1_epi64(-1));
                    let cl = l64(2);
                    let (a, b) = deint16(win.camp.as_ptr(), h);
                    let ch = funnel(a, b, s64(cl));
                    let cpre = _mm512_cmpeq_epi64_mask(_mm512_and_si512(ch, _mm512_set1_epi64(0xFFFF)), _mm512_set1_epi64(0x5F63));
                    let (cv, cok) = dec8v(b, ch, cl, 2, cpre);
                    let camp = _mm512_mask_mov_epi64(_mm512_set1_epi64(CAMP_ODD as i64), cok, cv);
                    let camp = _mm512_mask_mov_epi64(camp, _mm512_cmpeq_epi64_mask(cl, _mm512_setzero_si512()), _mm512_set1_epi64(CAMP_EMPTY as i64));
                    let (tsv, tok) = ts8w(&deint32(win.ts.as_ptr(), h), l64(3), kh);
                    let fb = _mm512_maskz_mov_epi64(!ok0, _mm512_set1_epi64(1));
                    let fb = _mm512_mask_or_epi64(fb, !ok1 & !uempty, fb, _mm512_set1_epi64(2));
                    let fb = _mm512_mask_or_epi64(fb, !tok, fb, _mm512_set1_epi64(8));
                    let o = 8 * h;
                    _mm512_storeu_si512(out.cid.as_mut_ptr().add(o) as *mut _, cid);
                    _mm512_storeu_si512(out.uid.as_mut_ptr().add(o) as *mut _, uid);
                    _mm512_storeu_si512(out.camp.as_mut_ptr().add(o) as *mut _, camp);
                    _mm512_storeu_si512(out.fb.as_mut_ptr().add(o) as *mut _, fb);
                    _mm512_storeu_si512(out.ts.as_mut_ptr().add(o) as *mut _, tsv);
                    special |= (_mm512_test_epi64_mask(fb, fb) as u32) << o;
                }
                special |= _mm512_cmpeq_epi32_mask(len[0], _mm512_setzero_si512()) as u32;
                _mm512_storeu_si512(out.len0.as_mut_ptr() as *mut _, len[0]);
            }
            for i in 0..nb {
                let mut row = ClRow { cid: out.cid[i], uid: out.uid[i], campaign: out.camp[i] as u32, ts: out.ts[i] as i64 };
                if special >> i & 1 == 0 {
                    sink.click(&row);
                    continue;
                }
                if bt.slow >> i & 1 != 0 {
                    slow_click(buf, bt.start[i], bt.end[i], sink);
                    continue;
                }
                if out.len0[i] == 0 {
                    continue; // empty click_id
                }
                if out.fb[i] != 0 {
                    cl_fix(buf, bt.start[i], bt.end[i], out.fb[i], &mut row);
                }
                sink.click(&row);
            }
        });
        #[cfg(zmm_stats)]
        dump_stats("clicks");
    }
}

#[inline(always)]
unsafe fn ld64(p: *const u8) -> u64 {
    unsafe { core::ptr::read_unaligned(p as *const u64) }
}

#[cold]
#[inline(never)]
fn slow_event<S: EventSink>(buf: &[u8], start: usize, end: usize, sink: &mut S) {
    stat(0);
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
fn slow_click<S: ClickSink>(buf: &[u8], start: usize, end: usize, sink: &mut S) {
    stat(0);
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

#[cold]
#[inline(never)]
fn slow_id(buf: &[u8], s: usize, e: usize, prefix: u8) -> u64 {
    stat(1);
    parse_id(buf, (s, e), prefix)
}
#[cold]
#[inline(never)]
fn slow_ts(v: &[u8]) -> i64 {
    stat(2);
    parse_ts(v)
}
#[cold]
#[inline(never)]
fn slow_f1(v: &[u8]) -> i64 {
    stat(3);
    parse_f1(v)
}

