use crate::core::Name;
use crate::sys::*;
use crate::*;
use std::sync::atomic::Ordering;

#[inline(always)]
fn user_of(r: &Rec) -> u32 {
    ((r.w1 >> 37) & (ODDU - 1)) as u32
}

#[inline(always)]
pub fn cpush(t: &mut TState, pkey: u64, w0: u64) {
    t.cx_ev[shard(pkey)].push(Rec { w0, w1: pkey });
}

pub fn cid_cmp(g: &G, a: u64, b: u64) -> std::cmp::Ordering {
    if (a | b) & RAW == 0 {
        return enc36(a).0.cmp(&enc36(b).0);
    }
    let (mut sa, mut sb) = (Vec::new(), Vec::new());
    g.id_str(a, b'k', &mut sa);
    g.id_str(b, b'k', &mut sb);
    sa.cmp(&sb)
}

pub unsafe fn process_run(g: &G, t: &mut TState, u: u32, recs: &mut [Rec]) {
    t.n_runs += 1;
    let tv = *g.utab.add(u as usize);
    if tv & (COMPLEX | EXTRA) == EXTRA {
        let mut buf = recs.to_vec();
        buf.extend(g.extra[&u].iter().map(|&w0| Rec { w0, w1: 0 }));
        t.n_odd += (buf.len() - recs.len()) as u64;
        return simple_run(g, t, u, &mut buf, tv);
    }
    simple_run(g, t, u, recs, tv)
}

unsafe fn simple_run(g: &G, t: &mut TState, u: u32, recs: &mut [Rec], tv: u16) {
    if tv & COMPLEX != 0 {
        t.cx_raw.extend(recs.iter().map(|r| Rec { w0: r.w0, w1: u as u64 }));
        t.n_complex += recs.len() as u64;
        return;
    }
    if recs.is_empty() {
        return;
    }
    claim(g, u);
    let mask = (tv & MASK) as u32;
    let mut bits = 0u32;
    let mut sorted = true;
    let mut prev = 0u64;
    for r in recs.iter() {
        bits |= 1 << (r.w0 & 15);
        sorted &= (r.w0 >> 33) >= prev;
        prev = r.w0 >> 33;
    }
    let class = if bits & !mask == 0 {
        mask
    } else if bits & (bits - 1) == 0 {
        bits
    } else {
        t.mixed_users.push(u);
        t.cx_raw.extend(recs.iter().map(|r| Rec { w0: r.w0, w1: u as u64 }));
        t.n_complex += recs.len() as u64;
        return;
    };
    if !sorted {
        recs.sort_unstable_by_key(|r| r.w0);
    }
    t.n_simple += recs.len() as u64;
    let mut st = t.core.begin(recs[0].w0);
    for r in recs.iter() {
        t.core.feed(&mut st, r.w0);
    }
    finish_simple(g, t, u, class, &st);
}

#[inline(always)]
fn class_of(tv: u16, bits: u32) -> u32 {
    let mask = (tv & MASK) as u32;
    if bits & !mask == 0 {
        mask
    } else if bits & (bits - 1) == 0 {
        bits
    } else {
        0
    }
}

#[inline(always)]
unsafe fn finish_simple(g: &G, t: &mut TState, u: u32, class: u32, st: &crate::core::St) {
    let csr = if crate::parse::SKIP & 4 != 0 { &[][..] } else { g.csr_range(u) };
    let mut allin = true;
    for c in csr {
        allin &= (class >> (c[0] & 15)) & 1 != 0;
    }
    if !allin {
        t.ck.clear();
        for c in csr {
            if (class >> (c[0] & 15)) & 1 != 0 {
                t.ck.push(*c);
            }
        }
    }
    let cks: &[[u64; 2]] = if allin { csr } else { &t.ck };
    t.core.finish(g, st, cks, Name::Simple(u, class));
}

#[inline(always)]
unsafe fn claim(g: &G, u: u32) {
    let word = &*g.claim.add(u as usize >> 6);
    let bit = 1u64 << (u & 63);
    if word.fetch_or(bit, Ordering::Relaxed) & bit != 0 {
        fallback("a user has two runs");
    }
}

#[inline(always)]
unsafe fn claim_word(g: &G, word: usize, bits: u64) {
    if bits != 0 && (*g.claim.add(word)).fetch_or(bits, Ordering::Relaxed) & bits != 0 {
        fallback("a user has two runs");
    }
}

#[inline(always)]
unsafe fn edge_copy_w(t: &mut TState, tid: usize, u: u32, w: *const u64, n: usize, cap: usize) -> Seg {
    let len = t.edge.len();
    if len + n > u32::MAX as usize {
        fallback("edge store over 4G records");
    }
    if len + n > cap {
        fallback("avec full");
    }
    let seg = Seg { tid: tid as u32, off: len as u32, len: n as u32, user: u };
    let p = t.edge.as_mut_ptr().add(len);
    let w1 = (u as u64) << 37;
    for i in 0..n {
        p.add(i).write(Rec { w0: *w.add(i), w1 });
    }
    t.edge.truncate(len + n);
    if t.edge.len() != len + n {
        fallback("avec length");
    }
    seg
}

#[inline(always)]
unsafe fn edge_copy(t: &mut TState, tid: usize, u: u32, recs: &[Rec]) -> Seg {
    if t.edge.len() + recs.len() > u32::MAX as usize {
        fallback("edge store over 4G records");
    }
    let seg = Seg { tid: tid as u32, off: t.edge.len() as u32, len: recs.len() as u32, user: u };
    t.edge.extend(recs);
    seg
}

fn stage3a_old(g: &G, tid: usize) {
    let t = g.t(tid);
    t.slots = vec![Rec { w0: u64::MAX, w1: u64::MAX }; BSIZE];
    t.edge = AVec::new(g.ev_bytes / 28 / g.nt * 2 + 65536);
    let slots = t.slots.as_mut_ptr();
    loop {
        let b = g.next.fetch_add(1, Ordering::Relaxed);
        if b >= g.nb {
            break;
        }
        unsafe {
            let ta = std::time::Instant::now();
            std::ptr::write_bytes(slots as *mut u8, 0xFF, BSIZE * 16);
            let mut any = false;
            for th in 0..g.nt {
                g.t(th).ev.for_each(b, |recs| {
                    any = true;
                    for r in recs {
                        let s = slots.add((r.w1 & (BSIZE as u64 - 1)) as usize);
                        if ((r.w1 >> 16) & ING_MAX) < (((*s).w1 >> 16) & ING_MAX) {
                            *s = *r;
                        }
                    }
                });
            }
            let info = &mut *g.edges.add(b);
            info.n = 0;
            if !any {
                continue;
            }
            let tb = std::time::Instant::now();
            let mut k = 0usize;
            for s in 0..BSIZE {
                let r = *slots.add(s);
                if r.w0 >> 33 != INVALID_TS {
                    if r.w1 >> 63 != 0 {
                        t.cx_raw.push(Rec { w0: r.w0, w1: ODDU | user_of(&r) as u64 });
                        t.n_odd += 1;
                    } else {
                        *slots.add(k) = r;
                        k += 1;
                    }
                }
            }
            let tc = std::time::Instant::now();
            let recs = std::slice::from_raw_parts_mut(slots, k);
            let mut i = 0usize;
            if k > 0 {
                let u = user_of(&recs[0]);
                let mut j = 1;
                while j < k && user_of(&recs[j]) == u {
                    j += 1;
                }
                info.first = edge_copy(t, tid, u, &recs[..j]);
                info.n = 1;
                i = j;
            }
            while i < k {
                let u = user_of(&recs[i]);
                let tv = *g.utab.add(u as usize);
                let mut j = i;
                if tv & (COMPLEX | EXTRA) != 0 {
                    while j < k && user_of(&recs[j]) == u {
                        j += 1;
                    }
                    if j == k {
                        info.last = edge_copy(t, tid, u, &recs[i..j]);
                        info.n = 2;
                    } else {
                        process_run(g, t, u, &mut recs[i..j]);
                    }
                    i = j;
                    continue;
                }
                let mut st = t.core.begin(recs[i].w0);
                let (mut bits, mut prev, mut sorted) = (0u32, 0u64, true);
                while j < k {
                    let r = *recs.get_unchecked(j);
                    if user_of(&r) != u {
                        break;
                    }
                    bits |= 1 << (r.w0 & 15);
                    sorted &= r.w0 >> 33 >= prev;
                    prev = r.w0 >> 33;
                    t.core.feed(&mut st, r.w0);
                    j += 1;
                }
                if j == k {
                    t.core.abort(&st);
                    info.last = edge_copy(t, tid, u, &recs[i..j]);
                    info.n = 2;
                    break;
                }
                let class = class_of(tv, bits);
                if class == 0 || !sorted {
                    t.core.abort(&st);
                    process_run(g, t, u, &mut recs[i..j]);
                } else {
                    t.n_runs += 1;
                    t.n_simple += (j - i) as u64;
                    claim(g, u);
                    finish_simple(g, t, u, class, &st);
                }
                i = j;
            }
            let td = std::time::Instant::now();
            t.prof[0] += (tb - ta).as_nanos() as u64;
            t.prof[1] += (tc - tb).as_nanos() as u64;
            t.prof[2] += (td - tc).as_nanos() as u64;
        }
    }
}


use std::arch::x86_64::*;

const PF_CHUNK: bool = true;
const M31: u32 = 0x7FFF_FFFF;
const FLAG: u32 = 0x8000_0000;
const NONE: u32 = u32::MAX;
const GUARD: usize = 16;
const DIV_M: u64 = 6_515_625;

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct V16([u32; 16]);

static ONEHOT: [V16; 16] = {
    let mut t = [V16([0; 16]); 16];
    let mut i = 0;
    while i < 16 {
        t[i].0[i] = 1;
        i += 1;
    }
    t
};

const POW36: [u64; 9] = [1, 36, 1296, 46_656, 1_679_616, 60_466_176, 2_176_782_336, 78_364_164_096, 2_821_109_907_456];

#[inline(always)]
pub fn fast_name(ukey: u64, u: u32, class: u32) -> (u64, usize) {
    let mut m = class;
    let mut best = u64::MAX;
    while m != 0 {
        let a = m.trailing_zeros() as u64;
        m &= m - 1;
        let y = crate::keyrec::scramble((u as u64) << 9 | a, ukey);
        let len = 1 + (y >= POW36[1]) as usize + (y >= POW36[2]) as usize + (y >= POW36[3]) as usize + (y >= POW36[4]) as usize + (y >= POW36[5]) as usize + (y >= POW36[6]) as usize + (y >= POW36[7]) as usize;
        let key = (y * POW36[8 - len]) << 4 | len as u64;
        best = best.min(key);
    }
    if best == u64::MAX {
        return (u64::MAX, 0);
    }
    let (norm, len) = (best >> 4, (best & 15) as usize);
    let hi = (norm / 1_679_616) as u32;
    let lo = (norm % 1_679_616) as u32;
    let p = |v: u32| crate::PAIR36[v as usize] as u64;
    let le = p(hi / 1296) | p(hi % 1296) << 16 | p(lo / 1296) << 32 | p(lo % 1296) << 48;
    (le.swap_bytes() & (u64::MAX << (8 * (8 - len))), len)
}

#[inline(always)]
pub unsafe fn fast_name_v(ukey: u64, u: u32, class: u32) -> (u64, usize) {
    use crate::keyrec::{A, B, MASK as M40};
    let n = class.count_ones();
    if n > 8 || n == 0 {
        return fast_name(ukey, u, class);
    }
    let one = _mm512_set1_epi64(1);
    let m40 = _mm512_set1_epi64(M40 as i64);
    let iota = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
    let al = _mm512_cvtepu32_epi64(_mm512_castsi512_si256(_mm512_maskz_compress_epi32(class as u16, iota)));
    let v = _mm512_or_si512(_mm512_set1_epi64(((u as u64) << 9) as i64), al);
    let mut m = _mm512_and_si512(_mm512_xor_si512(_mm512_mullo_epi64(v, _mm512_set1_epi64(A as i64)), _mm512_set1_epi64(ukey as i64)), m40);
    m = _mm512_xor_si512(m, _mm512_srli_epi64(m, 17));
    let y = _mm512_and_si512(_mm512_mullo_epi64(m, _mm512_set1_epi64(B as i64)), m40);
    let bits = _mm512_sub_epi64(_mm512_set1_epi64(64), _mm512_lzcnt_epi64(_mm512_or_si512(y, one)));
    let g = _mm512_add_epi64(_mm512_srli_epi64(_mm512_mul_epu32(_mm512_sub_epi64(bits, one), _mm512_set1_epi64(99)), 9), one);
    let pow_a = _mm512_loadu_si512(POW36.as_ptr().add(1) as *const _);
    let pow_b = _mm512_loadu_si512(POW36.as_ptr() as *const _);
    let p = _mm512_permutexvar_epi64(_mm512_sub_epi64(g, one), pow_a);
    let len = _mm512_mask_add_epi64(g, _mm512_cmpge_epu64_mask(y, p), g, one);
    let fac = _mm512_permutexvar_epi64(_mm512_sub_epi64(_mm512_set1_epi64(8), len), pow_b);
    let key = _mm512_or_si512(_mm512_slli_epi64(_mm512_mullo_epi64(y, fac), 4), len);
    let best = _mm512_mask_reduce_min_epu64(((1u32 << n) - 1) as u8, key);
    let (norm, len) = (best >> 4, (best & 15) as usize);
    let hi = (norm / 1_679_616) as u32;
    let lo = (norm % 1_679_616) as u32;
    let p = |v: u32| crate::PAIR36[v as usize] as u64;
    let le = p(hi / 1296) | p(hi % 1296) << 16 | p(lo / 1296) << 32 | p(lo % 1296) << 48;
    (le.swap_bytes() & (u64::MAX << (8 * (8 - len))), len)
}

fn name_selftest(g: &G) {
    use crate::keyrec::{unshuffle, A, MASK as M40};
    let mut x = 0x1234_5678_9ABC_DEFu64;
    let bad = |u: u32, class: u32| -> ! {
        eprintln!("[v2b] CHECK FAILED: fast_name({u}, {class:#x})");
        unsafe { crate::sys::_exit(5) }
    };
    for i in 0..600_000u64 {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let u = ((x >> 24) as u32) & (ODDU as u32 - 1);
        let class = if i % 3 == 0 { 1 << ((x >> 8) % NALIAS) } else { ((x >> 3) as u32 & MASK as u32).max(1) };
        let mut key = if i % 2 == 0 { g.ukey } else { (x >> 7) & M40 };
        if i >= 200_000 {
            let j = i - 200_000;
            let y = if j < 100_000 {
                j
            } else if j < 200_000 {
                (POW36[1 + (j % 7) as usize] + (j / 7) % 41).wrapping_sub(20) & M40
            } else {
                (x >> 13) & M40 >> (j % 40)
            };
            let v = (u as u64) << 9 | class.trailing_zeros() as u64;
            key = unshuffle(y) ^ (v.wrapping_mul(A) & M40);
            if crate::keyrec::scramble(v, key) != y {
                bad(u, class);
            }
        }
        let want = crate::core::simple_name_key(key, u, class);
        if want != fast_name(key, u, class) || want != unsafe { fast_name_v(key, u, class) } {
            bad(u, class);
        }
    }
    for y in 0..(1u64 << 22) {
        if (y * DIV_M) >> 39 != y / 84_375 {
            eprintln!("[v2b] CHECK FAILED: day division at {y}");
            unsafe { crate::sys::_exit(5) }
        }
    }
}

struct Fast {
    slots: *mut Rec,
    bm: *mut u64,
    u: *mut u32,
    sidx: *mut u32,
    sts: *mut u32,
    sday: *mut u32,
    scamp: *mut u32,
    blk_ns: *mut u32,
    ssb: *mut u8,
    row_day: *mut u32,
    row_s0: *mut u32,
    row_r0: *mut u32,
    row_rev: *mut u32,
    row_cat: *mut u8,
    blk_nrow: *mut u32,
    rwb: *mut u16,
    run_s0: *mut u32,
    run_row0: *mut u32,
    run_pidx: *mut u32,
    pur_w: *mut u64,
    pur_i: *mut u32,
    mix: Vec<u32>,
    f: Vec<V16>,
    last: *mut u64,
    serial: u32,
    asst: *mut V16,
    cred: *mut u32,
    rc: *mut Rc,
    wl_s: *mut u32,
    wl_r: *mut u32,
    alist: *mut u32,
}

#[repr(C, align(64))]
#[derive(Clone, Copy)]
struct Rc {
    ts: [u32; 16],
    camp: [u32; 16],
    conf: [u32; 16],
    vm: u32,
    pad: [u32; 15],
}
const RC_N: usize = 128;
const WL_CAP: usize = 4096;
const WL_RUN: usize = 1024;
const AL_CAP: usize = 4096;

#[inline(never)]
unsafe fn light_flush(fx: &mut Fast, nw: usize, org: u32) {
    let (sts, sday, scamp, rc, wl_s, wl_r, list) = (fx.sts, fx.sday, fx.scamp, fx.rc, fx.wl_s, fx.wl_r, fx.alist);
    let af = fx.asst as *mut u32;
    let mut nl = 0usize;
    for i in 0..nw {
        if nl > AL_CAP {
            for j in 0..nl {
                *af.add(*list.add(j) as usize) += 1;
            }
            nl = 0;
        }
        let x = *wl_s.add(i) as usize;
        let rcp = rc.add(*wl_r.add(i) as usize);
        let s = *sts.add(x);
        let d = *sday.add(x);
        let vm = (*rcp).vm;
        let ts = _mm512_load_si512((*rcp).ts.as_ptr() as *const _);
        let him = _mm512_cmple_epu32_mask(ts, _mm512_set1_epi32((s + WEEK_MS as u32) as i32)) as u32 & vm;
        let lom = _mm512_cmplt_epu32_mask(ts, _mm512_set1_epi32(s as i32)) as u32 & vm;
        let win = him & !lom;
        let cv = *(*rcp).camp.as_ptr().add((31u32.wrapping_sub(win.leading_zeros()) & 15) as usize);
        let sel = ((win != 0) as u32).wrapping_neg();
        *scamp.add(x) = org ^ ((org ^ cv) & sel);
        let am = win & _mm512_cmplt_epu32_mask(_mm512_load_si512((*rcp).conf.as_ptr() as *const _), _mm512_set1_epi32((lom + 1) as i32)) as u32;
        let cells = _mm512_add_epi32(_mm512_slli_epi32(_mm512_load_si512((*rcp).camp.as_ptr() as *const _), 4), _mm512_set1_epi32(d as i32));
        _mm512_storeu_si512(list.add(nl) as *mut _, _mm512_maskz_compress_epi32(am as u16, cells));
        nl += am.count_ones() as usize;
    }
    for i in 0..nl {
        *af.add(*list.add(i) as usize) += 1;
    }
}
impl Fast {
    fn new(nc: usize, nd: usize) -> Fast {
        let n = BSIZE + GUARD + 64;
        let r64 = |x: usize| (x + 63) & !63;
        let dense = r64(16 * (BSIZE + 16)) + 9 * r64(4 * n);
        let sparse = r64(8 * (BSIZE / 64 + 2)) + r64(4 * (BSIZE / 8 + 2)) + r64(BSIZE / 8 + 2) + r64(n) + r64(4 * (BSIZE / 16 + 2)) + r64(2 * (BSIZE / 16 + 2)) + 3 * r64(4 * n) + r64(8 * n) + r64(4 * n) + r64(8 * (CBOUND + 2)) + r64(64 * (nc + 2)) + r64(4 * (nc + 2) * nd) + r64(256 * RC_N) + 2 * r64(4 * (WL_CAP + WL_RUN + 64)) + r64(4 * (AL_CAP + 64));
        unsafe {
            let hp = anon(dense);
            let sp = anon(sparse);
            madvise(sp, sparse, MADV_NOHUGEPAGE);
            let (mut h, mut q) = (0usize, 0usize);
            let take = |base: *mut u8, cur: &mut usize, bytes: usize| -> *mut u8 {
                let p = base.add(*cur);
                *cur += r64(bytes);
                p
            };
            let fx = Fast {
                slots: (take(hp, &mut h, 16 * (BSIZE + 16)) as *mut Rec).add(8),
                u: (take(hp, &mut h, 4 * n) as *mut u32).add(GUARD),
                sidx: take(hp, &mut h, 4 * n) as *mut u32,
                sts: take(hp, &mut h, 4 * n) as *mut u32,
                sday: (take(hp, &mut h, 4 * n) as *mut u32).add(GUARD),
                scamp: take(hp, &mut h, 4 * n) as *mut u32,
                row_day: take(hp, &mut h, 4 * n) as *mut u32,
                row_s0: take(hp, &mut h, 4 * n) as *mut u32,
                row_r0: take(hp, &mut h, 4 * n) as *mut u32,
                row_rev: take(hp, &mut h, 4 * n) as *mut u32,
                bm: take(sp, &mut q, 8 * (BSIZE / 64 + 2)) as *mut u64,
                blk_ns: take(sp, &mut q, 4 * (BSIZE / 8 + 2)) as *mut u32,
                ssb: take(sp, &mut q, BSIZE / 8 + 2),
                row_cat: take(sp, &mut q, n),
                blk_nrow: take(sp, &mut q, 4 * (BSIZE / 16 + 2)) as *mut u32,
                rwb: take(sp, &mut q, 2 * (BSIZE / 16 + 2)) as *mut u16,
                run_s0: take(sp, &mut q, 4 * n) as *mut u32,
                run_row0: take(sp, &mut q, 4 * n) as *mut u32,
                run_pidx: take(sp, &mut q, 4 * n) as *mut u32,
                pur_w: take(sp, &mut q, 8 * n) as *mut u64,
                pur_i: take(sp, &mut q, 4 * n) as *mut u32,
                mix: Vec::new(),
                f: vec![V16([0; 16]); 64],
                last: take(sp, &mut q, 8 * (CBOUND + 2)) as *mut u64,
                serial: 0,
                asst: take(sp, &mut q, 64 * (nc + 2)) as *mut V16,
                cred: take(sp, &mut q, 4 * (nc + 2) * nd) as *mut u32,
                rc: take(sp, &mut q, 256 * RC_N) as *mut Rc,
                wl_s: take(sp, &mut q, 4 * (WL_CAP + WL_RUN + 64)) as *mut u32,
                wl_r: take(sp, &mut q, 4 * (WL_CAP + WL_RUN + 64)) as *mut u32,
                alist: take(sp, &mut q, 4 * (AL_CAP + 64)) as *mut u32,
            };
            if h > dense || q > sparse {
                fallback("scratch layout");
            }
            fx
        }
    }
}

#[inline(always)]
unsafe fn run_clicks(fx: &mut Fast, cks: &[[u64; 2]], class: u32, s0: usize, nsr: usize, org: u32) {
    let sts = fx.sts.add(s0);
    let sday = fx.sday.add(s0);
    let sc = fx.scamp.add(s0);
    if fx.f.len() < nsr + 1 {
        fx.f.resize(nsr + 1, V16([0; 16]));
    }
    let f = fx.f.as_mut_ptr();
    let mut acc = _mm512_setzero_si512();
    _mm512_store_si512(f as *mut _, acc);
    for x in 0..nsr {
        acc = _mm512_add_epi32(acc, _mm512_load_si512(ONEHOT.as_ptr().add(*sday.add(x) as usize) as *const _));
        _mm512_store_si512(f.add(x + 1) as *mut _, acc);
    }
    fx.serial = fx.serial.wrapping_add(1);
    if fx.serial == 0 {
        std::ptr::write_bytes(fx.last, 0, CBOUND + 2);
        fx.serial = 1;
    }
    let serial = fx.serial as u64;
    let last = fx.last;
    let asst = fx.asst;
    let (mut lo_p, mut hi_p, mut c_p) = (0usize, 0usize, org);
    let orgv = _mm512_set1_epi32(org as i32);
    let week = _mm512_set1_epi32(WEEK_MS as i32);
    let vmask = |rem: usize| -> u32 { if rem >= 16 { 0xFFFF } else { (1u32 << rem) - 1 } };
    let (mut lob, mut hib) = (0usize, 0usize);
    let mut lov = _mm512_add_epi32(_mm512_loadu_si512(sts as *const _), week);
    let mut hiv = _mm512_loadu_si512(sts as *const _);
    let (mut lom, mut him) = (vmask(nsr), vmask(nsr));
    for k in cks {
        let k0 = k[0];
        if (class >> (k0 & 15)) & 1 == 0 {
            continue;
        }
        let t = _mm512_set1_epi32((k0 >> 32) as i32);
        let c = ((k0 >> 4) & (CBOUND as u64 - 1)) as u32;
        let mut n = (_mm512_cmplt_epu32_mask(lov, t) as u32 & lom).count_ones() as usize;
        while n == 16 {
            lob += 16;
            lov = _mm512_add_epi32(_mm512_loadu_si512(sts.add(lob) as *const _), week);
            lom = vmask(nsr - lob);
            n = (_mm512_cmplt_epu32_mask(lov, t) as u32 & lom).count_ones() as usize;
        }
        let lo = lob + n;
        let mut n = (_mm512_cmple_epu32_mask(hiv, t) as u32 & him).count_ones() as usize;
        while n == 16 {
            hib += 16;
            hiv = _mm512_loadu_si512(sts.add(hib) as *const _);
            him = vmask(nsr - hib);
            n = (_mm512_cmple_epu32_mask(hiv, t) as u32 & him).count_ones() as usize;
        }
        let hi = hib + n;
        let to = lo.min(hi_p);
        let cv = _mm512_set1_epi32(c_p as i32);
        let mut x = lo_p;
        loop {
            _mm512_storeu_si512(sc.add(x) as *mut _, cv);
            x += 16;
            if x >= to {
                break;
            }
        }
        _mm512_storeu_si512(sc.add(to) as *mut _, orgv);
        lo_p = lo;
        hi_p = hi;
        c_p = c;
        let e = *last.add(c as usize);
        let prevhi = if e >> 32 == serial { (e & 0xFFFF_FFFF) as usize } else { 0 };
        *last.add(c as usize) = serial << 32 | hi as u64;
        let from = lo.max(prevhi);
        let row = asst.add(c as usize) as *mut __m512i;
        let d = _mm512_sub_epi32(_mm512_load_si512(f.add(hi) as *const _), _mm512_load_si512(f.add(from) as *const _));
        _mm512_store_si512(row, _mm512_add_epi32(_mm512_load_si512(row), d));
    }
    let to = nsr.min(hi_p);
    let cv = _mm512_set1_epi32(c_p as i32);
    let mut x = lo_p;
    loop {
        _mm512_storeu_si512(sc.add(x) as *mut _, cv);
        x += 16;
        if x >= to {
            break;
        }
    }
    _mm512_storeu_si512(sc.add(to) as *mut _, orgv);
}

fn stage3a(g: &G, tid: usize) {
    let t = g.t(tid);
    let edge_cap = g.ev_bytes / 28 / g.nt * 2 + 65536;
    t.edge = AVec::new(edge_cap);
    let mut fx = Fast::new(g.nc, g.nd);
    let nd = g.nd;
    let org = g.nc as u32;
    let trash = g.nc as u32 + 1;
    let row_cap = g.row_cap;
    unsafe {
        let slots = fx.slots;
        let w = slots as *mut u64;
        let (bm, up, sidx, sts, sday, scamp, blk_ns, ssb) = (fx.bm, fx.u, fx.sidx, fx.sts, fx.sday, fx.scamp, fx.blk_ns, fx.ssb);
        let (row_day, row_s0, row_r0, row_rev, row_cat, blk_nrow, rwb) = (fx.row_day, fx.row_s0, fx.row_r0, fx.row_rev, fx.row_cat, fx.blk_nrow, fx.rwb);
        let (run_s0, run_row0, run_pidx, pur_w, pur_i) = (fx.run_s0, fx.run_row0, fx.run_pidx, fx.pur_w, fx.pur_i);
        let utab_g = g.utab as *const i32;
        let ev_idx = _mm512_setr_epi64(0, 2, 4, 6, 8, 10, 12, 14);
        let od_idx = _mm512_setr_epi64(1, 3, 5, 7, 9, 11, 13, 15);
        let iota8 = _mm256_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7);
        let iota16 = _mm512_setr_epi32(0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15);
        loop {
            let b = g.next.fetch_add(1, Ordering::Relaxed);
            if b >= g.nb {
                break;
            }
            let ta = std::time::Instant::now();
            std::ptr::write_bytes(bm, 0, BSIZE / 64);
            let mut any = false;
            for th in 0..g.nt {
                g.t(th).ev.for_each(b, |recs| {
                    any = true;
                    let n = recs.len();
                    let p = recs.as_ptr();
                    let nextc = if PF_CHUNK && n == crate::parse::LINK_OFF / 16 { *((p as usize + crate::parse::LINK_OFF) as *const usize) as *const i8 } else { std::ptr::null() };
                    let put = |r: *const Rec| {
                        let w1 = (*r).w1;
                        let sl = (w1 & (BSIZE as u64 - 1)) as usize;
                        let wp = bm.add(sl >> 6);
                        let wd = *wp;
                        let s = slots.add(sl);
                        if wd >> (sl & 63) & 1 == 0 {
                            *wp = wd | 1u64 << (sl & 63);
                            *s = *r;
                        } else if ((w1 >> 16) & ING_MAX) < (((*s).w1 >> 16) & ING_MAX) {
                            *s = *r;
                        }
                    };
                    let mut i = 0usize;
                    while i + 4 <= n {
                        if PF_CHUNK {
                            _mm_prefetch::<_MM_HINT_T0>(nextc.wrapping_add(i * 16));
                        }
                        put(p.add(i));
                        put(p.add(i + 1));
                        put(p.add(i + 2));
                        put(p.add(i + 3));
                        i += 4;
                    }
                    while i < n {
                        put(p.add(i));
                        i += 1;
                    }
                });
            }
            let info = &mut *g.edges.add(b);
            info.n = 0;
            if !any {
                continue;
            }
            let tb = std::time::Instant::now();
            let mut k = 0usize;
            let bmb = bm as *const u8;
            let inv = _mm512_set1_epi64(INVALID_TS as i64);
            for blk in 0..BSIZE / 8 {
                let occ = *bmb.add(blk);
                let r0 = _mm512_loadu_si512(slots.add(8 * blk) as *const _);
                let r1 = _mm512_loadu_si512(slots.add(8 * blk + 4) as *const _);
                let w0 = _mm512_permutex2var_epi64(r0, ev_idx, r1);
                let w1 = _mm512_permutex2var_epi64(r0, od_idx, r1);
                let valid = _mm512_mask_cmpneq_epu64_mask(occ, _mm512_srli_epi64(w0, 33), inv);
                let odd = valid & _mm512_movepi64_mask(w1);
                if odd != 0 {
                    let mut m = odd as u32;
                    while m != 0 {
                        let r = *slots.add(8 * blk + m.trailing_zeros() as usize);
                        m &= m - 1;
                        t.cx_raw.push(Rec { w0: r.w0, w1: ODDU | user_of(&r) as u64 });
                        t.n_odd += 1;
                    }
                }
                let keep = valid & !odd;
                let uv = _mm256_and_si256(_mm512_cvtepi64_epi32(_mm512_srli_epi64(w1, 37)), _mm256_set1_epi32(ODDU as i32 - 1));
                _mm512_storeu_si512(w.add(k) as *mut _, _mm512_maskz_compress_epi64(keep, w0));
                _mm256_storeu_si256(up.add(k) as *mut _, _mm256_maskz_compress_epi32(keep, uv));
                k += keep.count_ones() as usize;
            }
            _mm256_storeu_si256(up.add(k) as *mut _, _mm256_setzero_si256());
            let tc = std::time::Instant::now();
            if k == 0 {
                t.prof[0] += (tb - ta).as_nanos() as u64;
                t.prof[1] += (tc - tb).as_nanos() as u64;
                continue;
            }
            *up.sub(1) = u32::MAX;
            let (mut ns, mut np) = (0usize, 0usize);
            {
                let gapv = _mm512_set1_epi64(GAP_MS as i64);
                let pmask = _mm512_set1_epi64((0xFF_FFFFu64 << 9) as i64);
                let c15 = _mm256_set1_epi32(15);
                let mut carry = 0u32;
                let mut cur = 0u32;
                fx.mix.clear();
                let mut i = 0usize;
                while i < k {
                    let rem = k - i;
                    let tail: u32 = if rem >= 8 { 0xFF } else { (1u32 << rem) - 1 };
                    let wv = _mm512_loadu_si512(w.add(i) as *const _);
                    let wpv = _mm512_loadu_si512(w.add(i).sub(1) as *const _);
                    let uv = _mm256_loadu_si256(up.add(i) as *const _);
                    let upv = _mm256_loadu_si256(up.add(i).sub(1) as *const _);
                    let rs = _mm256_cmpneq_epi32_mask(uv, upv) as u32 & tail;
                    let tv = _mm512_srli_epi64(wv, 33);
                    let tpv = _mm512_srli_epi64(wpv, 33);
                    let gap = _mm512_cmpgt_epu64_mask(_mm512_sub_epi64(tv, tpv), gapv) as u32;
                    let ss = (rs | gap) & tail;
                    let un = _mm512_cmplt_epu64_mask(tv, tpv) as u32 & !rs;
                    let ut = if rs & rs.wrapping_sub(1) == 0 {
                        let p = (rs | 0x100).trailing_zeros();
                        let new = *g.utab.add(*up.add(i + p.min(7) as usize) as usize) as u32;
                        let v = _mm256_mask_mov_epi32(_mm256_set1_epi32(cur as i32), (0xFFu32 << p) as u8, _mm256_set1_epi32(new as i32));
                        cur = if p < 8 { new } else { cur };
                        v
                    } else {
                        cur = *g.utab.add(*up.add(i + 7) as usize) as u32;
                        _mm256_i32gather_epi32::<2>(utab_g, uv)
                    };
                    let a = _mm256_and_si256(_mm512_cvtepi64_epi32(wv), c15);
                    let ap = _mm256_and_si256(_mm512_cvtepi64_epi32(wpv), c15);
                    let abit = _mm256_sllv_epi32(_mm256_set1_epi32(1), a);
                    let bad = _mm256_test_epi32_mask(abit, _mm256_andnot_si256(ut, _mm256_set1_epi32(MASK as i32))) as u32;
                    let ac = _mm256_cmpneq_epi32_mask(a, ap) as u32 & !rs;
                    let badp = ((bad << 1) | carry) & 0xFF;
                    carry = (bad >> 7) & 1;
                    let special = (un | (ac & (bad | badp))) & tail;
                    if special != 0 {
                        let mut m = special;
                        while m != 0 {
                            fx.mix.push((i + m.trailing_zeros() as usize) as u32);
                            m &= m - 1;
                        }
                    }
                    let base = _mm256_add_epi32(_mm256_set1_epi32(i as i32), iota8);
                    let idxf = _mm256_mask_or_epi32(base, rs as u8, base, _mm256_set1_epi32(FLAG as i32));
                    _mm256_storeu_si256(sidx.add(ns) as *mut _, _mm256_maskz_compress_epi32(ss as u8, idxf));
                    _mm256_storeu_si256(sts.add(ns) as *mut _, _mm256_maskz_compress_epi32(ss as u8, _mm512_cvtepi64_epi32(tv)));
                    *blk_ns.add(i >> 3) = ns as u32;
                    *ssb.add(i >> 3) = ss as u8;
                    ns += ss.count_ones() as usize;
                    let pm = _mm512_test_epi64_mask(wv, pmask) as u32 & tail;
                    _mm512_storeu_si512(pur_w.add(np) as *mut _, _mm512_maskz_compress_epi64(pm as u8, wv));
                    _mm256_storeu_si256(pur_i.add(np) as *mut _, _mm256_maskz_compress_epi32(pm as u8, base));
                    np += pm.count_ones() as usize;
                    i += 8;
                }
                fx.mix.push(u32::MAX);
            }
            *sidx.add(ns) = k as u32 | FLAG;
            let mut nrow = 0usize;
            {
                let wsm = _mm512_set1_epi32(g.ws_mod as i32);
                let mg = _mm512_set1_epi64(DIV_M as i64);
                let flagv = _mm512_set1_epi32(FLAG as i32);
                let m31v = _mm512_set1_epi32(M31 as i32);
                *sday.sub(1) = u32::MAX;
                let mut s = 0usize;
                while s < ns {
                    let rem = ns - s;
                    let tail: u32 = if rem >= 16 { 0xFFFF } else { (1u32 << rem) - 1 };
                    let x = _mm512_srli_epi32(_mm512_add_epi32(_mm512_loadu_si512(sts.add(s) as *const _), wsm), 10);
                    let pe = _mm512_srli_epi64(_mm512_mul_epu32(x, mg), 39);
                    let po = _mm512_srli_epi64(_mm512_mul_epu32(_mm512_srli_epi64(x, 32), mg), 39);
                    let day = _mm512_or_si512(pe, _mm512_slli_epi64(po, 32));
                    _mm512_storeu_si512(sday.add(s) as *mut _, day);
                    let dprev = _mm512_loadu_si512(sday.add(s).sub(1) as *const _);
                    let iv = _mm512_loadu_si512(sidx.add(s) as *const _);
                    let rw = ((_mm512_movepi32_mask(iv) | _mm512_cmpneq_epi32_mask(day, dprev)) as u32 & tail) as u16;
                    *blk_nrow.add(s >> 4) = nrow as u32;
                    *rwb.add(s >> 4) = rw;
                    let ord = _mm512_or_si512(_mm512_add_epi32(_mm512_set1_epi32(s as i32), iota16), _mm512_and_si512(iv, flagv));
                    _mm512_storeu_si512(row_day.add(nrow) as *mut _, _mm512_maskz_compress_epi32(rw, day));
                    _mm512_storeu_si512(row_s0.add(nrow) as *mut _, _mm512_maskz_compress_epi32(rw, ord));
                    _mm512_storeu_si512(row_r0.add(nrow) as *mut _, _mm512_maskz_compress_epi32(rw, _mm512_and_si512(iv, m31v)));
                    nrow += rw.count_ones() as usize;
                    s += 16;
                }
                *row_s0.add(nrow) = ns as u32 | FLAG;
                *row_r0.add(nrow) = k as u32;
            }
            let mut nrun = 0usize;
            {
                let m31v = _mm512_set1_epi32(M31 as i32);
                let mut r = 0usize;
                while r < nrow {
                    let rem = nrow - r;
                    let tail: u32 = if rem >= 16 { 0xFFFF } else { (1u32 << rem) - 1 };
                    let v = _mm512_loadu_si512(row_s0.add(r) as *const _);
                    let m = (_mm512_movepi32_mask(v) as u32 & tail) as u16;
                    _mm512_storeu_si512(run_s0.add(nrun) as *mut _, _mm512_maskz_compress_epi32(m, _mm512_and_si512(v, m31v)));
                    _mm512_storeu_si512(run_row0.add(nrun) as *mut _, _mm512_maskz_compress_epi32(m, _mm512_add_epi32(_mm512_set1_epi32(r as i32), iota16)));
                    nrun += m.count_ones() as usize;
                    r += 16;
                }
                *run_s0.add(nrun) = ns as u32;
                *run_row0.add(nrun) = nrow as u32;
            }
            std::ptr::write_bytes(row_rev, 0, nrow);
            std::ptr::write_bytes(row_cat, 255, nrow);
            let (mut cw, mut cb) = (0usize, 0u64);
            let (mut nw, mut nrc) = (0usize, 0usize);
            let mut mixp = 0usize;
            let orgv = _mm512_set1_epi32(org as i32);
            let trashv = _mm512_set1_epi32(trash as i32);
            for r in 0..nrun {
                let s0 = *run_s0.add(r) as usize;
                let s1 = *run_s0.add(r + 1) as usize;
                let a = (*sidx.add(s0) & M31) as usize;
                let e = (*sidx.add(s1) & M31) as usize;
                let u = *up.add(a);
                let tv = *g.utab.add(u as usize);
                while (*fx.mix.get_unchecked(mixp) as usize) < a {
                    mixp += 1;
                }
                let special = (*fx.mix.get_unchecked(mixp) as usize) < e;
                let edge = r == 0 || r + 1 == nrun;
                if edge || special || tv & (COMPLEX | EXTRA) != 0 {
                    *run_pidx.add(r) = NONE;
                    let mut x = s0;
                    while x < s1 {
                        _mm512_storeu_si512(scamp.add(x) as *mut _, trashv);
                        x += 16;
                    }
                    if r == 0 {
                        info.first = edge_copy_w(t, tid, u, w.add(a), e - a, edge_cap);
                        info.n = 1;
                    } else if r + 1 == nrun {
                        info.last = edge_copy_w(t, tid, u, w.add(a), e - a, edge_cap);
                        info.n = 2;
                    } else {
                        let mut buf = std::mem::take(&mut t.runbuf);
                        buf.clear();
                        for i in a..e {
                            buf.push(Rec { w0: *w.add(i), w1: (u as u64) << 37 });
                        }
                        process_run(g, t, u, &mut buf);
                        t.runbuf = buf;
                    }
                    continue;
                }
                t.n_runs += 1;
                t.n_simple += (e - a) as u64;
                if u as usize >> 6 != cw {
                    claim_word(g, cw, cb);
                    cw = u as usize >> 6;
                    cb = 0;
                }
                if cb >> (u & 63) & 1 != 0 {
                    fallback("a user has two runs");
                }
                cb |= 1u64 << (u & 63);
                let mask = (tv & MASK) as u32;
                let bit = 1u32 << (*w.add(a) & 15);
                let class = if bit & mask != 0 { mask } else { bit };
                let core = &mut t.core;
                let nl = core.names.len();
                if nl + 16 > row_cap * 10 {
                    fallback("avec full");
                }
                let (v, len) = fast_name_v(g.ukey, u, class);
                let p = core.names.as_mut_ptr().add(nl);
                *p = b'u';
                (p.add(1) as *mut [u8; 8]).write_unaligned(v.to_be_bytes());
                let nl = nl + 1 + len;
                core.names.truncate(nl);
                if nl > u32::MAX as usize {
                    fallback("names over 4 GB");
                }
                *run_pidx.add(r) = (core.names_off.len() - 1) as u32;
                core.names_off.push(nl as u32);
                core.n_sess += (s1 - s0) as u64;
                let mut x = s0;
                while x < s1 {
                    _mm512_storeu_si512(scamp.add(x) as *mut _, orgv);
                    x += 16;
                }
                let cks = g.csr_range(u);
                let n = cks.len();
                let nsr = s1 - s0;
                let mut light = false;
                if n <= 16 && nsr <= WL_RUN {
                    if nrc == RC_N || nw + nsr > WL_CAP {
                        light_flush(&mut fx, nw, org);
                        nw = 0;
                        nrc = 0;
                    }
                    let p = cks.as_ptr() as *const __m512i;
                    let ka = _mm512_permutex2var_epi64(_mm512_loadu_si512(p), ev_idx, _mm512_loadu_si512(p.add(1)));
                    let kb = _mm512_permutex2var_epi64(_mm512_loadu_si512(p.add(2)), ev_idx, _mm512_loadu_si512(p.add(3)));
                    let lo32 = _mm512_inserti64x4::<1>(_mm512_castsi256_si512(_mm512_cvtepi64_epi32(ka)), _mm512_cvtepi64_epi32(kb));
                    let ts = _mm512_inserti64x4::<1>(_mm512_castsi256_si512(_mm512_cvtepi64_epi32(_mm512_srli_epi64(ka, 32))), _mm512_cvtepi64_epi32(_mm512_srli_epi64(kb, 32)));
                    let vm = (1u32 << n) - 1;
                    let inc = _mm512_test_epi32_mask(_mm512_sllv_epi32(_mm512_set1_epi32(1), _mm512_and_si512(lo32, _mm512_set1_epi32(15))), _mm512_set1_epi32(class as i32)) as u32;
                    if inc & vm == vm {
                        light = true;
                        let camp = _mm512_and_si512(_mm512_srli_epi32(lo32, 4), _mm512_set1_epi32(CBOUND as i32 - 1));
                        let ri = nrc;
                        nrc += 1;
                        let rcp = fx.rc.add(ri);
                        _mm512_store_si512((*rcp).ts.as_mut_ptr() as *mut _, ts);
                        _mm512_store_si512((*rcp).camp.as_mut_ptr() as *mut _, camp);
                        _mm512_store_si512((*rcp).conf.as_mut_ptr() as *mut _, _mm512_conflict_epi32(camp));
                        (*rcp).vm = vm;
                        let wl_s = fx.wl_s.add(nw);
                        let wl_r = fx.wl_r.add(nw);
                        let rv = _mm512_set1_epi32(ri as i32);
                        let mut x = 0usize;
                        loop {
                            _mm512_storeu_si512(wl_s.add(x) as *mut _, _mm512_add_epi32(_mm512_set1_epi32((s0 + x) as i32), iota16));
                            _mm512_storeu_si512(wl_r.add(x) as *mut _, rv);
                            x += 16;
                            if x >= nsr {
                                break;
                            }
                        }
                        nw += if n != 0 { nsr } else { 0 };
                    }
                }
                if !light {
                    run_clicks(&mut fx, cks, class, s0, nsr, org);
                }
            }
            light_flush(&mut fx, nw, org);
            claim_word(g, cw, cb);
            {
                let cred = fx.cred;
                for s in 0..ns {
                    let cell = *scamp.add(s) as usize * nd + *sday.add(s) as usize;
                    *cred.add(cell) += 1;
                }
            }
            {
                let aggp = t.core.aggp.as_mut_ptr();
                let mut csum = [0u64; 32];
                let mut crow = [u32::MAX; 32];
                let (mut prev_sid, mut cur_row, mut best_val, mut best_cat) = (u32::MAX, u32::MAX, 0u64, 255u32);
                for p in 0..np {
                    let i = *pur_i.add(p) as usize;
                    let wv = *pur_w.add(p);
                    let price = (wv >> 9) & 0xFF_FFFF;
                    let cat = ((wv >> 4) & 31) as u32;
                    let sid = *blk_ns.add(i >> 3) + (*ssb.add(i >> 3) as u32 & ((2u32 << (i & 7)) - 1)).count_ones() - 1;
                    let cell = *scamp.add(sid as usize) as usize * nd + *sday.add(sid as usize) as usize;
                    (*aggp.add(cell))[0] += (sid != prev_sid) as u64;
                    (*aggp.add(cell))[1] += price;
                    prev_sid = sid;
                    let row = *blk_nrow.add(sid as usize >> 4) + (*rwb.add(sid as usize >> 4) as u32 & ((2u32 << (sid & 15)) - 1)).count_ones() - 1;
                    let v = if crow[cat as usize] == row { csum[cat as usize] } else { 0 } + price;
                    csum[cat as usize] = v;
                    crow[cat as usize] = row;
                    if row != cur_row {
                        best_val = 0;
                        best_cat = 255;
                    }
                    cur_row = row;
                    if v > best_val || (v == best_val && cat < best_cat) {
                        best_val = v;
                        best_cat = cat;
                    }
                    let rv = *row_rev.add(row as usize) as u64 + price;
                    *row_rev.add(row as usize) = rv.min(0x8000_0000) as u32;
                    *row_cat.add(row as usize) = best_cat as u8;
                }
            }
            {
                let core = &mut t.core;
                let mut n = core.day.len();
                if core.keys.len() != n || core.ses.len() != n || core.evs.len() != n || core.rev.len() != n || core.cat.len() != n {
                    fallback("row columns differ in length");
                }
                let (keys, dayc, sesc, evsc, revc, catc) = (core.keys.as_mut_ptr(), core.day.as_mut_ptr(), core.ses.as_mut_ptr(), core.evs.as_mut_ptr(), core.rev.as_mut_ptr(), core.cat.as_mut_ptr());
                let day0 = _mm512_set1_epi32(g.day0);
                let m31v = _mm512_set1_epi32(M31 as i32);
                let maxv = _mm512_set1_epi32(i32::MAX);
                for r in 0..nrun {
                    let pidx = *run_pidx.add(r);
                    if pidx == NONE {
                        continue;
                    }
                    let ra = *run_row0.add(r) as usize;
                    let rb = *run_row0.add(r + 1) as usize;
                    if n + (rb - ra) + 16 > row_cap {
                        fallback("avec full");
                    }
                    let pv = _mm512_set1_epi32(pidx as i32);
                    let mut x = ra;
                    while x < rb {
                        let o = n + (x - ra);
                        _mm512_storeu_si512(keys.add(o) as *mut _, pv);
                        _mm512_storeu_si512(dayc.add(o) as *mut _, _mm512_add_epi32(_mm512_loadu_si512(row_day.add(x) as *const _), day0));
                        let sa = _mm512_and_si512(_mm512_loadu_si512(row_s0.add(x) as *const _), m31v);
                        let sb = _mm512_and_si512(_mm512_loadu_si512(row_s0.add(x + 1) as *const _), m31v);
                        _mm512_storeu_si512(sesc.add(o) as *mut _, _mm512_sub_epi32(sb, sa));
                        _mm512_storeu_si512(evsc.add(o) as *mut _, _mm512_sub_epi32(_mm512_loadu_si512(row_r0.add(x + 1) as *const _), _mm512_loadu_si512(row_r0.add(x) as *const _)));
                        let rv = _mm512_loadu_si512(row_rev.add(x) as *const _);
                        let tail: u32 = if rb - x >= 16 { 0xFFFF } else { (1u32 << (rb - x)) - 1 };
                        if _mm512_cmpgt_epu32_mask(rv, maxv) as u32 & tail != 0 {
                            fallback("revenue_cents or events over i32");
                        }
                        _mm512_storeu_si512(revc.add(o) as *mut _, rv);
                        _mm_storeu_si128(catc.add(o) as *mut _, _mm_loadu_si128(row_cat.add(x) as *const _));
                        x += 16;
                    }
                    n += rb - ra;
                }
                core.keys.truncate(n);
                core.day.truncate(n);
                core.ses.truncate(n);
                core.evs.truncate(n);
                core.rev.truncate(n);
                core.cat.truncate(n);
                if core.day.len() != n {
                    fallback("avec length");
                }
            }
            let td = std::time::Instant::now();
            t.prof[0] += (tb - ta).as_nanos() as u64;
            t.prof[1] += (tc - tb).as_nanos() as u64;
            t.prof[2] += (td - tc).as_nanos() as u64;
        }
        for c in 0..=g.nc {
            for d in 0..nd {
                t.core.agg[c * nd + d][0] += *fx.cred.add(c * nd + d);
                t.core.agg[c * nd + d][1] += (*fx.asst.add(c)).0[d];
            }
        }
    }
}

fn stage3b(g: &G, tid: usize) {
    let t = g.t(tid);
    g.ev_arena.release_part(tid, g.nt);
    loop {
        let i = g.next.fetch_add(1, Ordering::Relaxed);
        if i >= g.joined.len() {
            break;
        }
        let segs = &g.joined[i];
        let u = segs[0].user;
        unsafe {
            let tv = *g.utab.add(u as usize);
            let seg = |s: &Seg| std::slice::from_raw_parts(g.t(s.tid as usize).edge.as_ptr().add(s.off as usize), s.len as usize);
            if tv & COMPLEX == 0 {
                let extra: &[u64] = if tv & EXTRA != 0 { &g.extra[&u] } else { &[] };
                let first = seg(&segs[0])[0].w0;
                let mut st = t.core.begin(if !extra.is_empty() && extra[0] < first { extra[0] } else { first });
                let (mut bits, mut prev, mut sorted, mut n, mut xi) = (0u32, 0u64, true, 0u64, 0usize);
                for s in segs {
                    for r in seg(s) {
                        while xi < extra.len() && extra[xi] < r.w0 {
                            bits |= 1 << (extra[xi] & 15);
                            t.core.feed(&mut st, extra[xi]);
                            xi += 1;
                        }
                        bits |= 1 << (r.w0 & 15);
                        sorted &= r.w0 >> 33 >= prev;
                        prev = r.w0 >> 33;
                        t.core.feed(&mut st, r.w0);
                    }
                    n += s.len as u64;
                }
                while xi < extra.len() {
                    bits |= 1 << (extra[xi] & 15);
                    t.core.feed(&mut st, extra[xi]);
                    xi += 1;
                }
                let class = class_of(tv, bits);
                if class != 0 && sorted {
                    t.n_runs += 1;
                    t.n_simple += n;
                    t.n_odd += extra.len() as u64;
                    claim(g, u);
                    finish_simple(g, t, u, class, &st);
                    continue;
                }
                t.core.abort(&st);
            }
            let mut buf = std::mem::take(&mut t.runbuf);
            buf.clear();
            for s in segs {
                buf.extend_from_slice(seg(s));
            }
            process_run(g, t, u, &mut buf);
            t.runbuf = buf;
        }
    }
}

pub fn stage3(g: &mut G) {
    g.next.store(0, Ordering::Relaxed);
    {
        let gr: &G = g;
        let old = gr.nd > 16 || std::env::var("XL_OLD3").is_ok_and(|v| v == "1");
        if gr.check {
            name_selftest(gr);
        }
        run_par(gr.nt, |tid| if old { stage3a_old(gr, tid) } else { stage3a(gr, tid) });
    }
    lap("ev3a");
    let mut joined: Vec<Vec<Seg>> = Vec::new();
    let mut cur: Vec<Seg> = Vec::new();
    for b in 0..g.nb {
        let info = unsafe { *g.edges.add(b) };
        if info.n == 0 {
            continue;
        }
        if !cur.is_empty() && cur[0].user != info.first.user {
            joined.push(std::mem::take(&mut cur));
        }
        cur.push(info.first);
        if info.n == 2 {
            joined.push(std::mem::take(&mut cur));
            cur.push(info.last);
        }
    }
    if !cur.is_empty() {
        joined.push(cur);
    }
    joined.sort_by_key(|v| std::cmp::Reverse(v.iter().map(|s| s.len as u64).sum::<u64>()));
    g.joined = joined;
    lap("stitch");
    g.next.store(0, Ordering::Relaxed);
    {
        let gr: &G = g;
        run_par(gr.nt, |tid| stage3b(gr, tid));
    }
}
