use crate::keyrec::scramble;
use crate::sys::*;
use crate::*;
use std::collections::HashMap;
use std::sync::atomic::Ordering;

pub struct Uf {
    canon: U64Map,
    raw: HashMap<Vec<u8>, u32>,
    parent: Vec<u32>,
    toks: Vec<u64>,
    root: Vec<u32>,
    names: Vec<Vec<u8>>,
}
impl Uf {
    pub fn new() -> Uf {
        Uf { canon: U64Map::new(), raw: HashMap::new(), parent: Vec::new(), toks: Vec::new(), root: Vec::new(), names: Vec::new() }
    }
    pub fn len(&self) -> usize {
        self.parent.len()
    }
    fn node(&mut self, g: &G, tok: u64) -> u32 {
        let n = self.parent.len() as u32;
        let got = if tok & RAW == 0 {
            self.canon.get_or_insert(tok, n)
        } else {
            *self.raw.entry(g.raw_bytes(tok).to_vec()).or_insert(n)
        };
        if got == n {
            self.parent.push(n);
            self.toks.push(tok);
        }
        got
    }
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
        self.root = (0..n as u32).map(|i| self.find(i)).collect();
        self.names = vec![Vec::new(); n];
        let mut s = Vec::new();
        for i in 0..n {
            g.id_str(self.toks[i], b'u', &mut s);
            let r = self.root[i] as usize;
            if self.names[r].is_empty() || s < self.names[r] {
                self.names[r] = s.clone();
            }
        }
    }
    #[inline]
    pub fn canon_root(&self, y: u64) -> Option<u32> {
        self.canon.get(y).map(|n| self.root[n as usize])
    }
    pub fn raw_root(&self, bytes: &[u8]) -> Option<u32> {
        self.raw.get(bytes).map(|&n| self.root[n as usize])
    }
    pub fn name(&self, root: u32) -> &[u8] {
        &self.names[root as usize]
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
        match g.uf.raw_root(g.raw_bytes(tok)) {
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

#[inline(always)]
fn sort_clicks(g: &G, s: &mut [[u64; 2]], tmp: &mut Vec<[u64; 2]>) {
    let n = s.len();
    if n < 2 {
        return;
    }
    if n <= 16 {
        unsafe { rank_sort(s) };
    } else if n <= 2048 {
        let mut keys = [0u64; 2048];
        for (i, c) in s.iter().enumerate() {
            keys[i] = c[0] >> 32 << 32 | i as u64;
        }
        keys[..n].sort_unstable();
        tmp.clear();
        tmp.extend(keys[..n].iter().map(|k| s[(k & 0xFFFF_FFFF) as usize]));
        s.copy_from_slice(tmp);
    } else {
        radix_ts(s, tmp);
    }
    let mut i = 1;
    while i < n {
        let x = s[i][0] ^ s[i - 1][0];
        if x >> 32 == 0 && x >> 4 != 0 {
            i = tie_group(g, s, i);
        } else {
            i += 1;
        }
    }
}

#[inline(always)]
unsafe fn rank_sort(s: &mut [[u64; 2]]) {
    use std::arch::x86_64::*;
    let n = s.len();
    let mut keys = [u64::MAX; 16];
    for i in 0..n {
        keys[i] = s[i][0];
    }
    let mut out = [[0u64; 2]; 16];
    let k0 = _mm512_loadu_si512(keys.as_ptr() as *const _);
    if n <= 8 {
        for i in 0..n {
            let x = _mm512_set1_epi64(keys[i] as i64);
            let lt = _mm512_cmplt_epu64_mask(k0, x) as u32;
            let eq = _mm512_cmpeq_epu64_mask(k0, x) as u32;
            let r = lt.count_ones() + (eq & ((1u32 << i) - 1)).count_ones();
            *out.get_unchecked_mut(r as usize) = *s.get_unchecked(i);
        }
    } else {
        let k1 = _mm512_loadu_si512(keys.as_ptr().add(8) as *const _);
        for i in 0..n {
            let x = _mm512_set1_epi64(keys[i] as i64);
            let lt = _mm512_cmplt_epu64_mask(k0, x) as u32 | (_mm512_cmplt_epu64_mask(k1, x) as u32) << 8;
            let eq = _mm512_cmpeq_epu64_mask(k0, x) as u32 | (_mm512_cmpeq_epu64_mask(k1, x) as u32) << 8;
            let r = lt.count_ones() + (eq & ((1u32 << i) - 1)).count_ones();
            *out.get_unchecked_mut(r as usize) = *s.get_unchecked(i);
        }
    }
    s.copy_from_slice(&out[..n]);
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

#[inline(never)]
fn radix_ts(s: &mut [[u64; 2]], tmp: &mut Vec<[u64; 2]>) {
    let n = s.len();
    tmp.clear();
    tmp.resize(n, [0; 2]);
    let mut h0 = vec![0u32; 65536];
    let mut h1 = vec![0u32; 65536];
    for c in s.iter() {
        h0[((c[0] >> 32) & 0xFFFF) as usize] += 1;
        h1[(c[0] >> 48) as usize] += 1;
    }
    for h in [&mut h0, &mut h1] {
        let mut at = 0u32;
        for x in h.iter_mut() {
            let c = *x;
            *x = at;
            at += c;
        }
    }
    for c in s.iter() {
        let d = ((c[0] >> 32) & 0xFFFF) as usize;
        tmp[h0[d] as usize] = *c;
        h0[d] += 1;
    }
    for c in tmp.iter() {
        let d = (c[0] >> 48) as usize;
        s[h1[d] as usize] = *c;
        h1[d] += 1;
    }
}

fn stage2c(g: &G, tid: usize) {
    g.us_arena.release_part(tid, g.nt);
    let mut tmp: Vec<[u64; 2]> = Vec::new();
    let units = 64 + g.ucap as usize / 64 - 1;
    loop {
        let i = g.next.fetch_add(1, Ordering::Relaxed);
        if i >= units {
            break;
        }
        let (u0, u1) = if i < 64 { (i, i + 1) } else { ((i - 63) * 64, (i - 62) * 64) };
        for u in u0..u1 {
            let s = g.csr_range(u as u32);
            if s.len() > 1 {
                sort_clicks(g, unsafe { std::slice::from_raw_parts_mut(s.as_ptr() as *mut [u64; 2], s.len()) }, &mut tmp);
            }
        }
    }
}

fn stage2b(g: &G, tid: usize) {
    let t = g.t(tid);
    t.cnt.clear();
    t.cnt.resize(UBSIZE + 1, 0);
    loop {
        let ub = g.next.fetch_add(1, Ordering::Relaxed);
        if ub >= g.nub {
            break;
        }
        let tstart = std::time::Instant::now();
        let u0 = ub << UB;
        let utab = unsafe { std::slice::from_raw_parts_mut(g.utab.add(u0), UBSIZE) };
        let cnt = &mut t.cnt[..];
        cnt.fill(0);
        t.pend.clear();
        let mut any = false;
        for th in 0..g.nt {
            g.t(th).us.for_each(ub, |recs| {
                any = true;
                for r in recs {
                    let ul = ((r.w0 >> 43) & (UBSIZE as u64 - 1)) as usize;
                    if r.w0 >> 63 == 0 {
                        cnt[ul] += 1;
                    } else {
                        let bits = (1u16 << ((r.w0 >> 4) & 15)) | (1u16 << (r.w0 & 15));
                        let m = utab[ul] & MASK;
                        if m == 0 || m & bits != 0 {
                            utab[ul] |= bits;
                        } else {
                            t.pend.push((ul as u32, bits));
                        }
                    }
                }
            });
        }
        let dbg1 = tstart.elapsed().as_nanos() as u64;
        t.prof[5] += dbg1;
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
        if !any {
            continue;
        }
        let off = unsafe { std::slice::from_raw_parts_mut(g.csr_off.add(u0 + ub), UBSIZE + 1) };
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
        let el = tstart.elapsed().as_nanos() as u64;
        if el > 4_000_000 && std::env::var("XL_DBG").is_ok() {
            eprintln!("[v2b] slow bucket {ub}: {:.1} ms, clicks={} pend={} (pass1 end {:.1}, pass2 end {:.1})", el as f64 / 1e6, off[UBSIZE] - off[0], t.pend.len(), dbg1 as f64 / 1e6, dbg2 as f64 / 1e6);
        }
        t.prof[3] += el;
        t.prof[4] = t.prof[4].max(el);
    }
}

struct SendPtr(*const G);
unsafe impl Send for SendPtr {}

pub fn stage2(g: &mut G) -> std::thread::JoinHandle<Uf> {
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
    g.csr = anon((at + 16) * 16) as *mut [u64; 2];
    lap("st2a");
    let gp = SendPtr(g as *const G);
    let handle = std::thread::spawn(move || {
        let gp = gp;
        let g: &G = unsafe { &*gp.0 };
        let mut uf = Uf::new();
        for tid in 0..g.nt {
            let t = g.t(tid);
            for &(a, b) in &t.gen_edges {
                let na = uf.node(g, a);
                let nb = uf.node(g, b);
                uf.union(na, nb);
            }
            for &tok in &t.odd_users {
                if tok & RAW != 0 {
                    uf.node(g, tok);
                }
            }
            for c in &t.odd_clicks {
                if c[0] & RAW != 0 {
                    uf.node(g, c[0]);
                }
            }
        }
        while !g.st2b_done.load(Ordering::Acquire) {
            std::thread::yield_now();
        }
        for tid in 0..g.nt {
            let t = g.t(tid);
            for &(u, a, b) in &t.cx_edges {
                let na = uf.node(g, scramble((u as u64) << 9 | a as u64, g.ukey));
                let nb = uf.node(g, scramble((u as u64) << 9 | b as u64, g.ukey));
                uf.union(na, nb);
            }
        }
        uf.finish(g);
        uf
    });
    g.next.store(0, Ordering::Relaxed);
    {
        let gr: &G = g;
        run_par(gr.nt, |tid| stage2b(gr, tid));
    }
    g.st2b_done.store(true, Ordering::Release);
    lap("st2b");
    g.next.store(0, Ordering::Relaxed);
    {
        let gr: &G = g;
        run_par(gr.nt, |tid| stage2c(gr, tid));
    }
    handle
}
