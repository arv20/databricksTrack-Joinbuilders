use crate::keyrec::scramble;
use crate::sys::*;
use crate::*;

#[derive(Clone, Copy)]
struct Sess {
    start: u64,
    day: u32,
    rev: u64,
}

#[derive(Clone, Copy)]
pub struct St {
    prev: u64,
    s_start: u64,
    s_day: u32,
    s_rev: u64,
    d_s: u32,
    d_e: u32,
    d_rev: u64,
    catmask: u32,
    mark: usize,
    pidx: u32,
}

pub enum Name<'a> {
    Simple(u32, u32),
    Bytes(&'a [u8]),
}

pub struct Core {
    ws_mod: u64,
    day0: i32,
    pub nd: usize,
    pub organic: usize,
    pub agg: Vec<[u32; 2]>,
    pub aggp: Vec<[u64; 2]>,
    pub names_off: AVec<u32>,
    pub names: AVec<u8>,
    pub keys: AVec<u32>,
    pub day: AVec<i32>,
    pub ses: AVec<i32>,
    pub evs: AVec<i32>,
    pub rev: AVec<i32>,
    pub cat: AVec<u8>,
    pub n_sess: u64,
    sess: Vec<Sess>,
    stamp: Vec<u32>,
    serial: u32,
    catsum: [u64; 32],
    ccnt: Vec<u32>,
    cts: Vec<u64>,
    dstart: Vec<u32>,
}

#[inline]
pub fn simple_name(g: &G, u: u32, class: u32) -> (u64, usize) {
    simple_name_key(g.ukey, u, class)
}

#[inline]
pub fn simple_name_key(ukey: u64, u: u32, class: u32) -> (u64, usize) {
    let mut m = class;
    let mut best = (u64::MAX, 0usize);
    while m != 0 {
        let a = m.trailing_zeros() as u64;
        m &= m - 1;
        let e = enc36(scramble((u as u64) << 9 | a, ukey));
        if e.0 < best.0 {
            best = e;
        }
    }
    best
}

impl Core {
    pub fn new(nc: usize, nd: usize, row_cap: usize, ws_mod: u64, day0: i32) -> Core {
        let mut names_off = AVec::new(row_cap + 1);
        names_off.push(0u32);
        Core {
            ws_mod,
            day0,
            nd,
            organic: nc,
            agg: vec![[0; 2]; (nc + 2) * nd],
            aggp: vec![[0; 2]; (nc + 2) * nd],
            names_off,
            names: AVec::new(row_cap * 10),
            keys: AVec::new(row_cap),
            day: AVec::new(row_cap),
            ses: AVec::new(row_cap),
            evs: AVec::new(row_cap),
            rev: AVec::new(row_cap),
            cat: AVec::new(row_cap),
            n_sess: 0,
            sess: Vec::new(),
            stamp: vec![0; nc + 1],
            serial: 0,
            catsum: [0; 32],
            ccnt: vec![0; nc + 2],
            cts: Vec::new(),
            dstart: vec![0; nd + 2],
        }
    }

    #[inline(always)]
    fn flush_row(&mut self, pidx: u32, day: u32, s: u32, e: u32, rev: u64, catmask: u32) {
        let mut top = 255u8;
        if catmask != 0 {
            let mut m = catmask;
            let mut best = 0u64;
            while m != 0 {
                let c = m.trailing_zeros() as usize;
                m &= m - 1;
                let v = self.catsum[c];
                self.catsum[c] = 0;
                if v > best {
                    best = v;
                    top = c as u8;
                }
            }
        }
        if rev > i32::MAX as u64 || e > i32::MAX as u32 {
            fallback("revenue_cents or events over i32");
        }
        self.keys.push(pidx);
        self.day.push(self.day0 + day as i32);
        self.ses.push(s as i32);
        self.evs.push(e as i32);
        self.rev.push(rev as i32);
        self.cat.push(top);
    }

    #[inline]
    pub fn person(&mut self, g: &G, evs: &[Rec], cks: &[[u64; 2]], name: Name) {
        if evs.is_empty() {
            return;
        }
        let mut st = self.begin(evs[0].w0);
        for r in evs {
            self.feed(&mut st, r.w0);
        }
        self.finish(g, &st, cks, name);
    }

    #[inline(always)]
    pub fn begin(&mut self, w0: u64) -> St {
        self.sess.clear();
        let t = w0 >> 33;
        St { prev: t, s_start: t, s_day: self.dayof(t), s_rev: 0, d_s: 1, d_e: 0, d_rev: 0, catmask: 0, mark: self.day.len(), pidx: (self.names_off.len() - 1) as u32 }
    }

    #[inline(always)]
    fn dayof(&self, t: u64) -> u32 {
        ((t + self.ws_mod) / DAY_MS as u64) as u32
    }

    #[inline(always)]
    pub fn feed(&mut self, st: &mut St, w: u64) {
        let t = w >> 33;
        if t.wrapping_sub(st.prev) > GAP_MS {
            self.sess.push(Sess { start: st.s_start + WEEK_MS, day: st.s_day, rev: st.s_rev });
            let nd = self.dayof(t);
            if nd != st.s_day {
                self.flush_row(st.pidx, st.s_day, st.d_s, st.d_e, st.d_rev, st.catmask);
                st.d_s = 0;
                st.d_e = 0;
                st.d_rev = 0;
                st.catmask = 0;
                st.s_day = nd;
            }
            st.d_s += 1;
            st.s_start = t;
            st.s_rev = 0;
        }
        st.prev = t;
        st.d_e += 1;
        let price = (w >> 9) & 0xFF_FFFF;
        if price != 0 {
            let c = ((w >> 4) & 31) as usize;
            st.s_rev += price;
            st.d_rev += price;
            self.catsum[c] += price;
            st.catmask |= 1 << c;
        }
    }

    pub fn abort(&mut self, st: &St) {
        let mut m = st.catmask;
        while m != 0 {
            self.catsum[m.trailing_zeros() as usize] = 0;
            m &= m - 1;
        }
        self.keys.truncate(st.mark);
        self.day.truncate(st.mark);
        self.ses.truncate(st.mark);
        self.evs.truncate(st.mark);
        self.rev.truncate(st.mark);
        self.cat.truncate(st.mark);
    }

    #[inline(always)]
    pub fn finish(&mut self, g: &G, st: &St, cks: &[[u64; 2]], name: Name) {
        self.sess.push(Sess { start: st.s_start + WEEK_MS, day: st.s_day, rev: st.s_rev });
        self.flush_row(st.pidx, st.s_day, st.d_s, st.d_e, st.d_rev, st.catmask);
        self.n_sess += self.sess.len() as u64;
        match name {
            Name::Simple(u, class) => {
                let (v, len) = simple_name(g, u, class);
                self.names.push(b'u');
                self.names.extend(&v.to_be_bytes()[..len]);
            }
            Name::Bytes(b) => self.names.extend(b),
        }
        if self.names.len() > u32::MAX as usize {
            fallback("names over 4 GB");
        }
        self.names_off.push(self.names.len() as u32);

        let nd = self.nd;
        let n = cks.len();
        if unsafe { crate::parse::SKIP } & 1 != 0 {
            return;
        }
        if n == 0 {
            let base = self.organic * nd;
            for s in &self.sess {
                let i = base + s.day as usize;
                self.agg[i][0] += 1;
                if s.rev != 0 {
                    self.aggp[i][0] += 1;
                    self.aggp[i][1] += s.rev;
                }
            }
            return;
        }
        let small = self.sess.len() * n <= 4096;
        let (mut lo, mut hi) = (0usize, 0usize);
        for s in &self.sess {
            while hi < n && (cks[hi][0] >> 32) <= s.start {
                hi += 1;
            }
            while lo < hi && (cks[lo][0] >> 32) + WEEK_MS < s.start {
                lo += 1;
            }
            let camp = if hi > lo { ((cks[hi - 1][0] >> 4) & (CBOUND as u64 - 1)) as usize } else { self.organic };
            let i = camp * nd + s.day as usize;
            self.agg[i][0] += 1;
            if s.rev != 0 {
                self.aggp[i][0] += 1;
                self.aggp[i][1] += s.rev;
            }
            if small && hi > lo && unsafe { crate::parse::SKIP } & 2 == 0 {
                self.serial += 1;
                let serial = self.serial;
                for k in &cks[lo..hi] {
                    let c = ((k[0] >> 4) & (CBOUND as u64 - 1)) as usize;
                    if self.stamp[c] != serial {
                        self.stamp[c] = serial;
                        self.agg[c * nd + s.day as usize][1] += 1;
                    }
                }
            }
        }
        if !small {
            self.assists_big(cks);
        }
    }

    #[inline(never)]
    fn assists_big(&mut self, cks: &[[u64; 2]]) {
        let nd = self.nd;
        let ns = self.sess.len();
        let mut i = 0usize;
        for d in 0..=nd {
            while i < ns && (self.sess[i].day as usize) < d {
                i += 1;
            }
            self.dstart[d] = i as u32;
        }
        self.dstart[nd + 1] = ns as u32;
        let n = cks.len();
        self.cts.clear();
        if n < 1024 {
            self.cts.extend(cks.iter().map(|k| ((k[0] >> 4) & (CBOUND as u64 - 1)) << 32 | k[0] >> 32));
            self.cts.sort_unstable();
        } else {
            let nc1 = self.ccnt.len() - 1;
            self.ccnt.fill(0);
            for k in cks {
                self.ccnt[((k[0] >> 4) & (CBOUND as u64 - 1)) as usize + 1] += 1;
            }
            for c in 0..nc1 {
                self.ccnt[c + 1] += self.ccnt[c];
            }
            self.cts.resize(n, 0);
            for k in cks {
                let c = ((k[0] >> 4) & (CBOUND as u64 - 1)) as usize;
                self.cts[self.ccnt[c] as usize] = (c as u64) << 32 | k[0] >> 32;
                self.ccnt[c] += 1;
            }
        }
        let mut j = 0usize;
        while j < n {
            let c = (self.cts[j] >> 32) as usize;
            let a = self.cts[j] & 0xFFFF_FFFF;
            let mut b = a + WEEK_MS;
            j += 1;
            while j < n && (self.cts[j] >> 32) as usize == c && (self.cts[j] & 0xFFFF_FFFF) <= b {
                b = (self.cts[j] & 0xFFFF_FFFF) + WEEK_MS;
                j += 1;
            }
            let lo = self.sess.partition_point(|s| s.start < a);
            let hi = self.sess.partition_point(|s| s.start <= b);
            let mut k = lo;
            while k < hi {
                let d = self.sess[k].day as usize;
                let e = (self.dstart[d + 1] as usize).min(hi);
                self.agg[c * nd + d][1] += (e - k) as u32;
                k = e;
            }
        }
    }
}
