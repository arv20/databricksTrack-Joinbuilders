//! Clicks: parse, validate, route to person shards.
//! Dedup on click_id is not needed: copies have identical values (rules.md), so a copy changes
//! neither the latest-click choice (same ts, same click_id, same campaign) nor the set of assisting campaigns.
use crate::events::{shard_of, slack};
use crate::links::LinkMap;
use crate::parse::{for_lines, hash, parse_ts, split, B, DAY_MS};
use hashbrown::HashMap;

pub struct Ck0 {
    pub cid: B,
    pub user: B,
    pub ts: i64,
    pub camp: u32,
}

pub struct Ck {
    pub name: B,
    pub nh: u64,
    pub cid: B,
    pub ts: i64,
    pub camp: u32,
}

pub fn parse_chunk(c: B, ws: i64, we: i64, campaigns: &HashMap<B, u32, foldhash::fast::FixedState>) -> Vec<Ck0> {
    let lo = ws - 7 * DAY_MS;
    let mut v = Vec::with_capacity(memchr::memchr_iter(b'\n', c).count() + 1);
    for_lines(c, |line| {
        let Some([cid, user, camp, ts]) = split::<4>(line) else { return };
        if cid.is_empty() || user.is_empty() {
            return;
        }
        let Some(t) = parse_ts(ts) else { return };
        if t < lo || t >= we {
            return;
        }
        let Some(&ci) = campaigns.get(camp) else { return };
        v.push(Ck0 { cid, user, ts: t, camp: ci });
    });
    v
}

pub fn route(v: &[Ck0], links: &LinkMap, npshard: usize) -> Vec<Vec<Ck>> {
    let mut out: Vec<Vec<Ck>> = (0..npshard).map(|_| Vec::with_capacity(slack(v.len() / npshard))).collect();
    for c in v {
        let (name, nh) = links.name(c.user, hash(c.user));
        out[shard_of(nh, npshard)].push(Ck { name, nh, cid: c.cid, ts: c.ts, camp: c.camp });
    }
    out
}
