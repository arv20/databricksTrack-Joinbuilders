use crate::keyrec::scramble;
use crate::sys::*;
use crate::*;
use std::cell::UnsafeCell;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

pub struct Uf {
    map: U64Map,
    parent: Vec<u32>,
    toks: Vec<u64>,
    root: Vec<u32>,
    name_at: Vec<(u32, u32)>,
    name_buf: Vec<u8>,
}
const RAWKEY: u64 = 1 << 62;

fn hbytes(b: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &c in b {
        h = (h ^ c as u64).wrapping_mul(0x100_0000_01b3);
    }
    h ^= h >> 29;
    h = h.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h ^ (h >> 32)
}

impl Uf {
    pub fn new() -> Uf {
        Uf::with_capacity(0)
    }
    pub fn with_capacity(n: usize) -> Uf {
        Uf { map: U64Map::with_capacity(n), parent: Vec::with_capacity(n), toks: Vec::with_capacity(n), root: Vec::new(), name_at: Vec::new(), name_buf: Vec::new() }
    }
    pub fn len(&self) -> usize {
        self.parent.len()
    }
    #[inline]
    fn node(&mut self, g: &G, tok: u64) -> u32 {
        let n = self.parent.len() as u32;
        let got = if tok & RAW == 0 {
            self.map.get_or_insert(tok, n)
        } else {
            let b = g.raw_bytes(tok);
            let got = self.map.get_or_insert(RAWKEY | (hbytes(b) & (RAWKEY - 1)), n);
            if got != n && g.raw_bytes(self.toks[got as usize]) != b {
                fallback("raw ID hash collision");
            }
            got
        };
        if got == n {
            self.parent.push(n);
            self.toks.push(tok);
        }
        got
    }
    #[inline]
    fn find(&mut self, mut x: u32) -> u32 {
        let mut r = x;
        while self.parent[r as usize] != r {
            r = self.parent[r as usize];
        }
        while self.parent[x as usize] != r {
            let n = self.parent[x as usize];
            self.parent[x as usize] = r;
            x = n;
        }
        r
    }
    #[inline]
    fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            if ra < rb {
                self.parent[rb as usize] = ra;
            } else {
                self.parent[ra as usize] = rb;
            }
        }
    }
    fn finish(&mut self, g: &G) {
        let n = self.parent.len();
        let mut root = Vec::with_capacity(n);
        for i in 0..n as u32 {
            root.push(self.find(i));
        }
        let mut best = vec![u32::MAX; n];
        let mut enc = Vec::with_capacity(n);
        for &t in &self.toks {
            enc.push(if t & RAW == 0 { enc36(t).0 } else { 0 });
        }
        let (mut sa, mut sb) = (Vec::new(), Vec::new());
        for i in 0..n {
            let r = root[i] as usize;
            let b = best[r];
            let smaller = if b == u32::MAX {
                true
            } else {
                let (ta, tb) = (self.toks[i], self.toks[b as usize]);
                if (ta | tb) & RAW == 0 {
                    enc[i] < enc[b as usize]
                } else {
                    g.id_str(ta, b'u', &mut sa);
                    g.id_str(tb, b'u', &mut sb);
                    sa < sb
                }
            };
            if smaller {
                best[r] = i as u32;
            }
        }
        let mut at = vec![(0u32, 0u32); n];
        let mut buf = Vec::new();
        for r in 0..n {
            if best[r] != u32::MAX {
                g.id_str(self.toks[best[r] as usize], b'u', &mut sa);
                if buf.len() + sa.len() > u32::MAX as usize {
                    fallback("names too long");
                }
                at[r] = (buf.len() as u32, sa.len() as u32);
                buf.extend_from_slice(&sa);
            }
        }
        self.root = root;
        self.name_at = at;
        self.name_buf = buf;
    }
    #[inline]
    pub fn canon_root(&self, y: u64) -> Option<u32> {
        self.map.get(y).map(|n| self.root[n as usize])
    }
    pub fn raw_root(&self, g: &G, bytes: &[u8]) -> Option<u32> {
        let n = self.map.get(RAWKEY | (hbytes(bytes) & (RAWKEY - 1)))?;
        if g.raw_bytes(self.toks[n as usize]) != bytes {
            return None;
        }
        Some(self.root[n as usize])
    }
    pub fn name(&self, root: u32) -> &[u8] {
        let (a, l) = self.name_at[root as usize];
        &self.name_buf[a as usize..(a + l) as usize]
    }
    fn part1(&mut self, g: &G) {
        for tid in 0..g.nt {
            let t = g.t(tid);
            for &(a, b) in &t.gen_edges {
                let na = self.node(g, a);
                let nb = self.node(g, b);
                self.union(na, nb);
            }
            for &tok in &t.odd_users {
                if tok & RAW != 0 {
                    self.node(g, tok);
                }
            }
            for c in &t.odd_clicks {
                if c[0] & RAW != 0 {
                    self.node(g, c[0]);
                }
            }
        }
    }
    fn cx(&mut self, g: &G, edges: &[(u32, u8, u8)]) {
        for &(u, a, b) in edges {
            let na = self.node(g, scramble((u as u64) << 9 | a as u64, g.ukey));
            let nb = self.node(g, scramble((u as u64) << 9 | b as u64, g.ukey));
            self.union(na, nb);
        }
    }
    fn part2a(&mut self, g: &G) {
        for tid in 0..g.nt {
            self.cx(g, &g.t(tid).cx_edges);
        }
    }
    fn part2b(&mut self, g: &G, heavy: &[Heavy]) {
        for h in heavy {
            self.cx(g, unsafe { &*h.cx.get() });
        }
        self.finish(g);
    }
}

#[inline]
pub fn resolve_normal(g: &G, u: u32, a: u32) -> u64 {
    let tv = unsafe { *g.utab.add(u as usize) };
    if tv & COMPLEX != 0 {
        let y = scramble((u as u64) << 9 | a as u64, g.ukey);
        match g.uf.canon_root(y) {
            Some(r) => TAG_UF | r as u64,
            None => (u as u64) << 4 | a as u64,
        }
    } else if ((tv & MASK) >> a) & 1 != 0 {
        (u as u64) << 4 | 15
    } else {
        (u as u64) << 4 | a as u64
    }
}

pub fn resolve_token(g: &G, tok: u64) -> u64 {
    if tok & RAW == 0 {
        match g.uf.canon_root(tok) {
            Some(r) => TAG_UF | r as u64,
            None => TAG_ODDC | tok,
        }
    } else {
        match g.uf.raw_root(g, g.raw_bytes(tok)) {
            Some(r) => TAG_UF | r as u64,
            None => fallback("raw user ID was not interned"),
        }
    }
}

pub fn name_of(g: &G, pkey: u64, out: &mut Vec<u8>) {
    out.clear();
    match pkey >> 62 {
        0 => {
            let u = (pkey >> 4) as u32;
            let code = (pkey & 15) as u32;
            let class = if code == 15 { (unsafe { *g.utab.add(u as usize) } & MASK) as u32 } else { 1 << code };
            let (v, len) = crate::core::simple_name(g, u, class);
            out.push(b'u');
            out.extend_from_slice(&v.to_be_bytes()[..len]);
        }
        1 => out.extend_from_slice(g.uf.name((pkey & 0xFFFF_FFFF) as u32)),
        _ => g.id_str(pkey & (RAW - 1), b'u', out),
    }
}

#[derive(Default)]
pub struct Scratch {
    tmp: Vec<[u64; 2]>,
    k1: Vec<u64>,
    k2: Vec<u64>,
}

const RANK_MAX: usize = env_u32(option_env!("XL_RANK_MAX"), 64) as usize;
const RADIX_MIN: usize = env_u32(option_env!("XL_RADIX_MIN"), 512) as usize;

#[inline(always)]
fn sort_clicks(g: &G, s: &mut [[u64; 2]], sc: &mut Scratch) {
    let n = s.len();
    if n < 2 {
        return;
    }
    if n <= RANK_MAX {
        unsafe { rank_sort(s) };
    } else {
        let k = &mut sc.k1;
        k.clear();
        k.extend(s.iter().enumerate().map(|(i, c)| c[0] >> 32 << 32 | i as u64));
        if n <= RADIX_MIN {
            k.sort_unstable();
        } else {
            radix_keys(k, &mut sc.k2);
        }
        let tmp = &mut sc.tmp;
        tmp.clear();
        tmp.extend(k.iter().map(|&x| unsafe { *s.get_unchecked((x & 0xFFFF_FFFF) as usize) }));
        s.copy_from_slice(tmp);
    }
    let mut i = 1;
    while i < n {
        let x = s[i][0] ^ s[i - 1][0];
        if x >> 32 == 0 && x >> 4 != 0 {
            let ts = s[i][0] >> 32;
            if (i < 2 || s[i - 2][0] >> 32 != ts) && (i + 1 >= n || s[i + 1][0] >> 32 != ts) && (s[i - 1][1] | s[i][1]) & RAW == 0 {
                let (e0, e1) = (enc36(s[i - 1][1]).0, enc36(s[i][1]).0);
                if e0 < e1 || (e0 == e1 && s[i - 1][0] > s[i][0]) {
                    s.swap(i - 1, i);
                }
                i += 1;
            } else {
                i = tie_group(g, s, i);
            }
        } else {
            i += 1;
        }
    }
}

#[inline(never)]
fn radix_keys(k: &mut Vec<u64>, k2: &mut Vec<u64>) {
    let n = k.len();
    let mut h = [[0u32; 2048]; 3];
    for &x in k.iter() {
        let t = (x >> 32) as usize;
        h[0][t & 2047] += 1;
        h[1][(t >> 11) & 2047] += 1;
        h[2][(t >> 22) & 2047] += 1;
    }
    k2.clear();
    k2.resize(n, 0);
    for p in 0..3 {
        let sh = 32 + 11 * p;
        let hp = &mut h[p];
        if hp[((k[0] >> sh) & 2047) as usize] as usize == n {
            continue;
        }
        let mut at = 0u32;
        for x in hp.iter_mut() {
            let c = *x;
            *x = at;
            at += c;
        }
        for &x in k.iter() {
            let d = ((x >> sh) & 2047) as usize;
            unsafe { *k2.get_unchecked_mut(hp[d] as usize) = x };
            hp[d] += 1;
        }
        std::mem::swap(k, k2);
    }
}

#[inline(always)]
unsafe fn rank_sort(s: &mut [[u64; 2]]) {
    let n = s.len();
    if n == 2 {
        if s[1][0] < s[0][0] {
            s.swap(0, 1);
        }
        return;
    }
    match (n + 7) / 8 {
        1 => rank_nv::<1>(s),
        2 => rank_nv::<2>(s),
        3 => rank_nv::<3>(s),
        4 => rank_nv::<4>(s),
        5 => rank_nv::<5>(s),
        6 => rank_nv::<6>(s),
        7 => rank_nv::<7>(s),
        _ => rank_nv::<8>(s),
    }
}

#[inline(always)]
unsafe fn rank_nv<const NV: usize>(s: &mut [[u64; 2]]) {
    use std::arch::x86_64::*;
    let n = s.len();
    let p = s.as_ptr() as *const u64;
    let even = _mm512_setr_epi64(0, 2, 4, 6, 8, 10, 12, 14);
    let odd = _mm512_setr_epi64(1, 3, 5, 7, 9, 11, 13, 15);
    let max = _mm512_set1_epi64(-1);
    let lane = _mm512_setr_epi64(0, 1, 2, 3, 4, 5, 6, 7);
    let seven = _mm512_set1_epi64(7);
    let one = _mm512_set1_epi64(1);
    let nv = NV;
    let mut kv = [max; NV];
    let mut vv = [max; NV];
    let mut kmask = [0u8; NV];
    for b in 0..nv {
        let left = n - 8 * b;
        let m = if left >= 8 { 0xFFFF_u32 } else { (1u32 << (2 * left)) - 1 };
        let lo = _mm512_maskz_loadu_epi64(m as u8, p.add(16 * b) as *const _);
        let hi = _mm512_maskz_loadu_epi64((m >> 8) as u8, p.add(16 * b + 8) as *const _);
        let km = if left >= 8 { 0xFF } else { ((1u32 << left) - 1) as u8 };
        kmask[b] = km;
        kv[b] = _mm512_mask_mov_epi64(max, km, _mm512_permutex2var_epi64(lo, even, hi));
        vv[b] = _mm512_permutex2var_epi64(lo, odd, hi);
    }
    let mut r = [_mm512_setzero_si512(); NV];
    for d in 0..8i64 {
        let idx = _mm512_and_si512(_mm512_add_epi64(lane, _mm512_set1_epi64(d)), seven);
        for b in 0..nv {
            let kr = _mm512_permutexvar_epi64(idx, kv[b]);
            for a in 0..nv {
                let ka = kv[a];
                let m = if a == b {
                    if d == 0 {
                        continue;
                    }
                    _mm512_cmplt_epu64_mask(kr, ka) | (_mm512_cmpeq_epu64_mask(kr, ka) & (0xFFu8 << (8 - d)))
                } else if b > a {
                    _mm512_cmplt_epu64_mask(kr, ka)
                } else {
                    _mm512_cmple_epu64_mask(kr, ka)
                };
                r[a] = _mm512_mask_add_epi64(r[a], m, r[a], one);
            }
        }
    }
    let q = s.as_mut_ptr() as *mut u64;
    for a in 0..nv {
        let r2 = _mm512_slli_epi64(r[a], 1);
        _mm512_mask_i64scatter_epi64(q as *mut _, kmask[a], r2, kv[a], 8);
        _mm512_mask_i64scatter_epi64(q.add(1) as *mut _, kmask[a], r2, vv[a], 8);
    }
}

#[cold]
#[inline(never)]
fn tie_group(g: &G, s: &mut [[u64; 2]], i: usize) -> usize {
    let ts = s[i][0] >> 32;
    let mut a = i - 1;
    while a > 0 && s[a - 1][0] >> 32 == ts {
        a -= 1;
    }
    let mut b = i + 1;
    while b < s.len() && s[b][0] >> 32 == ts {
        b += 1;
    }
    let grp = &mut s[a..b];
    if grp.iter().all(|c| c[1] & RAW == 0) {
        if grp.len() <= 16 {
            let mut e = [0u64; 16];
            for (k, c) in grp.iter().enumerate() {
                e[k] = enc36(c[1]).0;
            }
            for x in 1..grp.len() {
                let (ex, cx) = (e[x], grp[x]);
                let mut y = x;
                while y > 0 && (e[y - 1] < ex || (e[y - 1] == ex && grp[y - 1][0] > cx[0])) {
                    e[y] = e[y - 1];
                    grp[y] = grp[y - 1];
                    y -= 1;
                }
                e[y] = ex;
                grp[y] = cx;
            }
        } else {
            let mut v: Vec<(u64, [u64; 2])> = grp.iter().map(|c| (enc36(c[1]).0, *c)).collect();
            v.sort_unstable_by(|x, y| y.0.cmp(&x.0).then_with(|| x.1[0].cmp(&y.1[0])));
            for (d, x) in grp.iter_mut().zip(&v) {
                *d = x.1;
            }
        }
    } else {
        grp.sort_by(|x, y| crate::events::cid_cmp(g, y[1], x[1]).then_with(|| x[0].cmp(&y[0])));
    }
    b
}

struct Heavy {
    ub: usize,
    cnt: Vec<UnsafeCell<Vec<u32>>>,
    links: Vec<UnsafeCell<Vec<u64>>>,
    cx: UnsafeCell<Vec<(u32, u8, u8)>>,
}
const HEAVY_DIV: usize = env_u32(option_env!("XL_HEAVY_DIV"), 32) as usize;
pub static DBG: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
struct Plan {
    heavy: Vec<Heavy>,
    normal: Vec<u32>,
    uf: UnsafeCell<Uf>,
}
unsafe impl Sync for Plan {}

#[inline(always)]
fn link_rec(t: &mut TState, utab: &mut [u16], w0: u64) {
    let ul = ((w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
    let bits = (1u16 << ((w0 >> 4) & 15)) | (1u16 << (w0 & 15));
    let m = utab[ul] & MASK;
    if m == 0 || m & bits != 0 {
        utab[ul] |= bits;
    } else {
        t.pend.push((ul as u32, bits));
    }
}

fn links_done(t: &mut TState, utab: &mut [u16], u0: usize) -> bool {
    if !t.pend.is_empty() {
        loop {
            let mut changed = false;
            for &(ul, bits) in &t.pend {
                let m = utab[ul as usize] & MASK;
                if m & bits != 0 && m | bits != m {
                    utab[ul as usize] |= bits;
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }
        for &(ul, bits) in &t.pend {
            if utab[ul as usize] & bits == 0 {
                utab[ul as usize] |= COMPLEX;
            }
        }
    }
    let mut anyc = false;
    for (ul, &v) in utab.iter().enumerate() {
        if v & COMPLEX != 0 {
            t.cx_users.push((u0 + ul) as u32);
            anyc = true;
        }
    }
    anyc
}

#[inline(always)]
fn utab_of(g: &G, ub: usize) -> &mut [u16] {
    unsafe { std::slice::from_raw_parts_mut(g.utab.add(ub << UB), UBSIZE) }
}
#[inline(always)]
fn off_of(g: &G, ub: usize) -> &mut [u32] {
    unsafe { std::slice::from_raw_parts_mut(g.csr_off.add((ub << UB) + ub), UBSIZE + 1) }
}

fn bucket(g: &G, t: &mut TState, tmp: &mut Scratch, ub: usize) {
    let tstart = std::time::Instant::now();
    let u0 = ub << UB;
    let utab = utab_of(g, ub);
    let mut cnt = std::mem::take(&mut t.cnt);
    cnt.clear();
    cnt.resize(UBSIZE + 1, 0);
    t.pend.clear();
    let mut any = false;
    for th in 0..g.nt {
        g.t(th).us.for_each(ub, |recs| {
            any = true;
            for r in recs {
                if r.w0 >> 63 == 0 {
                    cnt[((r.w0 >> 43) & (UBSIZE as u64 - 1)) as usize] += 1;
                } else {
                    link_rec(t, utab, r.w0);
                }
            }
        });
    }
    let dbg1 = tstart.elapsed().as_nanos() as u64;
    t.prof[5] += dbg1;
    let anyc = links_done(t, utab, u0);
    if any {
        let off = off_of(g, ub);
        let mut at = g.csr_base[ub] as u32;
        for ul in 0..UBSIZE {
            off[ul] = at;
            at += cnt[ul];
            cnt[ul] = off[ul];
        }
        off[UBSIZE] = at;
        for th in 0..g.nt {
            g.t(th).us.for_each(ub, |recs| {
                for r in recs {
                    let ul = ((r.w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
                    if r.w0 >> 63 == 0 {
                        unsafe { *g.csr.add(cnt[ul] as usize) = [r.w1, r.w0 & (RAW | (RAW - 1))] };
                        cnt[ul] += 1;
                    } else if anyc && utab[ul] & COMPLEX != 0 {
                        t.cx_edges.push(((u0 + ul) as u32, ((r.w0 >> 4) & 15) as u8, (r.w0 & 15) as u8));
                    }
                }
            });
        }
        let dbg2 = tstart.elapsed().as_nanos() as u64;
        t.prof[6] += dbg2;
        for ul in 0..UBSIZE {
            let (a, b) = (off[ul] as usize, off[ul + 1] as usize);
            if b - a > 1 {
                sort_clicks(g, unsafe { std::slice::from_raw_parts_mut(g.csr.add(a), b - a) }, tmp);
            }
        }
    }
    t.cnt = cnt;
    let el = tstart.elapsed().as_nanos() as u64;
    t.prof[3] += el;
    t.prof[4] = t.prof[4].max(el);
}

fn heavy_count(g: &G, h: &Heavy, th: usize) {
    let cnt = unsafe { &mut *h.cnt[th].get() };
    let lk = unsafe { &mut *h.links[th].get() };
    cnt.clear();
    cnt.resize(UBSIZE, 0);
    g.t(th).us.for_each(h.ub, |recs| {
        for r in recs {
            if r.w0 >> 63 == 0 {
                cnt[((r.w0 >> 43) & (UBSIZE as u64 - 1)) as usize] += 1;
            } else {
                lk.push(r.w0);
            }
        }
    });
}

fn heavy_links(g: &G, t: &mut TState, h: &Heavy) {
    let ub = h.ub;
    let u0 = ub << UB;
    let utab = utab_of(g, ub);
    t.pend.clear();
    for th in 0..g.nt {
        for &w0 in unsafe { &*h.links[th].get() } {
            link_rec(t, utab, w0);
        }
    }
    if links_done(t, utab, u0) {
        let cx = unsafe { &mut *h.cx.get() };
        for th in 0..g.nt {
            for &w0 in unsafe { &*h.links[th].get() } {
                let ul = ((w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
                if utab[ul] & COMPLEX != 0 {
                    cx.push(((u0 + ul) as u32, ((w0 >> 4) & 15) as u8, (w0 & 15) as u8));
                }
            }
        }
    }
    let off = off_of(g, ub);
    let mut at = g.csr_base[ub] as u32;
    for ul in 0..UBSIZE {
        off[ul] = at;
        for th in 0..g.nt {
            at += unsafe { &*h.cnt[th].get() }[ul];
        }
    }
    off[UBSIZE] = at;
}

fn heavy_scatter(g: &G, t: &mut TState, h: &Heavy, th: usize) {
    let mut pos = std::mem::take(&mut t.cnt);
    pos.clear();
    pos.resize(UBSIZE + 1, 0);
    let mut at = g.csr_base[h.ub] as u32;
    for ul in 0..UBSIZE {
        for t2 in 0..g.nt {
            if t2 == th {
                pos[ul] = at;
            }
            at += unsafe { &*h.cnt[t2].get() }[ul];
        }
    }
    g.t(th).us.for_each(h.ub, |recs| {
        for r in recs {
            if r.w0 >> 63 == 0 {
                let ul = ((r.w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
                unsafe { *g.csr.add(pos[ul] as usize) = [r.w1, r.w0 & (RAW | (RAW - 1))] };
                pos[ul] += 1;
            }
        }
    });
    t.cnt = pos;
}

fn heavy_sort(g: &G, p: &Plan, tid: usize) {
    g.us_arena.release_part(tid, g.nt);
    let mut tmp = Scratch::default();
    let per = 64 + UBSIZE / 64 - 1;
    let units = per * p.heavy.len();
    loop {
        let i = g.next.fetch_add(1, Ordering::Relaxed);
        if i == 0 {
            let c0 = thread_cpu_ms();
            let uf = unsafe { &mut *p.uf.get() };
            uf.part2b(g, &p.heavy);
            if DBG.load(Ordering::Relaxed) {
                eprintln!("[dbg] uf part2 cpu {:.2} ms, nodes {}", thread_cpu_ms() - c0, uf.len());
            }
            continue;
        }
        let i = i - 1;
        if i >= units {
            break;
        }
        let (h, j) = (i / per, i % per);
        let u0 = p.heavy[h].ub << UB;
        let (a, b) = if j < 64 { (j, j + 1) } else { ((j - 63) * 64, (j - 62) * 64) };
        for u in u0 + a..u0 + b {
            let s = g.csr_range(u as u32);
            if s.len() > 1 {
                sort_clicks(g, unsafe { std::slice::from_raw_parts_mut(s.as_ptr() as *mut [u64; 2], s.len()) }, &mut tmp);
            }
        }
    }
}

pub fn stage2(g: &mut G) {
    let mark = |g: &G, tok: u64| {
        if tok & RAW == 0 {
            if let Some((u, _)) = g.normal(tok) {
                unsafe { *g.utab.add(u as usize) |= COMPLEX };
            }
        }
    };
    let mut n_odd_ev = 0u64;
    let mut n_ev = 0u64;
    for tid in 0..g.nt {
        let t = g.t(tid);
        n_odd_ev += t.odd_events.len() as u64;
        n_ev += t.n_ev_rows;
        for &(a, b) in &t.gen_edges {
            mark(g, a);
            mark(g, b);
        }
    }
    if n_odd_ev * 50 > n_ev + 5000 {
        fallback("more than 2% odd events");
    }
    let mut m: HashMap<Vec<u8>, OddEv> = HashMap::new();
    let mut s = Vec::new();
    for tid in 0..g.nt {
        for oe in &g.t(tid).odd_events {
            g.id_str(oe.eid, b'e', &mut s);
            match m.get_mut(&s) {
                Some(old) => {
                    if oe.ing < old.ing {
                        *old = *oe;
                    }
                }
                None => {
                    m.insert(s.clone(), *oe);
                }
            }
        }
    }
    let mut extra: HashMap<u32, Vec<u64>> = HashMap::new();
    let mut odd_kept = Vec::new();
    for oe in m.values() {
        if oe.w0 >> 33 == INVALID_TS {
            continue;
        }
        if oe.user & ODDU != 0 {
            odd_kept.push(*oe);
        } else {
            unsafe { *g.utab.add(oe.user as usize) |= EXTRA };
            extra.entry(oe.user as u32).or_default().push(oe.w0);
        }
    }
    for v in extra.values_mut() {
        v.sort_unstable();
    }
    g.extra = extra;
    g.odd_kept = odd_kept;
    let mut base = Vec::with_capacity(g.nub + 1);
    let mut at = 0usize;
    for ub in 0..g.nub {
        base.push(at);
        for tid in 0..g.nt {
            at += g.t(tid).us.count(ub);
        }
    }
    if at >= u32::MAX as usize {
        fallback("too many clicks");
    }
    g.csr_base = base;
    g.ncsr = at;
    g.csr = anon((at + 16) * 16) as *mut [u64; 2];
    lap("st2a");
    let total = at;
    let nt = g.nt;
    let mut plan = Plan { heavy: Vec::new(), normal: Vec::new(), uf: UnsafeCell::new(Uf::new()) };
    for ub in 0..g.nub {
        let n = if ub + 1 < g.nub { g.csr_base[ub + 1] } else { total } - g.csr_base[ub];
        if n > (total / HEAVY_DIV).max(1 << 17) {
            plan.heavy.push(Heavy { ub, cnt: (0..nt).map(|_| UnsafeCell::new(Vec::new())).collect(), links: (0..nt).map(|_| UnsafeCell::new(Vec::new())).collect(), cx: UnsafeCell::new(Vec::new()) });
        } else {
            plan.normal.push(ub as u32);
        }
    }
    let nh = plan.heavy.len();
    DBG.store(std::env::var("XL_DBG").is_ok(), Ordering::Relaxed);
    if DBG.load(Ordering::Relaxed) {
        eprintln!("[dbg] heavy buckets {:?} total {total}", plan.heavy.iter().map(|h| (h.ub, if h.ub + 1 < g.nub { g.csr_base[h.ub + 1] } else { total } - g.csr_base[h.ub])).collect::<Vec<_>>());
    }
    g.next.store(0, Ordering::Relaxed);
    {
        let gr: &G = g;
        let p = &plan;
        run_par(nt, |tid| {
            let t = gr.t(tid);
            let mut tmp = Scratch::default();
            loop {
                let i = gr.next.fetch_add(1, Ordering::Relaxed);
                if i == 0 {
                    let c0 = thread_cpu_ms();
                    let uf = unsafe { &mut *p.uf.get() };
                    let mut n = 0;
                    for th in 0..gr.nt {
                        let t = gr.t(th);
                        n += 2 * t.gen_edges.len() + t.odd_users.len() / 8 + t.odd_clicks.len() / 8;
                    }
                    *uf = Uf::with_capacity(n + 1024);
                    uf.part1(gr);
                    if DBG.load(Ordering::Relaxed) {
                        eprintln!("[dbg] uf part1 cpu {:.2} ms, nodes {}", thread_cpu_ms() - c0, uf.len());
                    }
                } else if i <= nh * nt {
                    heavy_count(gr, &p.heavy[(i - 1) / nt], (i - 1) % nt);
                } else if i <= nh * nt + p.normal.len() {
                    bucket(gr, t, &mut tmp, p.normal[i - 1 - nh * nt] as usize);
                } else {
                    break;
                }
            }
        });
    }
    lap("st2b");
    g.next.store(0, Ordering::Relaxed);
    if nh > 0 {
        let gr: &G = g;
        let p = &plan;
        run_par(nt, |tid| {
            let t = gr.t(tid);
            loop {
                let i = gr.next.fetch_add(1, Ordering::Relaxed);
                if i == 0 {
                    unsafe { &mut *p.uf.get() }.part2a(gr);
                    continue;
                }
                let i = i - 1;
                if i < nh {
                    heavy_links(gr, t, &p.heavy[i]);
                } else if i < nh + nh * nt {
                    let j = i - nh;
                    heavy_scatter(gr, t, &p.heavy[j / nt], j % nt);
                } else {
                    break;
                }
            }
        });
    }
    if nh == 0 {
        unsafe { &mut *plan.uf.get() }.part2a(g);
    }
    lap("st2h");
    g.next.store(0, Ordering::Relaxed);
    {
        let gr: &G = g;
        let p = &plan;
        run_par(gr.nt, |tid| heavy_sort(gr, p, tid));
    }
    g.uf = plan.uf.into_inner();
}

pub fn sort_bench(g: &G) {
    let mut sc = Scratch::default();
    let mut x = 0x1234_5678_9abc_def0u64;
    let mut rnd = || {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        x
    };
    for &n in &[2usize, 3, 4, 5, 6, 7, 8, 9, 12, 15, 16, 17, 24, 31, 32, 33, 40, 48, 63, 64, 128, 300, 600] {
        let reps = 400_000 / n;
        let mut data: Vec<[u64; 2]> = (0..reps * n).map(|_| { let ts = rnd() % 1_000_000_000; let c = rnd() % 2000; [ts << 32 | c << 4 | (rnd() % 3), rnd() % (1 << 40)] }).collect();
        for rep in 0..2000 {
            let mut a: Vec<[u64; 2]> = (0..n).map(|_| { let w = if rep % 2 == 0 { rnd() % 4 } else { rnd() % 1000 }; [w << 32 | (rnd() % 3) << 4, rnd()] }).collect();
            let mut b = a.clone();
            b.sort_by_key(|c| c[0]);
            if n <= 64 {
                unsafe { rank_sort(&mut a) };
                if a != b {
                    eprintln!("[sortbench] RANK_SORT WRONG n={n}");
                    unsafe { crate::sys::_exit(9) };
                }
            }
        }
        let t0 = unsafe { std::arch::x86_64::_rdtsc() };
        let mut acc = 0u64;
        for r in 0..reps {
            acc = acc.wrapping_add(data[r * n][0] ^ data[r * n + n - 1][1]);
        }
        let base = unsafe { std::arch::x86_64::_rdtsc() } - t0;
        std::hint::black_box(acc);
        let t0 = unsafe { std::arch::x86_64::_rdtsc() };
        for r in 0..reps {
            sort_clicks(g, &mut data[r * n..(r + 1) * n], &mut sc);
        }
        let el = unsafe { std::arch::x86_64::_rdtsc() } - t0;
        eprintln!("[sortbench] n={n}: base {:.0} cycles/call", base as f64 / reps as f64);
        let t0 = unsafe { std::arch::x86_64::_rdtsc() };
        for r in 0..reps {
            unsafe { rank_sort(&mut data[r * n..(r + 1) * n]) };
        }
        let el2 = unsafe { std::arch::x86_64::_rdtsc() } - t0;
        eprintln!("[sortbench] n={n}: rank_sort only (sorted input) {:.0} cycles/call", el2 as f64 / reps as f64);
        eprintln!("[sortbench] n={n}: {:.0} cycles/call, {:.1} per click", el as f64 / reps as f64, el as f64 / (reps * n) as f64);
    }
}
