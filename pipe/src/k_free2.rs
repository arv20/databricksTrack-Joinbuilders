//! k_free.rs -- parse kernels "free" (default), "freen" and "freep": the same decoders with three
//! ways to walk the lines (see PIPE / CHAIN / PRE at the runners).
//!
//! Needs: -C target-cpu=cascadelake (AVX2, BMI1/2, LZCNT, POPCNT, AVX-512 F/BW/VL/DQ). No VBMI & co.
//! zmm is used only for byte compares into masks (2 loads per event line); every multiply is 128-bit.
//! Self-contained: depends only on iface.rs. State: one thread-local 64-entry date cache (2 kB).
//!
//! DESIGN
//! 1. Line ends. free / freen: the b'\n' is found in the line's own window, so the only
//!    dependency between lines is "next start = this b'\n' + 1". free also makes the bit masks
//!    of the NEXT line while it decodes the current one (software pipelining through memory).
//!    freep: a prepass per 8 kB chunk stores the positions of all b'\n'.
//! 2. Windows. events: W1 = comma / newline bit masks of the 64 bytes at the line start l
//!    (commas c0 c1 c2), WB = comma / quote / newline masks of the 64 bytes at l + 40 (commas
//!    c3 c4 c5 = the last three before the b'\n'). clicks: one window at l.
//! 3. Fields. Every field is decoded by a table row selected by its RAW length (quotes
//!    included); product / campaign also by their opening-quote bit. The row holds a byte
//!    pattern that must match exactly (prefix, quotes, punctuation, digit ranges) and the
//!    multiplier weights. Each decoder returns its value and a "bad" vector; the fast line ORs
//!    all bad vectors and tests once.
//!
//! THREE LEVELS
//!  fast line    (ev_fast / cl_core with CAREFUL = false): no data-dependent branch except
//!               "type is 8+ bytes" (purchase). Delivers the row only if every check holds.
//!  careful line (ev_core / cl_core with CAREFUL = true, not inlined): starts again from the
//!               line start and tests each field; a field that fails its pattern is decoded by
//!               cold_* = the reference code for one field. It also fills the date cache.
//!  slow line    (slow::event / slow::click = a copy of the reference): everything else.
//!
//! FAST-PATH CONDITIONS
//! Structure (else slow line, via the careful line):
//!  - the line ends with b'\n' inside the buffer, and (free / freen) starts at least 64 bytes
//!    before the buffer end (freep: 40 bytes, events only);
//!  - events: exactly 6 commas; c2 at most 62 bytes after the line start; the b'\n' at
//!    l + 40 ..= l + 103 with c3, c4, c5 at or after l + 40 (so line length 40..=103);
//!    clicks: exactly 3 commas and line length <= 63 (free / freen) / 64 (freep);
//!  - price_cents: no quote in the field, or exactly one pair around it; if event_type has 8
//!    or more raw bytes (purchase): at most 8 value bytes (digits -> value, else 0).
//! Fields (else careful line, which sends only the failing field to the reference code; if that
//! reports misplaced quotes or an empty first Id, the line goes to the slow line):
//!  - event_id / click_id: prefix + 1..8 chars of [0-9a-z], no leading '0' (unless alone),
//!    unquoted; or quoted with 7 or 8 chars (fast line; the careful line takes every length);
//!  - user_id: the same, or empty (also as "");
//!  - ts: F1 (raw 20), "F1" (22), F2 (25), "F2" (27), F3 of 1..14 or 16 digits unquoted, "F3" of
//!    13 digits (raw 15); all with valid ranges; the date must be in the cache (the careful
//!    line adds any valid YYYY-MM-DD to it);
//!  - ingest_ts: valid F1 or "F1";
//!  - event_type: view / cart / purchase, quoted or not;
//!  - product_id: [quote] p + 1..8 digits, canonical [quote]; campaign_id: [quote] c_ + 1..8 digits [quote].
//! Every byte of the fields 0..5 is matched against a pattern that has no free quote and no
//! b'\n', so a stray quote always fails a pattern; the price field is checked with the quote mask.

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
    ($ty:ident, $name:literal, $mode:ident) => {
        pub struct $ty;
        impl Kernel for $ty {
            const NAME: &'static str = $name;
            fn events<S: EventSink>(buf: &[u8], sink: &mut S) {
                unsafe { run_events::<S, $mode>(buf, sink) }
            }
            fn clicks<S: ClickSink>(buf: &[u8], sink: &mut S) {
                unsafe { run_clicks::<S, $mode>(buf, sink) }
            }
        }
    };
}
kernel!(Free, "free", PIPE);
kernel!(FreeN, "freen", CHAIN);
kernel!(FreeP, "freep", PRE);

/// Counters by reason (only with --features stats).
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

// ---------------------------------------------------------------------------------------------
// Tables
// ---------------------------------------------------------------------------------------------

/// Shape of an Id field, index raw_len * 2 + opening_quote. Source: the 16 bytes that end at the
/// field's comma. ctrl moves the digits right-aligned into 8 bytes (dig..dig+8) and the prefix /
/// closing quote into check bytes (chk, chk+1); subv turns digits into c - '0' and check bytes
/// into 0 when they hold the expected character. lz marks the first digit's byte when there
/// are two or more digits (it must not be '0'). or: all ones for an empty field (ID_EMPTY).
#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct IdShape {
    ctrl: [u8; 16],
    subv: [u8; 16],
    lz: [u8; 16],
    or: u64,
    _pad: u64,
}

const fn id_shapes(prefix: u8, dig: usize, chk: usize, allow_empty: bool) -> [IdShape; 128] {
    let mut t = [IdShape { ctrl: [0x80; 16], subv: [0; 16], lz: [0; 16], or: 0, _pad: 0 }; 128];
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
                    t[i].lz[dig + 8 - nd] = 0xFF;
                }
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
                t[i].subv[chk] = 1; // 0 - 1 != 0: never passes
            }
            q += 1;
        }
        raw += 1;
    }
    t
}

/// Id shapes by raw length alone, for the fast lines: the common reading of each length.
/// 0: empty (user_id only); 2: "" (user_id) or prefix + 1 digit (others); 3..=9: unquoted,
/// 2..=8 digits; 10, 11: quoted, 7 or 8 digits (the opening quote is one more check byte).
/// Other readings (a quoted Id of 1..6 digits, ...) fail the shape and go to the careful line.
const fn id_shapes_raw(prefix: u8, dig: usize, chk: usize, allow_empty: bool) -> [IdShape; 64] {
    let both = id_shapes(prefix, dig, chk, allow_empty);
    let mut t = [both[1]; 64]; // raw 0, quoted: an invalid shape
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

/// Shape of a prefix + canonical-decimal field (product, campaign); digits to bytes 0..8,
/// check bytes from 8; lz as in IdShape.
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
                    t[i].lz[8 - nd] = 0xFF;
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

/// Timestamp row, index raw_len (three parallel tables of 64-byte rows).
/// lane = pshufb(the 16 bytes that end at the raw field's end, ctrl): the row's digits at fixed
/// places. d = lane - base must be <= lim bytewise. Three multiply-add levels (w1, w2, w3) give
/// the dwords E and F; the four dwords after w2 must be <= limv (signed; this bounds the hours).
/// result = (date_ms & dmask) + E * me + F * mf[s >> 1 & 1], s = the byte at raw_end + soff
/// (the F2 sign: '-' -> mf[0], '+' -> mf[1]).
/// Rows with a date compare the 16 bytes at raw_start + koff with a cache key under kmaskv.
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
// 0 > -1: never passes
const TSB_INVALID: TsB = TsB { w2: [0; 8], w3: [0; 8], kmaskv: [0; 16], limv: [-1, i32::MAX, i32::MAX, i32::MAX] };
const TSC_INVALID: TsC = TsC { koff: -1, soff: -1, dmask: 0, me: 0, mf: [0, 0] };

/// F1 / F2 row. Normalized lane = the 16 bytes at raw_start + 11 + q: "HH:MM:SS" + "Z" or "+HH:MM" [+ quote].
const fn ts_f12(f2: bool, q: bool) -> (TsA, TsB, TsC) {
    let (mut a, mut b, mut c) = (TSA_INVALID, TSB_INVALID, TSC_INVALID);
    let raw = 20 + 5 * f2 as usize + 2 * q as usize;
    let off = 11 + q as usize;
    let mut i = 0;
    while i < 16 {
        // lane byte i = raw byte off + i = byte off + i + 16 - raw of the end-anchored load
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
        let l2 = [2u8, 2, 9, 0, 5, 9]; // '+' ',' '-' pass; a ',' cannot be inside a field
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
        c.soff = (off + 8) as i8 - raw as i8; // the sign byte
    } else {
        a.base[8] = b'Z';
        a.lim[8] = 0;
    }
    if q {
        a.base[next] = b'"';
        a.lim[next] = 0;
    }
    // words after w1: HH, Mt*10, Mo, SS, OHt*10, OHo, OM, 0
    b.w2 = [60, 1, 60, 1, 1, 1, 1, 0]; // HH*60 + Mt*10 | Mo*60 + SS | OH | OM
    b.w3 = [60, 1, 60, 1, 0, 0, 0, 0]; // E = local seconds | F = offset minutes
    let mut i = if q { 0 } else { 1 };
    while i < 12 {
        b.kmaskv[i] = 0xFF;
        i += 1;
    }
    // HH*60 + Mt*10 <= 1430 means HH <= 23 (Mt <= 5 by lim); OH <= 23
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
    // F3, n digits, unquoted (n = 15 is taken by the quoted 13-digit form): lane = end-anchored load
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
    // "F3" with 13 digits: lane = [junk, '"', d0..d12, '"']
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
    b.w2 = [0, 1, 100, 1, 100, 1, 10, 1]; // d0d1 | d2..d5 | d6..d9 | d10..d12
    b.w3 = [10_000, 1, 1000, 1, 0, 0, 0, 0]; // E = d0..d5 | F = d6..d12
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

/// event_type row, index raw_len: the bytes at raw_start under maskv must equal cand[f], where
/// f = bit 0 of the byte at raw_start + 3 ('w' and 'e' of view: 1, 't' and 'r' of cart: 0).
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
    // b'\n' can never be the first byte at raw_start (it is inside the line): never matches.
    let mut t = [KindRow { cand: [[b'\n'; 16]; 2], maskv: [0xFF; 16], k: [3, 3] }; 64];
    t[4] = kind_row(b"cart", 1, b"view", 0);
    t[6] = kind_row(b"\"cart\"", 1, b"\"view\"", 0);
    t[8] = kind_row(b"purchase", 2, b"purchase", 2);
    t[10] = kind_row(b"\"purchase\"", 2, b"\"purchase\"", 2);
    t
}

#[repr(C, align(64))]
struct Tables {
    tsa: [TsA; 64],
    tsb: [TsB; 64],
    tsc: [TsC; 64],
    kind: [KindRow; 64],
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
    kind: kind_rows(),
    eid: id_shapes(b'e', 0, 8, false),
    kid: id_shapes(b'k', 0, 8, false),
    uid: id_shapes(b'u', 8, 0, true),
    eid1: id_shapes_raw(b'e', 0, 8, false),
    kid1: id_shapes_raw(b'k', 0, 8, false),
    uid1: id_shapes_raw(b'u', 8, 0, true),
    prod: dec_shapes(b"p"),
    camp: dec_shapes(b"c_"),
};

/// Date cache: direct-mapped by the two day digits. key = b'"' + "YYYY-MM-DDT" (+ 4 unused
/// bytes); ms = epoch ms of that date's midnight. Only valid dates are ever stored. The empty
/// key is all b',', which no field can hold.
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

/// Cache miss for a row that needs a date: kp[0] must be b'"' if quoted, kp[1..12] must be a
/// valid "YYYY-MM-DDT". Returns the date's epoch ms, or None (not a valid date). The entry is
/// replaced if it is empty or has missed 3 times since it was stored.
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

// ---------------------------------------------------------------------------------------------
// Field decoders. Each returns its value and a vector that is non-zero if the field does not
// fit its row (then the value is meaningless).
// ---------------------------------------------------------------------------------------------

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
/// Non-zero test without a mask register (see the note on mask registers in the header).
#[inline(always)]
fn nz(v: __m128i) -> bool {
    unsafe { _mm_cvtsi128_si64(_mm_or_si128(v, _mm_unpackhi_epi64(v, v))) != 0 }
}

/// Bit masks of the 64 bytes at p: commas, quotes, newlines.
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

/// Date part of the timestamp field with raw start rs and raw length raw (< 64):
/// (epoch ms of the cache entry, key mismatch vector (zero for a row without a date), key pointer).
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

/// Time / number part of the timestamp field that ends at rend with raw length raw (< 64).
/// FULL: any form (ts); !FULL: raw is 20 or 22 (ingest_ts, F1 only). day_ms is from ts_date
/// (or date_miss); rows without a date ignore it.
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

/// Two Ids at once: xa/xb = the 16 bytes ending at each field's comma. Returns (a, b, bad vector).
#[inline(always)]
unsafe fn ids2(xa: __m128i, sa: &IdShape, xb: __m128i, sb: &IdShape) -> (u64, u64, __m128i) {
    unsafe {
        let va = _mm_sub_epi8(_mm_shuffle_epi8(xa, ld(sa.ctrl.as_ptr())), ld(sa.subv.as_ptr()));
        let vb = _mm_sub_epi8(_mm_shuffle_epi8(xb, ld(sb.ctrl.as_ptr())), ld(sb.subv.as_ptr()));
        let d = _mm_blend_epi32::<0b1100>(va, vb); // a digits | b digits: c - '0'
        let chk = _mm_blend_epi32::<0b1100>(vb, va); // b checks | a checks: must be 0
        let e = _mm_sub_epi8(d, _mm_set1_epi8(49)); // c - 'a'
        // a byte is good if d <= 9 (digit) or e <= 25 (letter)
        let notdig = _mm_subs_epu8(d, _mm_set1_epi8(9));
        let notlet = _mm_subs_epu8(e, _mm_set1_epi8(25));
        // digit: d (the other operand wraps to >= 217); letter: e + 10 = d - 39 (< d)
        let val = _mm_min_epu8(d, _mm_sub_epi8(d, _mm_set1_epi8(39)));
        // leading zero: value 0 in the first digit's byte of a 2+ digit Id
        let lz = _mm_and_si128(_mm_cmpeq_epi8(val, _mm_setzero_si128()), _mm_or_si128(ld(sa.lz.as_ptr()), ld(sb.lz.as_ptr())));
        let bad = _mm_or_si128(_mm_or_si128(chk, lz), _mm_min_epu8(notdig, notlet));
        let m1 = _mm_maddubs_epi16(val, _mm_set1_epi16(0x0124)); // 36, 1
        let m2 = _mm_madd_epi16(m1, _mm_set1_epi32(0x0001_0510)); // 1296, 1
        let ids = _mm_add_epi64(_mm_mul_epu32(m2, _mm_set1_epi64x(1_679_616)), _mm_srli_epi64::<32>(m2));
        let a = _mm_cvtsi128_si64(ids) as u64;
        let b = _mm_extract_epi64::<1>(ids) as u64;
        (a, b | sb.or, bad)
    }
}

/// prefix + canonical decimal: x = the 16 bytes ending at the field's comma. Returns (value, bad vector).
#[inline(always)]
unsafe fn dec(x: __m128i, s: &DecShape) -> (u32, __m128i) {
    unsafe {
        let v = _mm_sub_epi8(_mm_shuffle_epi8(x, ld(s.ctrl.as_ptr())), ld(s.subv.as_ptr()));
        let lz = _mm_and_si128(_mm_cmpeq_epi8(v, _mm_setzero_si128()), ld(s.lz.as_ptr()));
        let bad = _mm_or_si128(lz, _mm_subs_epu8(v, _mm_set_epi64x(0, 0x0909_0909_0909_0909)));
        let m1 = _mm_maddubs_epi16(v, _mm_set_epi64x(0, 0x010A_010A_010A_010A));
        let m2 = _mm_madd_epi16(m1, _mm_set_epi64x(0, 0x0001_0064_0001_0064));
        let m3 = _mm_madd_epi16(_mm_packus_epi32(m2, m2), _mm_set_epi64x(0, 0x0001_2710));
        (_mm_cvtsi128_si32(m3) as u32, bad)
    }
}

/// event_type at raw start rs with raw length raw (< 64): (kind, bad vector).
#[inline(always)]
unsafe fn kind_of(rs: *const u8, raw: usize) -> (u32, __m128i) {
    unsafe {
        let kr = T.kind.get_unchecked(raw);
        let f = (*rs.wrapping_add(3) & 1) as usize;
        let bad = _mm_and_si128(_mm_xor_si128(ld(rs), ld(kr.cand.get_unchecked(f).as_ptr())), ld(kr.maskv.as_ptr()));
        (*kr.k.get_unchecked(f), bad)
    }
}

/// Purchase price: value bytes end at end, length len. None: too long for the fast path.
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
            return Some(0); // not digits only
        }
        let y = y.wrapping_mul(2561) >> 8 & 0x00FF_00FF_00FF_00FF;
        let y = y.wrapping_mul(6_553_601) >> 16 & 0x0000_FFFF_0000_FFFF;
        let v = (y.wrapping_mul(42_949_672_960_001) >> 32) as u32;
        Some(if v.wrapping_sub(1) < 10_000_000 { v } else { 0 })
    }
}

// Cold per-field fallbacks: the reference code for one raw field buf[a..b]. None = misplaced
// quotes (the line is malformed) or an empty first Id (no row): the caller takes the slow line.

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

// ---------------------------------------------------------------------------------------------
// Lines
// ---------------------------------------------------------------------------------------------

const FAIL: usize = usize::MAX;

/// Offset of the second event window from the line start. Fast lines have their 4th comma
/// at or after it and their b'\n' less than 64 bytes after it.
const OB: usize = 40;

/// Careful-mode timestamp: date via the cache or date_miss, then the row; None = use cold_ts.
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

/// Careful event line starting at l. CHAIN: finds the b'\n' itself (the caller guarantees
/// l + 64 <= len). !CHAIN: e_in is the position of the line's b'\n'.
/// Returns the position of the b'\n', or FAIL (nothing delivered: use the slow line).
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
        // commas 0..2: the lowest three of W1
        let c0 = _tzcnt_u64(w1.c);
        let t = _blsr_u64(w1.c);
        let c1 = _tzcnt_u64(t);
        let c2 = _tzcnt_u64(_blsr_u64(t));
        // m = WB bit of the b'\n'; commas 3..5: the highest three of WB below m
        let m = if CHAIN { _tzcnt_u64(wb.n) } else { e_in.wrapping_sub(lb) as u64 };
        let tb = _bzhi_u64(wb.c, m as u32);
        let c5 = 63 ^ _lzcnt_u64(tb);
        let t = _bzhi_u64(tb, c5 as u32);
        let c4 = 63 ^ _lzcnt_u64(t);
        let c3 = 63 ^ _lzcnt_u64(_bzhi_u64(t, c4 as u32));
        // The line is [l, lb + m): m in range; (CHAIN) no b'\n' before WB; 6 commas = those
        // before WB plus those in WB below m; at least three of them in WB (c3 c4 c5 exist) and
        // the third one of the line inside W1 with a byte after it (c0 c1 c2 exist).
        let head = (w1.c << (64 - OB)).count_ones() as u64;
        let tail = tb.count_ones() as u64;
        let nl_head = if CHAIN { w1.n << (64 - OB) } else { 0 };
        if (m > 63) | (nl_head != 0) | (head + tail != 6) | (tail < 3) | (c2 > 62) {
            return FAIL;
        }
        let (c0, c1, c2, c3, c4, c5, m) = (c0 as usize, c1 as usize, c2 as usize, c3 as usize, c4 as usize, c5 as usize, m as usize);
        let e = lb + m;
        // price: the quotes in the raw field must be none, or exactly its first and last byte
        let raw6 = m - c5 - 1;
        let pq = _bzhi_u64(wb.q >> (c5 + 1), raw6 as u32);
        let qb = pq & 1;
        if (CHAIN && e >= len) | (pq != qb + qb.wrapping_shl((raw6 as u32).wrapping_sub(1))) {
            return FAIL;
        }
        // field positions
        let (rs2, raw2) = (p.wrapping_add(c1 + 1), c2 - c1 - 1);
        // ingest_ts: its row comes from its opening-quote bit (F1 = 20 bytes, "F1" = 22), so that
        // the decode does not wait for c3; raw3 is only compared with it.
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

/// Bit masks of one event line's two windows (W1 at l, WB at l + OB), made one line ahead.
#[derive(Clone, Copy)]
struct EvMasks {
    w1c: u64,
    /// b'\n' bits of W1 below OB, shifted out of the way (non-zero: the line is shorter than OB)
    nl_head: u64,
    wbc: u64,
    wbq: u64,
}

/// (masks, WB bit of the first b'\n' or 64 if there is none)
#[inline(always)]
unsafe fn ev_masks(p: *const u8) -> (EvMasks, u64) {
    unsafe {
        let z1 = _mm512_loadu_si512(p as *const __m512i);
        let zb = _mm512_loadu_si512(p.wrapping_add(OB) as *const __m512i);
        let (comma, quote, newline) = (_mm512_set1_epi8(b',' as i8), _mm512_set1_epi8(b'"' as i8), _mm512_set1_epi8(b'\n' as i8));
        (
            EvMasks {
                w1c: _mm512_cmpeq_epi8_mask(z1, comma),
                nl_head: _mm512_cmpeq_epi8_mask(z1, newline) << (64 - OB),
                wbc: _mm512_cmpeq_epi8_mask(zb, comma),
                wbq: _mm512_cmpeq_epi8_mask(zb, quote),
            },
            _tzcnt_u64(_mm512_cmpeq_epi8_mask(zb, newline)),
        )
    }
}

/// Fast event line (see the header) from its masks. CHAIN: m_in (WB bit of the b'\n') and
/// k.nl_head are used. !CHAIN: e_in is the position of the line's b'\n'. Anything unusual goes to ev_careful,
/// which starts again from the line start.
#[inline(always)]
unsafe fn ev_fast<S: EventSink, const CHAIN: bool>(
    base: *const u8,
    len: usize,
    l: usize,
    e_in: usize,
    k: &EvMasks,
    m_in: u64,
    cache: *mut DateEnt,
    sink: &mut S,
) -> usize {
    unsafe {
        let p = base.add(l);
        let pb = p.wrapping_add(OB);
        let (m, nl_head) = if CHAIN { (m_in, k.nl_head) } else { (e_in.wrapping_sub(l + OB) as u64, 0) };
        let e = (l + OB).wrapping_add(m as usize);
        // commas 0..2: the lowest three of W1
        let c0 = _tzcnt_u64(k.w1c);
        let t = _blsr_u64(k.w1c);
        let c1 = _tzcnt_u64(t);
        let c2 = _tzcnt_u64(_blsr_u64(t));
        // commas 3..5: the highest three of WB below m
        let tb = _bzhi_u64(k.wbc, m as u32);
        let c5 = 63 ^ _lzcnt_u64(tb);
        let t = _bzhi_u64(tb, c5 as u32);
        let c4 = 63 ^ _lzcnt_u64(t);
        let c3 = 63 ^ _lzcnt_u64(_bzhi_u64(t, c4 as u32));
        // The line is [l, l + OB + m): m in range and inside the buffer; (CHAIN) no b'\n' before
        // WB; 6 commas = those before WB plus those in WB below m; at least three of them in WB
        // (c3 c4 c5 exist) and the third one of the line inside W1 with a byte after it.
        let head = (k.w1c << (64 - OB)).count_ones() as u64;
        let tail = tb.count_ones() as u64;
        // ingest_ts: F1 = 20 raw bytes, "F1" = 22
        let row3 = (OB as u64 + c3).wrapping_sub(c2 + 1);
        // price: the quotes in the raw field must be none, or exactly its first and last byte
        let raw6 = m.wrapping_sub(1).wrapping_sub(c5);
        let pq = _bzhi_u64(k.wbq.wrapping_shr(c5 as u32 + 1), raw6 as u32);
        let qb = pq & 1;
        if (m > 63)
            | (CHAIN && e >= len)
            | (nl_head != 0)
            | (head + tail != 6)
            | (tail < 3)
            | (c2 > 62)
            | (row3 | 2 != 22)
            | (pq != qb + qb.wrapping_shl((raw6 as u32).wrapping_sub(1)))
        {
            return ev_careful::<S, CHAIN>(base, len, l, e_in, cache, sink);
        }
        // here c0 < c1 < c2 <= 62 and c3 < c4 < c5 < m <= 63
        let (c0, c1, c2, c3, c4, c5, row3) = (c0 as usize, c1 as usize, c2 as usize, c3 as usize, c4 as usize, c5 as usize, row3 as usize);
        let raw4 = c4 - c3 - 1;
        // price value. Only "purchase" / "\"purchase\"" have 8 or more raw bytes, so this
        // branch is the type == purchase test (the type bytes are checked below), taken as
        // soon as the commas are known.
        let mut pr = 0;
        if raw4 >= 8 {
            match price(base.wrapping_add(e - qb as usize), raw6 as usize - 2 * qb as usize) {
                Some(v) => pr = v,
                None => return ev_careful::<S, CHAIN>(base, len, l, e_in, cache, sink),
            }
        }
        // ids
        let (se, su) = (T.eid1.get_unchecked(c0), T.uid1.get_unchecked(c1 - c0 - 1));
        let (eid, uid, bad) = ids2(ld(p.wrapping_add(c0).wrapping_sub(16)), se, ld(p.wrapping_add(c1).wrapping_sub(16)), su);
        // ts
        let (rs2, raw2) = (p.wrapping_add(c1 + 1), c2 - c1 - 1);
        let (day, bad_k, _) = ts_date(rs2, raw2, cache);
        let (ts, bad_t) = ts_time::<true>(p.wrapping_add(c2), raw2, day);
        let bad = _mm_or_si128(bad, _mm_or_si128(bad_k, bad_t));
        // ingest_ts
        let rs3 = p.wrapping_add(c2 + 1);
        let (day, bad_k, _) = ts_date(rs3, row3, cache);
        let (ingest, bad_t) = ts_time::<false>(rs3.wrapping_add(row3), row3, day);
        let bad = _mm_or_si128(bad, _mm_or_si128(bad_k, bad_t));
        // product, type
        let sp = T.prod.get_unchecked((c5 - c4 - 1) * 2 + (k.wbq >> (c4 + 1) & 1) as usize);
        let (product, bad_p) = dec(ld(pb.wrapping_add(c5).wrapping_sub(16)), sp);
        let (kind, bad_kind) = kind_of(pb.wrapping_add(c3 + 1), raw4);
        let bad = _mm_or_si128(bad, _mm_or_si128(bad_p, bad_kind));
        if nz(bad) {
            return ev_careful::<S, CHAIN>(base, len, l, e_in, cache, sink);
        }
        sink.event(&EvRow { eid, uid, ts, ingest, kind: kind as u8, product, price: pr });
        e
    }
}

#[cold]
#[inline(never)]
unsafe fn ev_careful<S: EventSink, const CHAIN: bool>(base: *const u8, len: usize, l: usize, e_in: usize, cache: *mut DateEnt, sink: &mut S) -> usize {
    unsafe { ev_core::<S, CHAIN>(base, len, l, e_in, cache, sink) }
}

/// Bit masks of one click line's window, made one line ahead.
#[derive(Clone, Copy)]
struct ClMasks {
    c: u64,
    q: u64,
}

/// (masks, bit of the first b'\n' or 64 if there is none)
#[inline(always)]
unsafe fn cl_masks(p: *const u8) -> (ClMasks, u64) {
    unsafe {
        let w = masks(p);
        (ClMasks { c: w.c, q: w.q }, _tzcnt_u64(w.n))
    }
}

/// One click line starting at l; see ev_core / ev_fast. CAREFUL: tests each field.
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
        // the line fits in the window, has three commas and no 4th
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
        if !CAREFUL {
            let (a, b, bad_id) = ids2(xk, sk, xu, su);
            let (cv, bad_c) = dec(xc, sc);
            let (day, bad_k, _) = ts_date(rs3, raw3, cache);
            let (v, bad_t) = ts_time::<true>(p.wrapping_add(n), raw3, day);
            let bad = _mm_or_si128(_mm_or_si128(bad_id, bad_c), _mm_or_si128(bad_k, bad_t));
            if nz(bad) {
                return cl_careful::<S, CHAIN>(base, len, l, e_in, cache, sink);
            }
            (cid, uid, campaign, ts) = (a, b, cv, v);
        } else {
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

#[inline(always)]
unsafe fn cl_fast<S: ClickSink, const CHAIN: bool>(
    base: *const u8,
    len: usize,
    l: usize,
    e_in: usize,
    w: &ClMasks,
    n_in: u64,
    cache: *mut DateEnt,
    sink: &mut S,
) -> usize {
    unsafe { cl_core::<S, CHAIN, false>(base, len, l, e_in, w, n_in, cache, sink) }
}

#[cold]
#[inline(never)]
unsafe fn cl_careful<S: ClickSink, const CHAIN: bool>(base: *const u8, len: usize, l: usize, e_in: usize, cache: *mut DateEnt, sink: &mut S) -> usize {
    unsafe {
        let (w, n) = cl_masks(base.add(l));
        cl_core::<S, CHAIN, true>(base, len, l, e_in, &w, n, cache, sink)
    }
}

const CHUNK: usize = 8192;

/// How the lines of a buffer are walked.
/// PIPE:  the b'\n' is found in the line's own window; the masks of a line are made during the
///        line before it, at its predicted start, and passed through memory (black_box, so
///        that they are really made there and not where they are used): the load + compare +
///        mask-move latency overlaps the previous line's decode.
/// CHAIN: the same without the one-line-ahead step (fewest instructions).
/// PRE:   a prepass per 8 kB chunk stores the positions of all b'\n'; lines are independent.
const PIPE: u8 = 0;
const CHAIN: u8 = 1;
const PRE: u8 = 2;

/// The line at pos goes to the slow line; finds its end first. Returns the next line start.
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

/// Prepass over one chunk: the offsets (from the chunk start) of all b'\n' in
/// buf[*off..stop]; advances *off to the first 64-byte block at or after stop. Returns the count.
#[inline(always)]
unsafe fn newlines(base: *const u8, off: &mut usize, stop: usize, ends: *mut u32) -> usize {
    unsafe {
        let cstart = *off;
        let mut cnt = 0usize;
        while *off < stop {
            let mut m = masks(base.add(*off)).n;
            if stop - *off < 64 {
                m &= (1u64 << (stop - *off)) - 1; // the buffer's last block
            }
            let rel = (*off - cstart) as u32;
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
            *off += 64;
        }
        cnt
    }
}

macro_rules! runner {
    ($name:ident, $sink:ident, $line:ident, $masks:ident, $adv:expr, $reach:expr, $slow:path, $scan:ident, $reason:literal) => {
        #[inline(never)]
        unsafe fn $name<S: $sink, const MODE: u8>(buf: &[u8], sink: &mut S) {
            unsafe {
                let base = buf.as_ptr();
                let len = buf.len();
                let cache = DATES.with(|c| c.get()) as *mut DateEnt;
                let mut pos = 0usize; // start of the current line
                if MODE == CHAIN {
                    // fast lines need 64 buffer bytes from the line start
                    while pos + 64 <= len {
                        let cur = $masks(base.add(pos));
                        let r = $line::<S, true>(base, len, pos, 0, &cur.0, cur.1, cache, sink);
                        pos = if r == FAIL { $scan(buf, pos, sink) } else { r + 1 };
                    }
                    while pos < len {
                        pos = $scan(buf, pos, sink);
                    }
                    return;
                }
                if MODE == PIPE {
                    'outer: while pos + 64 <= len {
                        let mut cur = $masks(base.add(pos));
                        loop {
                            let next = pos + $adv + cur.1 as usize + 1; // right if this line is a fast line
                            let pre = next + 64 <= len;
                            let nxt = $masks(base.add(if pre { next } else { pos }));
                            let nxt = core::hint::black_box(nxt);
                            let r = $line::<S, true>(base, len, pos, 0, &cur.0, cur.1, cache, sink);
                            if (r == FAIL) | (r.wrapping_add(1) != next) | !pre {
                                pos = if r == FAIL { $scan(buf, pos, sink) } else { r + 1 };
                                continue 'outer;
                            }
                            pos = next;
                            cur = nxt;
                        }
                    }
                    while pos < len {
                        pos = $scan(buf, pos, sink);
                    }
                    return;
                }
                let mut ends_buf = MaybeUninit::<[u32; CHUNK + 2]>::uninit();
                let ends = ends_buf.as_mut_ptr() as *mut u32;
                let mut off = 0usize; // prepass position
                while off < len {
                    let cstart = off;
                    let stop = if len - off > CHUNK { off + CHUNK } else { len };
                    let cnt = newlines(base, &mut off, stop, ends);
                    for i in 0..cnt {
                        let e = cstart + *ends.add(i) as usize;
                        // the windows reach $reach + 64 bytes from the line start: stay in the padding
                        let r = if pos + $reach <= len { $line::<S, false>(base, len, pos, e, &$masks(base.add(pos)).0, 0, cache, sink) } else { FAIL };
                        if r == FAIL {
                            count($reason);
                            $slow(buf, pos, e, sink);
                        }
                        pos = e + 1;
                    }
                }
                if pos < len {
                    count($reason);
                    $slow(buf, pos, len, sink); // final line without b'\n'
                }
            }
        }
    };
}
runner!(run_events, EventSink, ev_fast, ev_masks, OB, OB, slow::event, scan_event, 12);
runner!(run_clicks, ClickSink, cl_fast, cl_masks, 0, 0, slow::click, scan_click, 13);

// ---------------------------------------------------------------------------------------------
// Slow path: the reference code for one line (copied from k_ref.rs)
// ---------------------------------------------------------------------------------------------
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
