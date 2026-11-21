//! Attribution: credit each session to the latest qualifying click (tie: smallest click_id),
//! and count every campaign with a click in [start - 7 d, start] as an assist.
use crate::sessions::{key, Acc, PC, Sess};

pub const WEEK_MS: i64 = 7 * 86_400_000;

/// cks sorted by (ts, click_id); rs = run starts of equal ts (scratch).
pub fn credit(sess: &[Sess], cks: &[PC], rs: &mut Vec<u32>, organic: u32, acc: &mut Acc) {
    rs.clear();
    for i in 0..cks.len() {
        if i > 0 && cks[i].ts == cks[i - 1].ts {
            let r = rs[i - 1];
            rs.push(r);
        } else {
            rs.push(i as u32);
        }
    }
    let n = cks.len();
    let (mut lo, mut hi) = (0usize, 0usize);
    for s in sess {
        while hi < n && cks[hi].ts <= s.start {
            hi += 1;
        }
        while lo < hi && cks[lo].ts < s.start - WEEK_MS {
            lo += 1;
        }
        let camp = if hi > lo { cks[rs[hi - 1] as usize].camp } else { organic };
        let a = acc.entry(key(camp, s.day)).or_insert([0; 4]);
        a[0] += 1;
        a[1] += s.purch as u64;
        a[2] += s.rev;
    }
}

/// starts/days sorted by start; day_end[i] = first index after i with a different day.
pub fn assists(starts: &[i64], days: &[i32], day_end: &[u32], cks: &[PC], ct: &mut Vec<(u32, i64)>, acc: &mut Acc) {
    ct.clear();
    ct.extend(cks.iter().map(|c| (c.camp, c.ts)));
    ct.sort_unstable();
    let n = ct.len();
    let mut flush = |camp: u32, a: i64, b: i64| {
        let lo = starts.partition_point(|&s| s < a);
        let hi = starts.partition_point(|&s| s <= b);
        let mut k = lo;
        while k < hi {
            let e = (day_end[k] as usize).min(hi);
            acc.entry(key(camp, days[k])).or_insert([0; 4])[3] += (e - k) as u64;
            k = e;
        }
    };
    let mut i = 0;
    while i < n {
        let camp = ct[i].0;
        let a = ct[i].1;
        let mut b = a + WEEK_MS;
        i += 1;
        while i < n && ct[i].0 == camp && ct[i].1 <= b {
            b = b.max(ct[i].1 + WEEK_MS);
            i += 1;
        }
        flush(camp, a, b);
    }
}
