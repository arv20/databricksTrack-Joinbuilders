#[cfg(test)]
mod csv;
mod alloc;
mod arena;
mod ids;
mod io;
mod parquet;
mod scan;
mod table;
mod ts;

use ids::{Key, hash, key, key_cmp};
use memchr::memchr;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use arena::Scatter;
use table::KeyMap;
use ts::DAY;

/// How many records ahead person-map lookups are prefetched.
const PF_DIST: usize = 32;
const GAP: i64 = 30 * ts::MIN;
const LATE: i64 = DAY;
const LOOKBACK: i64 = 7 * DAY;
const NO_CAT: u32 = u32::MAX;

// ------------------------------------------------------------------ records

/// One parsed event line (before dedup). `cat == INVALID` marks a copy that
/// fails validation (or is late); its other payload is unused.
#[repr(C, packed(8))]
#[derive(Clone, Copy)]
struct EvRec<E: Copy> {
    eid: Key,
    user: Key,
    ingest: i64,
    /// the event packed as it is kept (`E::make`), or `E::INVALID`
    ev: E,
}

/// One valid click line.
#[repr(C, packed(8))]
#[derive(Clone, Copy)]
struct CkRec {
    cid: Key,
    user: Key,
    ts: i64,
    camp: u32,
}

struct ParseOut<E: Copy> {
    ev: Scatter<EvRec<E>>,
    ck: Vec<CkRec>,
}

/// A surviving event, keyed by person.
#[repr(C, packed(8))]
#[derive(Clone, Copy)]
struct PEv<E: Copy> {
    person: Key,
    ev: E,
}

/// A valid click, keyed by person.
#[repr(C, packed(8))]
#[derive(Clone, Copy)]
struct PCk {
    person: Key,
    cid: Key,
    ts: i64,
    camp: u32,
}

/// A surviving event in memory: its `ts - window_start`, category and price
/// packed into one integer whose order is `ts` order first, so a person's
/// events sort as plain integers.
trait EvT: Copy + Default + Ord + Send + Sync + 'static {
    /// Marks a copy that fails validation: all ones, never a valid packing
    /// (its price field would exceed 10^7).
    const INVALID: Self;
    fn make(rel: u64, cat: u32, price: u32) -> Self;
    fn rel(self) -> u64;
    fn cat(self) -> u32;
    fn price(self) -> u32;
    /// Two values in one integer, ordered by `hi` first: each must be under
    /// 2^32 for `u64` (2^64 for `u128`).
    fn pair(hi: u64, lo: u64) -> Self;
    fn hi(self) -> u64;
    fn lo(self) -> u64;
}

/// `rel << 32 | price << 8 | cat`: needs `rel < 2^32` (a window under ~49
/// days), at most 256 categories, and price < 2^24 (it is at most 10^7).
impl EvT for u64 {
    const INVALID: Self = u64::MAX;
    #[inline(always)]
    fn make(rel: u64, cat: u32, price: u32) -> Self {
        (rel << 32) | ((price as u64) << 8) | cat as u64
    }
    #[inline(always)]
    fn rel(self) -> u64 {
        self >> 32
    }
    #[inline(always)]
    fn cat(self) -> u32 {
        self as u8 as u32
    }
    #[inline(always)]
    fn price(self) -> u32 {
        (self as u32) >> 8
    }
    #[inline(always)]
    fn pair(hi: u64, lo: u64) -> Self {
        (hi << 32) | lo
    }
    #[inline(always)]
    fn hi(self) -> u64 {
        self >> 32
    }
    #[inline(always)]
    fn lo(self) -> u64 {
        self & 0xffff_ffff
    }
}

/// `rel << 64 | price << 32 | cat`: the general fallback.
impl EvT for u128 {
    const INVALID: Self = u128::MAX;
    #[inline(always)]
    fn make(rel: u64, cat: u32, price: u32) -> Self {
        ((rel as u128) << 64) | ((price as u128) << 32) | cat as u128
    }
    #[inline(always)]
    fn rel(self) -> u64 {
        (self >> 64) as u64
    }
    #[inline(always)]
    fn cat(self) -> u32 {
        self as u32
    }
    #[inline(always)]
    fn price(self) -> u32 {
        (self >> 32) as u32
    }
    #[inline(always)]
    fn pair(hi: u64, lo: u64) -> Self {
        ((hi as u128) << 64) | lo as u128
    }
    #[inline(always)]
    fn hi(self) -> u64 {
        (self >> 64) as u64
    }
    #[inline(always)]
    fn lo(self) -> u64 {
        self as u64
    }
}

/// Whether every surviving event and click fits the `u64` packing (a click's
/// `ts - (window_start - 7 days)` is the larger offset).
fn packed_fits(ctx: &Ctx) -> bool {
    ctx.cat_names.len() <= 256 && ctx.we - ctx.cws <= u32::MAX as i64 && std::env::var_os("SS_WIDE").is_none()
}

/// A person bucket's valid clicks: `keys` hold `pair(ts - cws, pos)` (sorted
/// per person), `cid[pos]` and `camp[pos]` the rest of the click.
struct Cks<'a, E> {
    keys: &'a mut [E],
    cid: &'a [Key],
    camp: &'a [u32],
}

struct UdRow {
    name: Key,
    day: i32,
    sessions: u32,
    events: u32,
    revenue: u64,
    top: u32,
}

// ------------------------------------------------------------------ context

struct Ctx {
    ws: i64,
    we: i64,
    cws: i64,
    day0: i64,
    ndays: usize,
    products: KeyMap<u32>,
    /// Fast path: products named `<pfx><canonical decimal>` by number.
    pfx: u8,
    pnum: Vec<u32>,
    unknown_cat: u32,
    cat_names: Vec<Vec<u8>>,
    campaigns: KeyMap<u32>,
    camp_names: Vec<Key>,
    camp_channels: Vec<Vec<u8>>,
    nthreads: usize,
}

fn log(msg: &str) {
    eprintln!("{msg}");
}

struct Timer {
    t0: Instant,
    last: Instant,
}

impl Timer {
    fn lap(&mut self, what: &str) {
        let now = Instant::now();
        eprintln!(
            "[{:>8.1} ms] {:<28} {:>8.1} ms",
            (now - self.t0).as_secs_f64() * 1e3,
            what,
            (now - self.last).as_secs_f64() * 1e3
        );
        self.last = now;
    }
}

fn csv_files(dir: &Path) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let p = e.path();
            let name = e.file_name();
            if name.to_string_lossy().ends_with(".csv") && p.is_file() {
                v.push(p);
            }
        }
    }
    v.sort();
    v
}

/// Calls `f` on the fields of every well-formed data line whose first byte
/// lies in `[s, e)` of `data` (the file's first line is its header and is skipped).
#[inline(always)]
fn for_lines<const N: usize>(data: &[u8], s: usize, e: usize, mut f: impl FnMut(&[&[u8]; N])) {
    let len = data.len();
    let mut p = if s == 0 {
        match memchr(b'\n', data) {
            Some(i) => i + 1,
            None => len,
        }
    } else if data[s - 1] == b'\n' {
        s
    } else {
        match memchr(b'\n', &data[s..]) {
            Some(i) => s + i + 1,
            None => len,
        }
    };
    if p >= e || p >= len {
        return;
    }
    let mut sc = scan::Scanner::new(data, p);
    let mut fields = [&b""[..]; N];
    while p < e && p < len {
        let (ok, next) = sc.line::<N>(p, &mut fields);
        if ok {
            f(&fields);
        }
        p = next;
    }
}

/// Rows of a two-column lookup file: (key, raw key bytes, value bytes). The
/// slices point into a leaked buffer, so no row allocates.
fn read_lookup(path: &Path) -> Vec<(Key, &'static [u8], &'static [u8])> {
    let data = io::read_small(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut out = Vec::new();
    for_lines::<2>(data, 0, data.len(), |f| {
        if !f[0].is_empty() {
            // re-borrow the fields from `data` itself to keep its 'static lifetime
            let at = |x: &[u8]| -> &'static [u8] {
                let o = x.as_ptr() as usize - data.as_ptr() as usize;
                &data[o..o + x.len()]
            };
            out.push((key(f[0]), at(f[0]), at(f[1])));
        }
    });
    out
}

fn manifest_ts(text: &str, field: &str) -> i64 {
    let at = text.find(&format!("\"{field}\"")).expect("manifest field");
    let rest = &text[at + field.len() + 2..];
    let q1 = rest.find('"').unwrap();
    let rest = &rest[q1 + 1..];
    let q2 = rest.find('"').unwrap();
    ts::parse(rest[..q2].as_bytes()).expect("manifest timestamp")
}

struct Chunk {
    data: &'static [u8],
    s: usize,
    e: usize,
}

fn plan_chunks(files: &[&'static [u8]], target: usize, out: &mut Vec<Chunk>) {
    for &data in files {
        let len = data.len();
        if len == 0 {
            continue;
        }
        let n = len.div_ceil(target).max(1);
        let step = len.div_ceil(n);
        let mut s = 0;
        while s < len {
            let e = (s + step).min(len);
            out.push(Chunk { data, s, e });
            s = e;
        }
    }
}

/// Runs `nthreads` workers, each with its own state, pulling task indexes
/// 0..ntasks from a shared counter. Returns the states.
fn parallel<S: Send>(
    nthreads: usize,
    ntasks: usize,
    init: impl Fn(usize) -> S + Sync,
    work: impl Fn(&mut S, usize) + Sync,
) -> Vec<S> {
    let next = AtomicUsize::new(0);
    std::thread::scope(|sc| {
        let hs: Vec<_> = (0..nthreads)
            .map(|t| {
                let next = &next;
                let init = &init;
                let work = &work;
                sc.spawn(move || {
                    let mut st = init(t);
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        if i >= ntasks {
                            break;
                        }
                        work(&mut st, i);
                    }
                    st
                })
            })
            .collect();
        hs.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

// ------------------------------------------------------------------ links

struct Persons {
    /// user key -> person name key, for every ID in a kept link line.
    map: KeyMap<Key>,
}

impl Persons {
    #[inline(always)]
    fn name(&self, user: Key) -> Key {
        self.map.get(user).unwrap_or(user)
    }
}

fn build_persons(pairs: Vec<Vec<(Key, Key)>>) -> Persons {
    let n: usize = pairs.iter().map(|v| v.len()).sum();
    let mut map: KeyMap<Key> = KeyMap::with_capacity(n + n / 2 + 16);
    let mut keys: Vec<Key> = Vec::with_capacity(n);
    let mut parent: Vec<u32> = Vec::with_capacity(n);
    fn find(parent: &mut [u32], mut x: u32) -> u32 {
        let mut r = x;
        while parent[r as usize] != r {
            r = parent[r as usize];
        }
        while parent[x as usize] != r {
            let nx = parent[x as usize];
            parent[x as usize] = r;
            x = nx;
        }
        r
    }
    for v in &pairs {
        for &(a, b) in v {
            let ia = map.get_or_insert(a, || {
                keys.push(a);
                parent.push(keys.len() as u32 - 1);
                (keys.len() - 1) as Key
            }) as u32;
            let ib = map.get_or_insert(b, || {
                keys.push(b);
                parent.push(keys.len() as u32 - 1);
                (keys.len() - 1) as Key
            }) as u32;
            let ra = find(&mut parent, ia);
            let rb = find(&mut parent, ib);
            if ra != rb {
                // link the larger index under the smaller: any rule works, names come after
                if ra < rb {
                    parent[rb as usize] = ra;
                } else {
                    parent[ra as usize] = rb;
                }
            }
        }
    }
    let mut name: Vec<Key> = keys.clone();
    for i in 0..keys.len() {
        let r = find(&mut parent, i as u32) as usize;
        if key_cmp(keys[i], name[r]).is_lt() {
            name[r] = keys[i];
        }
    }
    for i in 0..keys.len() {
        let r = parent[i] as usize; // fully compressed above
        let r = if parent[r] as usize == r { r } else { find(&mut parent, i as u32) as usize };
        name[i] = name[r];
    }
    map.map_values(|_, v| name[v as usize]);
    Persons { map }
}

// ------------------------------------------------------------------ campaign accumulators

struct CampAcc {
    ndays: usize,
    /// (camp * ndays + day) -> [sessions, purchasing, revenue, assisted]
    cells: Vec<[u64; 4]>,
}

impl CampAcc {
    fn new(ncamp: usize, ndays: usize) -> Self {
        CampAcc { ndays, cells: vec![[0; 4]; (ncamp + 1) * ndays] }
    }
    #[inline(always)]
    fn cell(&mut self, camp: u32, day: u32) -> &mut [u64; 4] {
        &mut self.cells[camp as usize * self.ndays + day as usize]
    }
}

// ------------------------------------------------------------------ sessionize one person

#[derive(Clone, Copy)]
struct Sess {
    start: i64,
    day: u32,
    n: u32,
    rev: u64,
    lo: u32,
    hi: u32,
}

struct Scratch<E> {
    sess: Vec<Sess>,
    cats: Vec<(u32, u32)>,
    starts: Vec<i64>,
    run_end: Vec<u32>,
    cp: Vec<E>,
    /// per campaign: the last session (`sid`) it assisted, for direct assists
    stamp: Vec<u64>,
    sid: u64,
}

/// Up to this many sessions x clicks, a person's assists are found per session.
const DIRECT_ASSISTS: usize = 2048;

#[inline]
fn process_person<E: EvT>(
    ctx: &Ctx,
    name: Key,
    evs: &mut [E],
    cks: Cks<E>,
    sc: &mut Scratch<E>,
    ud: &mut Vec<UdRow>,
    cd: &mut CampAcc,
) {
    if evs.len() > 1 {
        evs.sort_unstable();
    }
    let organic = ctx.camp_names.len() as u32;
    // sessions
    sc.sess.clear();
    let mut i = 0usize;
    let n = evs.len();
    while i < n {
        let srel = evs[i].rel();
        let lo = i;
        let mut prev = srel;
        let mut rev = 0u64;
        while i < n && evs[i].rel() - prev <= GAP as u64 {
            prev = evs[i].rel();
            rev += evs[i].price() as u64;
            i += 1;
        }
        let start = ctx.ws + srel as i64;
        let day = (start.div_euclid(DAY) - ctx.day0) as u32;
        sc.sess.push(Sess { start, day, n: (i - lo) as u32, rev, lo: lo as u32, hi: i as u32 });
    }
    // user_daily
    let ns = sc.sess.len();
    let mut k = 0usize;
    while k < ns {
        let day = sc.sess[k].day;
        let mut sessions = 0u32;
        let mut events = 0u32;
        let mut rev = 0u64;
        sc.cats.clear();
        while k < ns && sc.sess[k].day == day {
            let s = sc.sess[k];
            sessions += 1;
            events += s.n;
            if s.rev > 0 {
                rev += s.rev;
                for e in &evs[s.lo as usize..s.hi as usize] {
                    if e.price() > 0 {
                        sc.cats.push((e.cat(), e.price()));
                    }
                }
            }
            k += 1;
        }
        let top = if sc.cats.is_empty() {
            NO_CAT
        } else {
            sc.cats.sort_unstable_by_key(|c| c.0);
            let mut best = NO_CAT;
            let mut best_sum = 0u64;
            let mut j = 0;
            while j < sc.cats.len() {
                let c = sc.cats[j].0;
                let mut sum = 0u64;
                while j < sc.cats.len() && sc.cats[j].0 == c {
                    sum += sc.cats[j].1 as u64;
                    j += 1;
                }
                if best == NO_CAT || sum > best_sum {
                    best = c;
                    best_sum = sum;
                }
            }
            best
        };
        ud.push(UdRow { name, day: (ctx.day0 + day as i64) as i32, sessions, events, revenue: rev, top });
    }
    // credit
    let Cks { keys: cks, cid, camp: ck_camp } = cks;
    if cks.is_empty() {
        for s in &sc.sess {
            let c = cd.cell(organic, s.day);
            c[0] += 1;
            c[1] += (s.rev > 0) as u64;
            c[2] += s.rev;
        }
        return;
    }
    if cks.len() > 1 {
        cks.sort_unstable();
    }
    let cws = ctx.cws;
    let mut j = 0usize;
    let mut grp = 0usize; // first index of the run of equal ts ending at j-1
    let mut chosen = (usize::MAX, 0u32); // (grp, its smallest click_id's campaign)
    // few sessions x clicks: assists straight from each session's window of
    // clicks [lo, j), a campaign counted once per session by stamping it
    let direct = ns * cks.len() <= DIRECT_ASSISTS;
    if direct && sc.stamp.len() < organic as usize {
        sc.stamp.resize(organic as usize, 0);
    }
    let mut lo = 0usize;
    for s in &sc.sess {
        let srel = (s.start - cws) as u64; // >= 7 days: sessions start in the window
        while j < cks.len() && cks[j].hi() <= srel {
            if j == 0 || cks[j].hi() != cks[j - 1].hi() {
                grp = j;
            }
            j += 1;
        }
        let camp = if j > 0 && srel - cks[j - 1].hi() <= LOOKBACK as u64 {
            if chosen.0 != grp {
                // the latest clicks: on a tie in ts, the smallest click_id
                let mut best = cks[grp].lo() as usize;
                for c in &cks[grp + 1..j] {
                    let q = c.lo() as usize;
                    if key_cmp(cid[q], cid[best]).is_lt() {
                        best = q;
                    }
                }
                chosen = (grp, ck_camp[best]);
            }
            chosen.1
        } else {
            organic
        };
        let c = cd.cell(camp, s.day);
        c[0] += 1;
        c[1] += (s.rev > 0) as u64;
        c[2] += s.rev;
        if direct {
            while lo < j && cks[lo].hi() + (LOOKBACK as u64) < srel {
                lo += 1;
            }
            sc.sid += 1;
            for c in &cks[lo..j] {
                let camp = ck_camp[c.lo() as usize];
                let st = unsafe { sc.stamp.get_unchecked_mut(camp as usize) };
                if *st != sc.sid {
                    *st = sc.sid;
                    cd.cell(camp, s.day)[3] += 1;
                }
            }
        }
    }
    if direct {
        return;
    }
    // assists: the union over a campaign's clicks of [ts, ts + 7d] holds the
    // starts of exactly the sessions it assists
    sc.starts.clear();
    sc.starts.extend(sc.sess.iter().map(|s| s.start));
    sc.run_end.clear();
    sc.run_end.resize(ns, 0);
    let mut e = ns;
    for q in (0..ns).rev() {
        if q + 1 < ns && sc.sess[q + 1].day != sc.sess[q].day {
            e = q + 1;
        }
        sc.run_end[q] = e as u32;
    }
    sc.cp.clear();
    sc.cp.extend(cks.iter().map(|c| E::pair(ck_camp[c.lo() as usize] as u64, c.hi())));
    sc.cp.sort_unstable();
    let cp = &sc.cp;
    let mut q = 0usize;
    while q < cp.len() {
        let camp = cp[q].hi() as u32;
        let mut a = cws + cp[q].lo() as i64;
        let mut b = a + LOOKBACK;
        q += 1;
        loop {
            let more = q < cp.len() && cp[q].hi() as u32 == camp;
            if more && cws + cp[q].lo() as i64 <= b.saturating_add(1) {
                b = cws + cp[q].lo() as i64 + LOOKBACK;
                q += 1;
                continue;
            }
            // emit [a, b]
            let lo = sc.starts.partition_point(|&s| s < a);
            let hi = sc.starts.partition_point(|&s| s <= b);
            let mut x = lo;
            while x < hi {
                let end = (sc.run_end[x] as usize).min(hi);
                cd.cell(camp, sc.sess[x].day)[3] += (end - x) as u64;
                x = end;
            }
            if !more {
                break;
            }
            a = cws + cp[q].lo() as i64;
            b = a + LOOKBACK;
            q += 1;
        }
    }
}

// ------------------------------------------------------------------ main

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let (data_dir, out_dir) = if args.len() >= 3 {
        (PathBuf::from(&args[args.len() - 2]), PathBuf::from(&args[args.len() - 1]))
    } else {
        (
            PathBuf::from(std::env::var("DATA_DIR").unwrap_or("/data".into())),
            PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or("/output".into())),
        )
    };
    let nthreads = std::env::var("NCPU")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .filter(|&n| n > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(8, |n| n.get()))
        .min(32);
    run(&data_dir, &out_dir, nthreads);
    std::process::exit(0);
}

fn run(data_dir: &Path, out_dir: &Path, nthreads: usize) {
    let t0 = Instant::now();
    let mut tm = Timer { t0, last: t0 };

    // ---- small inputs
    let manifest = std::fs::read_to_string(data_dir.join("manifest.json")).expect("manifest.json");
    let ws = manifest_ts(&manifest, "window_start");
    let we = manifest_ts(&manifest, "window_end");
    let day0 = ws.div_euclid(DAY);
    let ndays = if we > ws { ((we - 1).div_euclid(DAY) - day0 + 1) as usize } else { 1 };

    // products and campaigns are built on a helper thread while the main
    // thread maps the input files and parses the links
    let ctx_thread = std::thread::spawn({
        let data_dir = data_dir.to_path_buf();
        move || -> Ctx {
            let data_dir = data_dir.as_path();
            let prod_rows = read_lookup(&data_dir.join("products.csv"));
            let mut cat_set: HashMap<&'static [u8], u32> = HashMap::new();
            for &(_, _, c) in &prod_rows {
                cat_set.entry(c).or_insert(0);
            }
            cat_set.entry(&b"unknown"[..]).or_insert(0);
            let mut cat_names: Vec<Vec<u8>> = cat_set.keys().map(|c| c.to_vec()).collect();
            cat_names.sort();
            for (i, c) in cat_names.iter().enumerate() {
                *cat_set.get_mut(&c[..]).unwrap() = i as u32;
            }
            let unknown_cat = cat_set[&b"unknown"[..]];
            let mut products = KeyMap::with_capacity(prod_rows.len());
            // the most common first byte of product IDs gets the array fast path
            let mut first = [0usize; 256];
            let mut row_cat: Vec<u32> = Vec::with_capacity(prod_rows.len());
            for &(k, raw, c) in &prod_rows {
                let cat = cat_set[c];
                row_cat.push(cat);
                products.insert(k, cat);
                first[raw[0] as usize] += 1;
            }
            let pfx = (0..256).max_by_key(|&i| first[i]).unwrap() as u8;
            let mut pnum: Vec<u32> = Vec::new();
            for (&(_, raw, _), &cat) in prod_rows.iter().zip(&row_cat) {
                // later rows win, as in the map
                if let Some(n) = canonical_num(raw, pfx) {
                    if n < (1 << 22) {
                        if pnum.len() <= n {
                            pnum.resize(n + 1, NO_CAT);
                        }
                        pnum[n] = cat;
                    }
                }
            }
            let camp_rows = read_lookup(&data_dir.join("campaigns.csv"));
            let mut campaigns: KeyMap<u32> = KeyMap::with_capacity(camp_rows.len());
            let mut camp_names = Vec::new();
            let mut camp_channels: Vec<Vec<u8>> = Vec::new();
            for (k, _, ch) in camp_rows {
                let idx = campaigns.get_or_insert(k, || {
                    camp_names.push(k);
                    camp_channels.push(Vec::new());
                    (camp_names.len() - 1) as u32
                });
                camp_channels[idx as usize] = ch.to_vec();
            }
            Ctx {
                ws,
                we,
                cws: ws - LOOKBACK,
                day0,
                ndays,
                products,
                pfx,
                pnum,
                unknown_cat,
                cat_names,
                campaigns,
                camp_names,
                camp_channels,
                nthreads,
            }
        }
    });

    // ---- map inputs
    let map_all = |dir: &str| -> Vec<&'static [u8]> {
        csv_files(&data_dir.join(dir))
            .iter()
            .map(|p| io::map(p).unwrap_or_else(|e| panic!("{}: {e}", p.display())))
            .collect()
    };
    let link_files = map_all("links");
    let event_files = map_all("events");
    let click_files = map_all("clicks");
    let ev_bytes: usize = event_files.iter().map(|f| f.len()).sum();
    let ck_bytes: usize = click_files.iter().map(|f| f.len()).sum();
    // address space for every scatter buffer: EvRec ~0.8x, PEv ~0.5x of the
    // event bytes, PCk ~1x of the click bytes, plus partly filled blocks
    arena::reserve(2 * ev_bytes + 2 * ck_bytes + (nthreads << 30));
    tm.lap("setup");

    // ---- links
    let mut chunks = Vec::new();
    plan_chunks(&link_files, 1 << 20, &mut chunks);
    let link_pairs: Vec<Vec<(Key, Key)>> = parallel(
        nthreads,
        chunks.len(),
        |_| Vec::new(),
        |v: &mut Vec<(Key, Key)>, i| {
            let c = &chunks[i];
            for_lines::<2>(c.data, c.s, c.e, |f| {
                if !f[0].is_empty() && !f[1].is_empty() {
                    v.push((key(f[0]), key(f[1])));
                }
            });
        },
    );
    let ctx = ctx_thread.join().expect("setup thread");
    let ctx = &ctx;
    tm.lap("links");

    // ---- events + clicks parse (and persons built alongside)
    let est_lines = ev_bytes / 70 + 1;
    let ne = (est_lines / 40_000).next_power_of_two().clamp(16, 4096);
    let mut chunks = Vec::new();
    plan_chunks(&event_files, 4 << 20, &mut chunks);
    let n_ev_chunks = chunks.len();
    plan_chunks(&click_files, 4 << 20, &mut chunks);

    let in_files: Vec<&'static [u8]> = link_files.into_iter().chain(event_files).chain(click_files).collect();
    let agg: Vec<(Vec<UdRow>, CampAcc)> = if packed_fits(ctx) {
        parse_dedup_sessions::<u64>(ctx, chunks, n_ev_chunks, link_pairs, in_files, ne, nthreads, &mut tm)
    } else {
        parse_dedup_sessions::<u128>(ctx, chunks, n_ev_chunks, link_pairs, in_files, ne, nthreads, &mut tm)
    };

    // ---- write outputs
    // every scatter buffer is dead now: the write's buffers reuse that
    // (already faulted) memory instead of fresh pages
    let (s, e) = arena::used();
    unsafe { alloc::donate(s, e) };
    let ud_dir = out_dir.join("user_daily");
    let cd_dir = out_dir.join("campaign_daily");
    std::fs::create_dir_all(&ud_dir).expect("create user_daily");
    std::fs::create_dir_all(&cd_dir).expect("create campaign_daily");
    let total_ud: usize = agg.iter().map(|a| a.0.len()).sum();
    let agg_ref = &agg;
    let accs: Vec<&CampAcc> = agg.iter().map(|a| &a.1).collect();
    let accs = &accs;
    let ud_dir_ref = &ud_dir;
    parallel(
        nthreads,
        agg.len() + 1,
        |_| (),
        |_, i| {
            if i == agg_ref.len() {
                write_campaign_daily(ctx, accs, &cd_dir);
                return;
            }
            let rows = &agg_ref[i].0;
            if rows.is_empty() && !(i == 0 && total_ud == 0) {
                return;
            }
            write_user_daily(ctx, rows, &ud_dir_ref.join(format!("part-{i:05}.parquet")));
        },
    );
    log(&format!("  user_daily rows {total_ud}"));
    tm.lap("write");
    if std::env::var_os("SS_DEBUG").is_some() {
        if let Ok(t) = std::fs::read_to_string("/proc/self/smaps_rollup") {
            for l in t.lines().filter(|l| l.starts_with("Rss") || l.contains("Huge") || l.starts_with("Anonymous")) {
                eprintln!("  {l}");
            }
        }
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
        eprintln!("  exit at {} ms", now.as_millis());
    }
    // exit without dropping the big buffers: the kernel reclaims them at once
    std::process::exit(0);
}

/// Parse of events and clicks (persons built alongside), then dedup and
/// sessions, with events packed as `E` from the parse on.
#[allow(clippy::too_many_arguments)]
fn parse_dedup_sessions<E: EvT>(
    ctx: &Ctx,
    chunks: Vec<Chunk>,
    n_ev_chunks: usize,
    link_pairs: Vec<Vec<(Key, Key)>>,
    in_files: Vec<&'static [u8]>,
    ne: usize,
    nthreads: usize,
    tm: &mut Timer,
) -> Vec<(Vec<UdRow>, CampAcc)> {
    let ne_shift = 64 - ne.trailing_zeros();
    let persons_slot: Mutex<Option<Persons>> = Mutex::new(None);
    let parse_out: Vec<ParseOut<E>> = std::thread::scope(|sc| {
        sc.spawn(|| {
            let t = Instant::now();
            let p = build_persons(link_pairs);
            eprintln!(
                "  persons: {} linked ids, built in {:.1} ms",
                p.map.len(),
                t.elapsed().as_secs_f64() * 1e3
            );
            *persons_slot.lock().unwrap() = Some(p);
        });
        parallel(
            nthreads,
            chunks.len(),
            |_| ParseOut { ev: Scatter::new(ne), ck: Vec::new() },
            |st: &mut ParseOut<E>, i| {
                let c = &chunks[i];
                io::populate(c.data, c.s, c.e + 4096);
                if i < n_ev_chunks {
                    for_lines::<7>(c.data, c.s, c.e, |f| parse_event(ctx, f, &mut st.ev, ne_shift));
                } else {
                    for_lines::<4>(c.data, c.s, c.e, |f| parse_click(ctx, f, &mut st.ck));
                }
            },
        )
    });
    // leaked like everything else big: the exit reclaims it
    let persons: &Persons = Box::leak(Box::new(persons_slot.into_inner().unwrap().unwrap()));
    tm.lap("parse");
    // nothing reads the input mappings after the parse (keys are copied), so
    // a helper thread unmaps them now: page-table teardown of ~2 GB of file
    // pages is otherwise ~10 ms of single-threaded work inside exit()
    drop(chunks);
    std::thread::spawn(move || {
        for f in in_files {
            unsafe { io::unmap(f) };
        }
    });

    let agg = dedup_sessions::<E>(ctx, persons, &parse_out, ne, nthreads, tm);
    std::mem::forget(parse_out);
    agg

}

/// Dedup (events by event_id, clicks and events mapped to persons and
/// scattered by person), then sessions per person. `E` is the in-memory event.
fn dedup_sessions<E: EvT>(
    ctx: &Ctx,
    persons: &Persons,
    parse_out: &[ParseOut<E>],
    ne: usize,
    nthreads: usize,
    tm: &mut Timer,
) -> Vec<(Vec<UdRow>, CampAcc)> {
    let ndays = ctx.ndays;
    // ---- dedup events by event_id, map to persons; scatter by person
    let total_ev: usize = parse_out.iter().map(|p| (0..ne).map(|b| p.ev.len(b)).sum::<usize>()).sum();
    let total_ck: usize = parse_out.iter().map(|p| p.ck.len()).sum();
    let np = (total_ev / 20_000).next_power_of_two().clamp(16, 8192);
    let np_shift = 64 - np.trailing_zeros();
    let ck_task = 1 << 16;
    let mut ck_tasks: Vec<(usize, usize)> = Vec::new();
    for (t, p) in parse_out.iter().enumerate() {
        let mut s = 0;
        while s < p.ck.len() {
            ck_tasks.push((t, s));
            s += ck_task;
        }
    }
    struct DedupOut<E: Copy> {
        ev: Scatter<PEv<E>>,
        ck: Scatter<PCk>,
        table: Vec<u64>,
        win: Vec<u64>,
        blks: Vec<*const EvRec<E>>,
        kept: usize,
    }
    unsafe impl<E: Copy> Send for DedupOut<E> {}
    unsafe impl<E: Copy> Sync for DedupOut<E> {}
    let parse_ref = parse_out;
    let dd: Vec<DedupOut<E>> = parallel(
        nthreads,
        ne + ck_tasks.len(),
        |_| DedupOut {
            ev: Scatter::new(np),
            ck: Scatter::new(np),
            table: Vec::new(),
            win: Vec::new(),
            blks: Vec::new(),
            kept: 0,
        },
        |st: &mut DedupOut<E>, task| {
            if task < ne {
                let b = task;
                let n: usize = parse_ref.iter().map(|p| p.ev.len(b)).sum();
                if n == 0 {
                    return;
                }
                let cap = (n * 2).next_power_of_two();
                let mask = cap - 1;
                st.table.clear();
                st.table.resize(cap, 0);
                st.blks.clear();
                let mut lens: Vec<u32> = Vec::new();
                for p in parse_ref {
                    for blk in p.ev.blocks(b) {
                        st.blks.push(blk.as_ptr());
                        lens.push(blk.len() as u32);
                    }
                }
                let blks = &st.blks;
                // ref = block index << 7 | offset (BLOCK = 128)
                let get = |r: u64| -> &EvRec<E> {
                    let r = (r as u32) & 0x7fff_ffff;
                    unsafe { &*blks[(r >> 7) as usize].add((r & 127) as usize) }
                };
                // winners as a bitmap over refs, so survivors are read back in
                // memory order; an invalid winner stays in the table (it still
                // beats later copies) but never gets a bit
                let win = &mut st.win;
                win.clear();
                win.resize(blks.len() * 2, 0);
                for (bi, (&bp, &bl)) in blks.iter().zip(&lens).enumerate() {
                    let blk = unsafe { std::slice::from_raw_parts(bp, bl as usize) };
                    for (i, r) in blk.iter().enumerate() {
                        let eid = r.eid;
                        let w = (bi << 7) | i;
                        let wbit = (({ r.ev } != E::INVALID) as u64) << (w & 63);
                        let hh = hash(eid);
                        let tag = (hh >> 16) & 0xffff_ffff;
                        let ent = (tag << 32) | (1 << 31) | ((bi as u64) << 7) | i as u64;
                        let mut h = (hh as usize) & mask;
                        loop {
                            let s = st.table[h];
                            if s == 0 {
                                st.table[h] = ent;
                                win[w >> 6] |= wbit;
                                break;
                            }
                            if s >> 32 == tag {
                                let o = get(s);
                                if { o.eid } == eid {
                                    if r.ingest < o.ingest {
                                        st.table[h] = ent;
                                        win[w >> 6] |= wbit;
                                        let ow = (s as usize) & 0x7fff_ffff;
                                        win[ow >> 6] &= !(1u64 << (ow & 63));
                                    }
                                    break;
                                }
                            }
                            h = (h + 1) & mask;
                        }
                    }
                }
                // software pipeline: prefetch each survivor's person slot D
                // records before it is looked up (FIFO, so order is kept)
                const D: usize = PF_DIST;
                let mut ring: [(Key, E); D] = [(0, E::default()); D];
                let mut cnt = 0usize;
                let emit = |x: &(Key, E), st_ev: &mut Scatter<PEv<E>>| {
                    let person = persons.name(x.0);
                    let pb = (hash(person) >> np_shift) as usize;
                    st_ev.push(pb, PEv { person, ev: x.1 });
                };
                for (wi, &w0) in st.win.iter().enumerate() {
                    let mut w = w0;
                    while w != 0 {
                        let rf = (wi << 6) | w.trailing_zeros() as usize;
                        w &= w - 1;
                        let r = unsafe { &*blks[rf >> 7].add(rf & 127) };
                        let x = (r.user, r.ev);
                        persons.map.prefetch(x.0);
                        if cnt >= D {
                            emit(&ring[cnt % D], &mut st.ev);
                        }
                        ring[cnt % D] = x;
                        cnt += 1;
                    }
                }
                for i in cnt.saturating_sub(D)..cnt {
                    emit(&ring[i % D], &mut st.ev);
                }
                st.kept += cnt;
                // this task was the only reader of bucket b's records: their
                // memory (faulted in, likely still cached) takes the next blocks
                for p in parse_ref {
                    unsafe { p.ev.recycle_bucket(b, &mut st.ev) };
                }
            } else {
                // event tasks all come first: leftover recycled memory goes to clicks
                st.ev.give_free(&mut st.ck);
                let (t, s) = ck_tasks[task - ne];
                let v = &parse_ref[t].ck;
                let part = &v[s..(s + ck_task).min(v.len())];
                for r in part.iter().take(PF_DIST) {
                    persons.map.prefetch(r.user);
                }
                for (i, r) in part.iter().enumerate() {
                    if let Some(a) = part.get(i + PF_DIST) {
                        persons.map.prefetch(a.user);
                    }
                    let person = persons.name(r.user);
                    let pb = (hash(person) >> np_shift) as usize;
                    st.ck.push(pb, PCk { person, cid: r.cid, ts: r.ts, camp: r.camp });
                }
            }
        },
    );
    let kept: usize = dd.iter().map(|d| d.kept).sum();
    log(&format!(
        "  events parsed {total_ev}, kept {kept}; clicks valid {total_ck}; eid buckets {ne}, person buckets {np}"
    ));
    tm.lap("dedup");
    // the parsed clicks are dead (the caller leaks `parse_out`): their big
    // buffers give the sessions' buffers already-faulted memory
    for p in parse_out {
        let bytes = p.ck.capacity() * std::mem::size_of::<CkRec>();
        if bytes >= 1 << 20 {
            let s = p.ck.as_ptr() as usize;
            unsafe { alloc::donate(s, s + p.ck.len() * std::mem::size_of::<CkRec>()) };
        }
    }

    // ---- per person: sessions, credit, assists, aggregates
    struct AggOut<E> {
        ud: Vec<UdRow>,
        cd: CampAcc,
        local: KeyMap<u32>,
        names: Vec<Key>,
        lids: Vec<u32>,
        cnt: Vec<u32>,
        evs: Vec<E>,
        ck_lids: Vec<u32>,
        ck_cnt: Vec<u32>,
        cks: Vec<E>,
        ck_cid: Vec<Key>,
        ck_camp: Vec<u32>,
        sc: Scratch<E>,
    }
    let dd_ref = &dd;
    let ncamp = ctx.camp_names.len();
    // biggest buckets first, so a whale person's bucket never runs alone at the end
    let mut order: Vec<(usize, u32)> = (0..np)
        .map(|p| (dd_ref.iter().map(|d| d.ev.len(p) + d.ck.len(p)).sum::<usize>(), p as u32))
        .collect();
    order.sort_unstable_by(|a, b| b.cmp(a));
    let order: Vec<usize> = order.into_iter().map(|(_, p)| p as usize).collect();
    let order = &order;
    let agg: Vec<AggOut<E>> = parallel(
        nthreads,
        np,
        |_| AggOut {
            // sized up front (from the donated memory) for ~1 row per 6 kept
            // events, so it rarely grows into fresh pages
            ud: Vec::with_capacity(kept / 6 / nthreads + 1),
            cd: CampAcc::new(ncamp, ndays),
            local: KeyMap::with_capacity(1024),
            names: Vec::new(),
            lids: Vec::new(),
            cnt: Vec::new(),
            evs: Vec::new(),
            ck_lids: Vec::new(),
            ck_cnt: Vec::new(),
            cks: Vec::new(),
            ck_cid: Vec::new(),
            ck_camp: Vec::new(),
            sc: Scratch { sess: Vec::new(), cats: Vec::new(), starts: Vec::new(), run_end: Vec::new(), cp: Vec::new(), stamp: Vec::new(), sid: 0 },
        },
        |st: &mut AggOut<E>, task| {
            let p = order[task];
            let n: usize = dd_ref.iter().map(|d| d.ev.len(p)).sum();
            if n == 0 {
                return;
            }
            st.local.clear_for(n / 32 + 16);
            st.names.clear();
            st.lids.clear();
            st.cnt.clear();
            for d in dd_ref {
                for e in d.ev.blocks(p).flatten() {
                    let person = e.person;
                    let names = &mut st.names;
                    let cnt = &mut st.cnt;
                    let lid = st.local.get_or_insert(person, || {
                        names.push(person);
                        cnt.push(0);
                        (names.len() - 1) as u32
                    });
                    st.cnt[lid as usize] += 1;
                    st.lids.push(lid);
                }
            }
            let np_ = st.names.len();
            // offsets
            let mut off = vec![0u32; np_ + 1];
            for i in 0..np_ {
                off[i + 1] = off[i] + st.cnt[i];
            }
            st.evs.clear();
            st.evs.resize(n, E::default());
            let mut pos: Vec<u32> = off[..np_].to_vec();
            let mut k = 0;
            for d in dd_ref {
                for e in d.ev.blocks(p).flatten() {
                    let lid = st.lids[k] as usize;
                    k += 1;
                    st.evs[pos[lid] as usize] = e.ev;
                    pos[lid] += 1;
                }
            }
            // clicks
            st.ck_lids.clear();
            st.ck_cnt.clear();
            st.ck_cnt.resize(np_, 0);
            let mut nck = 0usize;
            for d in dd_ref {
                for c in d.ck.blocks(p).flatten() {
                    let lid = st.local.get(c.person).unwrap_or(u32::MAX);
                    if lid != u32::MAX {
                        st.ck_cnt[lid as usize] += 1;
                        nck += 1;
                    }
                    st.ck_lids.push(lid);
                }
            }
            let mut coff = vec![0u32; np_ + 1];
            for i in 0..np_ {
                coff[i + 1] = coff[i] + st.ck_cnt[i];
            }
            st.cks.clear();
            st.cks.resize(nck, E::default());
            st.ck_cid.clear();
            st.ck_cid.resize(nck, 0);
            st.ck_camp.clear();
            st.ck_camp.resize(nck, 0);
            let mut cpos: Vec<u32> = coff[..np_].to_vec();
            let mut k = 0;
            for d in dd_ref {
                for c in d.ck.blocks(p).flatten() {
                    let lid = st.ck_lids[k];
                    k += 1;
                    if lid != u32::MAX {
                        let l = lid as usize;
                        let q = cpos[l] as usize;
                        st.cks[q] = E::pair((c.ts - ctx.cws) as u64, q as u64);
                        st.ck_cid[q] = c.cid;
                        st.ck_camp[q] = c.camp;
                        cpos[l] += 1;
                    }
                }
            }
            for l in 0..np_ {
                let evs = &mut st.evs[off[l] as usize..off[l + 1] as usize];
                let cks = Cks {
                    keys: &mut st.cks[coff[l] as usize..coff[l + 1] as usize],
                    cid: &st.ck_cid,
                    camp: &st.ck_camp,
                };
                process_person(ctx, st.names[l], evs, cks, &mut st.sc, &mut st.ud, &mut st.cd);
            }
        },
    );
    tm.lap("sessions");
    agg.into_iter().map(|a| (a.ud, a.cd)).collect()
}

#[inline(always)]
fn parse_event<E: EvT>(ctx: &Ctx, f: &[&[u8]; 7], out: &mut Scatter<EvRec<E>>, shift: u32) {
    if f[0].is_empty() {
        return;
    }
    let Some(ingest) = ts::parse_f1(f[3]) else {
        return;
    };
    let eid = key(f[0]);
    let mut rec = EvRec { eid, user: 0, ingest, ev: E::INVALID };
    'v: {
        let (user, pid) = (f[1], f[5]);
        if user.is_empty() || pid.is_empty() {
            break 'v;
        }
        let Some(t) = ts::parse(f[2]) else {
            break 'v;
        };
        if t < ctx.ws || t >= ctx.we || ingest - t > LATE {
            break 'v;
        }
        let et = f[4];
        let price = if et == b"view" || et == b"cart" {
            0
        } else if et == b"purchase" {
            match ts::parse_price(f[6]) {
                Some(p) => p,
                None => break 'v,
            }
        } else {
            break 'v;
        };
        rec.user = key(user);
        rec.ev = E::make((t - ctx.ws) as u64, product_cat(ctx, pid), price);
    }
    out.push((hash(eid) >> shift) as usize, rec);
}

#[inline(always)]
fn parse_click(ctx: &Ctx, f: &[&[u8]; 4], out: &mut Vec<CkRec>) {
    if f[0].is_empty() || f[1].is_empty() || f[2].is_empty() {
        return;
    }
    let Some(camp) = ctx.campaigns.get(key(f[2])) else {
        return;
    };
    let Some(t) = ts::parse(f[3]) else {
        return;
    };
    if t < ctx.cws || t >= ctx.we {
        return;
    }
    out.push(CkRec { cid: key(f[0]), user: key(f[1]), ts: t, camp });
}

fn write_user_daily(ctx: &Ctx, rows: &[UdRow], path: &Path) {
    use parquet::{Column, Data, Kind};
    let n = rows.len();
    let mut ub = Vec::with_capacity(n * 10);
    let mut ue = Vec::with_capacity(n);
    let mut cb = Vec::with_capacity(n * 4);
    let mut ce = Vec::with_capacity(n);
    let mut present = Vec::with_capacity(n);
    let mut day = Vec::with_capacity(n);
    let mut sess = Vec::with_capacity(n);
    let mut evs = Vec::with_capacity(n);
    let mut rev = Vec::with_capacity(n);
    for r in rows {
        ids::write_bytes(r.name, &mut ub);
        ue.push(ub.len() as u32);
        if r.top != NO_CAT {
            cb.extend_from_slice(&ctx.cat_names[r.top as usize]);
            present.push(true);
        } else {
            present.push(false);
        }
        ce.push(cb.len() as u32);
        day.push(r.day);
        sess.push(r.sessions as i32);
        evs.push(r.events as i32);
        rev.push(r.revenue as i64);
    }
    let cols = [
        Column { name: "user_id", kind: Kind::Str, data: Data::Str { bytes: ub, ends: ue } },
        Column { name: "day", kind: Kind::Date, data: Data::I32(day) },
        Column { name: "sessions", kind: Kind::I32, data: Data::I32(sess) },
        Column { name: "events", kind: Kind::I32, data: Data::I32(evs) },
        Column { name: "revenue_cents", kind: Kind::I64, data: Data::I64(rev) },
        Column { name: "top_category", kind: Kind::OptStr, data: Data::OptStr { bytes: cb, ends: ce, present } },
    ];
    parquet::write_file(path, &cols, n).expect("write user_daily");
}

fn write_campaign_daily(ctx: &Ctx, agg: &[&CampAcc], dir: &Path) {
    use parquet::{Column, Data, Kind};
    let ncamp = ctx.camp_names.len();
    let ndays = ctx.ndays;
    let mut sum = vec![[0u64; 4]; (ncamp + 1) * ndays];
    for a in agg {
        for (s, c) in sum.iter_mut().zip(&a.cells) {
            for k in 0..4 {
                s[k] += c[k];
            }
        }
    }
    let mut ib = Vec::new();
    let mut ie = Vec::new();
    let mut chb = Vec::new();
    let mut che = Vec::new();
    let mut day = Vec::new();
    let mut cols4: [Vec<i64>; 4] = Default::default();
    for camp in 0..=ncamp {
        for d in 0..ndays {
            let c = sum[camp * ndays + d];
            if c[0] == 0 && c[3] == 0 {
                continue;
            }
            if camp == ncamp {
                ib.extend_from_slice(b"organic");
                chb.extend_from_slice(b"organic");
            } else {
                ids::write_bytes(ctx.camp_names[camp], &mut ib);
                chb.extend_from_slice(&ctx.camp_channels[camp]);
            }
            ie.push(ib.len() as u32);
            che.push(chb.len() as u32);
            day.push((ctx.day0 + d as i64) as i32);
            for k in 0..4 {
                cols4[k].push(c[k] as i64);
            }
        }
    }
    let n = day.len();
    let [s, p, r, a] = cols4;
    let cols = [
        Column { name: "campaign_id", kind: Kind::Str, data: Data::Str { bytes: ib, ends: ie } },
        Column { name: "channel", kind: Kind::Str, data: Data::Str { bytes: chb, ends: che } },
        Column { name: "day", kind: Kind::Date, data: Data::I32(day) },
        Column { name: "sessions", kind: Kind::I64, data: Data::I64(s) },
        Column { name: "purchasing_sessions", kind: Kind::I64, data: Data::I64(p) },
        Column { name: "revenue_cents", kind: Kind::I64, data: Data::I64(r) },
        Column { name: "assisted_sessions", kind: Kind::I64, data: Data::I64(a) },
    ];
    parquet::write_file(&dir.join("part-00000.parquet"), &cols, n).expect("write campaign_daily");
}

/// `<pfx><decimal without leading zeros>` (up to 7 digits) -> the number.
#[inline(always)]
fn canonical_num(id: &[u8], pfx: u8) -> Option<usize> {
    let n = id.len();
    if !(2..=8).contains(&n) || id[0] != pfx || id[1] == b'0' {
        return None;
    }
    // SWAR: the 1..=7 digits after the prefix, as one 8-byte word (fields are
    // followed by padding, so reading 8 bytes is safe)
    let m = n - 1;
    let raw = u64::from_le_bytes(unsafe { std::ptr::read_unaligned(id.as_ptr().add(1) as *const [u8; 8]) });
    let keep = (1u64 << (8 * m)) - 1;
    let x = (raw ^ 0x3030_3030_3030_3030) & keep;
    if ((x | x.wrapping_add(0x7676_7676_7676_7676)) & 0x8080_8080_8080_8080 & keep) != 0 {
        return None;
    }
    // right-align the digits (leading zero digits), then combine pairwise
    let y = x << (8 * (8 - m));
    let y = (y & 0x0F0F_0F0F_0F0F_0F0F).wrapping_mul(2561) >> 8;
    let y = (y & 0x00FF_00FF_00FF_00FF).wrapping_mul(6_553_601) >> 16;
    let y = (y & 0x0000_FFFF_0000_FFFF).wrapping_mul(42_949_672_960_001) >> 32;
    Some(y as usize)
}

#[cfg(test)]
fn canonical_num_slow(id: &[u8], pfx: u8) -> Option<usize> {
    let n = id.len();
    if !(2..=8).contains(&n) || id[0] != pfx || id[1] == b'0' {
        return None;
    }
    let mut v = 0usize;
    for &b in &id[1..] {
        let d = b.wrapping_sub(b'0');
        if d > 9 {
            return None;
        }
        v = v * 10 + d as usize;
    }
    Some(v)
}

#[cfg(test)]
mod pid_tests {
    #[test]
    fn swar_matches_loop() {
        let mut cases: Vec<Vec<u8>> = Vec::new();
        for v in [0u64, 1, 7, 9, 10, 42, 99, 100, 12345, 50000, 99999, 123456, 999999, 1234567, 9999999] {
            cases.push(format!("p{v}").into_bytes());
            cases.push(format!("p0{v}").into_bytes());
            cases.push(format!("P{v}").into_bytes());
            cases.push(format!("p{v}x").into_bytes());
            cases.push(format!(" p{v}").into_bytes());
        }
        for odd in ["p", "p١٢", "p１", "p12/", "p12:", "p 1", "p-1", "p1.5", "p12345678", "p1234567"] {
            cases.push(odd.as_bytes().to_vec());
        }
        // every byte value in every position of a 1..=7 digit body
        for len in 1..=7usize {
            for pos in 0..len {
                for b in 0..=255u8 {
                    let mut c = vec![b'p'];
                    c.extend(std::iter::repeat(b'5').take(len));
                    c[1 + pos] = b;
                    cases.push(c);
                }
            }
        }
        for c in cases {
            let mut buf = c.clone();
            buf.extend_from_slice(&[0xAA; 16]); // garbage after the field
            let f = &buf[..c.len()];
            assert_eq!(super::canonical_num(f, b'p'), super::canonical_num_slow(f, b'p'), "{:?}", c);
        }
    }
}

#[inline(always)]
fn product_cat(ctx: &Ctx, pid: &[u8]) -> u32 {
    if let Some(n) = canonical_num(pid, ctx.pfx) {
        if n < ctx.pnum.len() {
            let c = ctx.pnum[n];
            return if c == NO_CAT { ctx.unknown_cat } else { c };
        }
        if n >= (1 << 22) {
            return ctx.products.get(key(pid)).unwrap_or(ctx.unknown_cat);
        }
        // a canonical number beyond every product's: not in the file
        return ctx.unknown_cat;
    }
    ctx.products.get(key(pid)).unwrap_or(ctx.unknown_cat)
}
