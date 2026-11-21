//! Events: parse, dedup on event_id (smallest ingest_ts), validate, drop late, route to person shards.
use crate::links::LinkMap;
use crate::parse::{for_lines, hash, parse_price, parse_ts, split, B, DAY_MS};
use hashbrown::{hash_table::Entry, HashMap, HashTable};

pub const T_VIEW: u8 = 0;
pub const T_CART: u8 = 1;
pub const T_PURCHASE: u8 = 2;
pub const T_INVALID: u8 = 255;

/// One parsed copy of an event (valid or not; dedup needs both).
pub struct Ev {
    pub eid: B,
    pub user: B,
    pub ingest: i64,
    pub ts: i64,
    pub price: u32,
    pub cat: u32,
    pub typ: u8,
}

/// A surviving event, routed to its person.
pub struct Rec {
    pub name: B,
    pub nh: u64,
    pub ts: i64,
    pub price: u32,
    pub cat: u32,
    pub typ: u8,
}

pub struct EvCtx<'a> {
    pub ws: i64,
    pub we: i64,
    pub products: &'a HashMap<B, u32, foldhash::fast::FixedState>,
    pub unknown_cat: u32,
}

/// Capacity for a bucket that expects n items on average: n plus about 4 standard deviations.
pub fn slack(n: usize) -> usize {
    n + 4 * (n as f64).sqrt() as usize + 16
}

#[inline]
pub fn shard_of(h: u64, nshard: usize) -> usize {
    ((h >> 32) as usize) & (nshard - 1)
}

/// Parse one chunk into per-dedup-shard vectors.
pub fn parse_chunk(c: B, ctx: &EvCtx, nshard: usize) -> Vec<Vec<Ev>> {
    let cap = slack(memchr::memchr_iter(b'\n', c).count() / nshard);
    let mut out: Vec<Vec<Ev>> = (0..nshard).map(|_| Vec::with_capacity(cap)).collect();
    for_lines(c, |line| {
        let Some([eid, user, ts, ingest, typ, product, price]) = split::<7>(line) else { return };
        if eid.is_empty() {
            return;
        }
        // rules.md: ingest_ts is always valid F1. Choice: an unparsable ingest_ts makes the line malformed.
        let Some(ingest) = parse_ts(ingest) else { return };
        let h = hash(eid);
        let mut e = Ev { eid, user, ingest, ts: 0, price: 0, cat: 0, typ: T_INVALID };
        'v: {
            if user.is_empty() || product.is_empty() {
                break 'v;
            }
            let Some(t) = parse_ts(ts) else { break 'v };
            if t < ctx.ws || t >= ctx.we {
                break 'v;
            }
            let ty = match typ {
                b"view" => T_VIEW,
                b"cart" => T_CART,
                b"purchase" => T_PURCHASE,
                _ => break 'v,
            };
            if ty == T_PURCHASE {
                let Some(p) = parse_price(price) else { break 'v };
                e.price = p;
                e.cat = *ctx.products.get(product).unwrap_or(&ctx.unknown_cat);
            }
            // Late: ingest_ts - ts over 24 h. Folded into validity: it only depends on the kept copy.
            if ingest - t > DAY_MS {
                break 'v;
            }
            e.ts = t;
            e.typ = ty;
        }
        out[shard_of(h, nshard)].push(e);
    });
    out
}

/// Dedup one shard, then route the survivors to person shards.
pub fn dedup_route(parts: &[Vec<Vec<Ev>>], s: usize, links: &LinkMap, npshard: usize) -> (Vec<Vec<Rec>>, usize) {
    let total: usize = parts.iter().map(|p| p[s].len()).sum();
    let mut t: HashTable<(u64, u32, u32)> = HashTable::with_capacity(total);
    for (ci, p) in parts.iter().enumerate() {
        for (i, e) in p[s].iter().enumerate() {
            let eh = hash(e.eid);
            match t.entry(eh, |&(h, c, j)| h == eh && parts[c as usize][s][j as usize].eid == e.eid, |x| x.0) {
                Entry::Occupied(mut o) => {
                    let (_, c, j) = *o.get();
                    if e.ingest < parts[c as usize][s][j as usize].ingest {
                        *o.get_mut() = (eh, ci as u32, i as u32);
                    }
                }
                Entry::Vacant(v) => {
                    v.insert((eh, ci as u32, i as u32));
                }
            }
        }
    }
    let cap = slack(t.len() / npshard);
    let mut out: Vec<Vec<Rec>> = (0..npshard).map(|_| Vec::with_capacity(cap)).collect();
    let mut kept = 0usize;
    for &(_, c, j) in t.iter() {
        let e = &parts[c as usize][s][j as usize];
        if e.typ == T_INVALID {
            continue;
        }
        kept += 1;
        let (name, nh) = links.name(e.user, hash(e.user));
        out[shard_of(nh, npshard)].push(Rec { name, nh, ts: e.ts, price: e.price, cat: e.cat, typ: e.typ });
    }
    (out, kept)
}
