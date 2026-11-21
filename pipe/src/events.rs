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
unsafe fn edge_copy(t: &mut TState, tid: usize, u: u32, recs: &[Rec]) -> Seg {
    if t.edge.len() + recs.len() > u32::MAX as usize {
        fallback("edge store over 4G records");
    }
    let seg = Seg { tid: tid as u32, off: t.edge.len() as u32, len: recs.len() as u32, user: u };
    t.edge.extend(recs);
    seg
}

fn stage3a(g: &G, tid: usize) {
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

fn stage3b(g: &G, tid: usize) {
    let t = g.t(tid);
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
        run_par(gr.nt, |tid| stage3a(gr, tid));
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
