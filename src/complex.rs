use crate::core::Name;
use crate::events::{cid_cmp, cpush};
use crate::links::{name_of, resolve_normal, resolve_token};
use crate::*;
use std::sync::atomic::Ordering;

fn resolve_all(g: &G, t: &mut TState) {
    let raw = std::mem::take(&mut t.cx_raw);
    let mut last = u64::MAX;
    let mut cache = [u64::MAX; 16];
    for r in &raw {
        let pk = if r.w1 & ODDU != 0 {
            let rf = (r.w1 & (ODDU - 1)) as usize;
            resolve_token(g, g.t(rf >> 22).odd_users[rf & ((1 << 22) - 1)])
        } else {
            if r.w1 != last {
                last = r.w1;
                cache = [u64::MAX; 16];
            }
            let a = (r.w0 & 15) as usize;
            if cache[a] == u64::MAX {
                cache[a] = resolve_normal(g, r.w1 as u32, a as u32);
            }
            cache[a]
        };
        cpush(t, pk, r.w0);
    }
}

fn clicks_a(g: &G, tid: usize) {
    let t = g.t(tid);
    resolve_all(g, t);
    loop {
        let i0 = g.next.fetch_add(64, Ordering::Relaxed);
        if i0 >= g.cxu.len() {
            break;
        }
        for &u in &g.cxu[i0..(i0 + 64).min(g.cxu.len())] {
            let mut cache = [u64::MAX; 16];
            for c in g.csr_range(u) {
                let a = (c[0] & 15) as usize;
                if cache[a] == u64::MAX {
                    cache[a] = resolve_normal(g, u, a as u32);
                }
                t.cx_ck[shard(cache[a])].push([cache[a], c[0], c[1]]);
            }
        }
    }
    let odd = std::mem::take(&mut t.odd_clicks);
    for c in &odd {
        let pk = resolve_token(g, c[0]);
        t.cx_ck[shard(pk)].push([pk, c[1], c[2]]);
    }
    t.odd_clicks = odd;
}

fn persons_b(g: &G, tid: usize) {
    let t = g.t(tid);
    let mut evs: Vec<Rec> = Vec::new();
    let mut cks: Vec<[u64; 3]> = Vec::new();
    let mut name = Vec::new();
    loop {
        let s = g.next.fetch_add(1, Ordering::Relaxed);
        if s >= NSHARD {
            break;
        }
        evs.clear();
        cks.clear();
        for th in 0..g.nt {
            evs.extend_from_slice(&g.t(th).cx_ev[s]);
            cks.extend_from_slice(&g.t(th).cx_ck[s]);
        }
        if evs.is_empty() {
            continue;
        }
        evs.sort_unstable_by_key(|r| (r.w1, r.w0));
        cks.sort_unstable_by(|a, b| (a[0], a[1] >> 32).cmp(&(b[0], b[1] >> 32)).then_with(|| cid_cmp(g, b[2], a[2])));
        let (mut i, mut j) = (0usize, 0usize);
        while i < evs.len() {
            let pk = evs[i].w1;
            let mut i2 = i + 1;
            while i2 < evs.len() && evs[i2].w1 == pk {
                i2 += 1;
            }
            while j < cks.len() && cks[j][0] < pk {
                j += 1;
            }
            t.ck.clear();
            while j < cks.len() && cks[j][0] == pk {
                t.ck.push([cks[j][1], cks[j][2]]);
                j += 1;
            }
            name_of(g, pk, &mut name);
            t.core.person(g, &evs[i..i2], &t.ck, Name::Bytes(&name));
            i = i2;
        }
    }
}

pub fn stage4(g: &mut G) {
    let users: Vec<u32> = g.extra.keys().copied().collect();
    for u in users {
        let tv = unsafe { *g.utab.add(u as usize) };
        let t0 = g.t(0);
        if tv & COMPLEX != 0 {
            for &w0 in &g.extra[&u] {
                t0.cx_raw.push(Rec { w0, w1: u as u64 });
                t0.n_odd += 1;
            }
        } else if unsafe { (*g.claim.add(u as usize >> 6)).load(Ordering::Relaxed) } >> (u & 63) & 1 == 0 {
            unsafe { crate::events::process_run(g, t0, u, &mut []) };
        }
    }
    for oe in &g.odd_kept {
        let t0 = g.t(0);
        t0.cx_raw.push(Rec { w0: oe.w0, w1: oe.user });
        t0.n_odd += 1;
    }
    let mut cxu = Vec::new();
    for tid in 0..g.nt {
        cxu.extend_from_slice(&g.t(tid).cx_users);
        cxu.extend_from_slice(&g.t(tid).mixed_users);
    }
    g.cxu = cxu;
    g.next.store(0, Ordering::Relaxed);
    {
        let gr: &G = g;
        sys::run_par(gr.nt, |tid| clicks_a(gr, tid));
    }
    g.next.store(0, Ordering::Relaxed);
    {
        let gr: &G = g;
        sys::run_par(gr.nt, |tid| persons_b(gr, tid));
    }
}
