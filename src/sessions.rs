//! Group routed events and clicks by person, sessionize, build user_daily rows, run attribution.
use crate::attribution;
use crate::clicks::Ck;
use crate::events::{Rec, T_PURCHASE};
use crate::parse::{day_of, B};
use hashbrown::{HashMap, HashTable};

pub const GAP_MS: i64 = 1_800_000;

pub type Acc = HashMap<u64, [u64; 4], foldhash::fast::FixedState>; // sessions, purchasing, revenue, assisted

#[inline]
pub fn key(camp: u32, day: i32) -> u64 {
    ((camp as u64) << 32) | (day as u32 as u64)
}
pub fn unkey(k: u64) -> (u32, i32) {
    ((k >> 32) as u32, k as u32 as i32)
}

#[derive(Clone, Copy)]
pub struct PE {
    pub ts: i64,
    pub price: u32,
    pub cat: u32,
    pub typ: u8,
}

#[derive(Clone, Copy)]
pub struct PC {
    pub ts: i64,
    pub cid: B,
    pub camp: u32,
}

pub struct Sess {
    pub start: i64,
    pub day: i32,
    pub rev: u64,
    pub purch: bool,
}

pub struct UserRow {
    pub name: B,
    pub day: i32,
    pub sessions: u64,
    pub events: u64,
    pub revenue: u64,
    pub top: Option<u32>,
}

pub struct Scratch {
    sess: Vec<Sess>,
    starts: Vec<i64>,
    days: Vec<i32>,
    day_end: Vec<u32>,
    catsum: Vec<u64>,
    touched: Vec<u32>,
    ct: Vec<(u32, i64)>,
    rs: Vec<u32>,
}

impl Scratch {
    pub fn new(ncat: usize) -> Self {
        Scratch { sess: vec![], starts: vec![], days: vec![], day_end: vec![], catsum: vec![0; ncat], touched: vec![], ct: vec![], rs: vec![] }
    }
}

struct DayAcc {
    day: i32,
    sessions: u64,
    events: u64,
    revenue: u64,
}

fn flush_row(name: B, d: &DayAcc, sc: &mut Scratch, rows: &mut Vec<UserRow>) {
    let mut top: Option<u32> = None;
    let mut best = 0u64;
    for &c in &sc.touched {
        let v = sc.catsum[c as usize];
        if top.is_none() || v > best || (v == best && c < top.unwrap()) {
            top = Some(c);
            best = v;
        }
        sc.catsum[c as usize] = 0;
    }
    sc.touched.clear();
    rows.push(UserRow { name, day: d.day, sessions: d.sessions, events: d.events, revenue: d.revenue, top });
}

/// One person: evs and cks are this person's events and clicks (any order).
pub fn person(name: B, evs: &mut [PE], cks: &mut [PC], sc: &mut Scratch, rows: &mut Vec<UserRow>, organic: u32, acc: &mut Acc) {
    if evs.is_empty() {
        return;
    }
    evs.sort_unstable_by_key(|e| e.ts);
    sc.sess.clear();
    let mut d = DayAcc { day: day_of(evs[0].ts), sessions: 0, events: 0, revenue: 0 };
    let mut prev = evs[0].ts;
    for (i, e) in evs.iter().enumerate() {
        if i == 0 || e.ts - prev > GAP_MS {
            let day = day_of(e.ts);
            if day != d.day {
                flush_row(name, &d, sc, rows);
                d = DayAcc { day, sessions: 0, events: 0, revenue: 0 };
            }
            d.sessions += 1;
            sc.sess.push(Sess { start: e.ts, day, rev: 0, purch: false });
        }
        prev = e.ts;
        d.events += 1;
        if e.typ == T_PURCHASE {
            let s = sc.sess.last_mut().unwrap();
            s.rev += e.price as u64;
            s.purch = true;
            d.revenue += e.price as u64;
            let c = e.cat as usize;
            if sc.catsum[c] == 0 {
                sc.touched.push(e.cat);
            }
            sc.catsum[c] += e.price as u64;
        }
    }
    flush_row(name, &d, sc, rows);

    if !cks.is_empty() {
        cks.sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| a.cid.cmp(b.cid)));
    }
    attribution::credit(&sc.sess, cks, &mut sc.rs, organic, acc);
    if !cks.is_empty() {
        sc.starts.clear();
        sc.days.clear();
        sc.starts.extend(sc.sess.iter().map(|s| s.start));
        sc.days.extend(sc.sess.iter().map(|s| s.day));
        let n = sc.sess.len();
        sc.day_end.clear();
        sc.day_end.resize(n, 0);
        let mut end = n as u32;
        for i in (0..n).rev() {
            if i + 1 < n && sc.days[i + 1] != sc.days[i] {
                end = (i + 1) as u32;
            }
            sc.day_end[i] = end;
        }
        attribution::assists(&sc.starts, &sc.days, &sc.day_end, cks, &mut sc.ct, acc);
    }
}

/// Process person shard p.
pub fn run_shard(p: usize, recs: &[(Vec<Vec<Rec>>, usize)], cks: &[Vec<Vec<Ck>>], ncat: usize, organic: u32, acc: &mut Acc) -> Vec<UserRow> {
    let n_ev: usize = recs.iter().map(|r| r.0[p].len()).sum();
    let mut table: HashTable<(u64, u32)> = HashTable::with_capacity(n_ev / 8 + 16);
    let mut names: Vec<B> = Vec::new();
    let mut pid_ev: Vec<u32> = Vec::with_capacity(n_ev);
    for r in recs {
        for e in &r.0[p] {
            let h = e.nh;
            let pid = match table.find(h, |&(eh, i)| eh == h && names[i as usize] == e.name) {
                Some(&(_, i)) => i,
                None => {
                    let i = names.len() as u32;
                    names.push(e.name);
                    table.insert_unique(h, (h, i), |x| x.0);
                    i
                }
            };
            pid_ev.push(pid);
        }
    }
    let np = names.len();
    let mut off = vec![0u32; np + 1];
    for &pid in &pid_ev {
        off[pid as usize + 1] += 1;
    }
    for i in 0..np {
        off[i + 1] += off[i];
    }
    let mut pos: Vec<u32> = off[..np].to_vec();
    let mut pe = vec![PE { ts: 0, price: 0, cat: 0, typ: 0 }; n_ev];
    let mut k = 0usize;
    for r in recs {
        for e in &r.0[p] {
            let pid = pid_ev[k] as usize;
            k += 1;
            pe[pos[pid] as usize] = PE { ts: e.ts, price: e.price, cat: e.cat, typ: e.typ };
            pos[pid] += 1;
        }
    }
    drop(pid_ev);
    // Clicks of people with no surviving event cannot credit anything: skip them.
    let n_ck: usize = cks.iter().map(|c| c[p].len()).sum();
    let mut pid_ck: Vec<u32> = Vec::with_capacity(n_ck);
    let mut coff = vec![0u32; np + 1];
    for c in cks {
        for x in &c[p] {
            let h = x.nh;
            let pid = match table.find(h, |&(eh, i)| eh == h && names[i as usize] == x.name) {
                Some(&(_, i)) => {
                    coff[i as usize + 1] += 1;
                    i
                }
                None => u32::MAX,
            };
            pid_ck.push(pid);
        }
    }
    for i in 0..np {
        coff[i + 1] += coff[i];
    }
    let mut cpos: Vec<u32> = coff[..np].to_vec();
    let nck_kept = coff[np] as usize;
    let mut pc = vec![PC { ts: 0, cid: &[], camp: 0 }; nck_kept];
    let mut k = 0usize;
    for c in cks {
        for x in &c[p] {
            let pid = pid_ck[k];
            k += 1;
            if pid != u32::MAX {
                let q = &mut cpos[pid as usize];
                pc[*q as usize] = PC { ts: x.ts, cid: x.cid, camp: x.camp };
                *q += 1;
            }
        }
    }
    drop(pid_ck);
    let mut sc = Scratch::new(ncat);
    let mut rows = Vec::with_capacity(np * 2);
    for i in 0..np {
        let evs = &mut pe[off[i] as usize..off[i + 1] as usize];
        let cs = &mut pc[coff[i] as usize..coff[i + 1] as usize];
        person(names[i], evs, cs, &mut sc, &mut rows, organic, acc);
    }
    rows
}
