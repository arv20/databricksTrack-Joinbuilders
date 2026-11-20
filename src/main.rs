mod csv;
mod ids;
mod io;
mod parquet;
mod table;
mod ts;

use ids::{Key, hash, key, key_cmp};
use memchr::memchr;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;
use table::KeyMap;
use ts::DAY;

const GAP: i64 = 30 * ts::MIN;
const LATE: i64 = DAY;
const LOOKBACK: i64 = 7 * DAY;
const INVALID: u32 = u32::MAX;
const NO_CAT: u32 = u32::MAX;

// ------------------------------------------------------------------ records

/// One parsed event line (before dedup). `cat == INVALID` marks a copy that
/// fails validation (or is late); its other payload is unused.
#[repr(C, packed(8))]
#[derive(Clone, Copy)]
struct EvRec {
    eid: Key,
    user: Key,
    ingest: i64,
    ts: i64,
    cat: u32,
    price: u32,
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

/// A surviving event, keyed by person.
#[repr(C, packed(8))]
#[derive(Clone, Copy)]
struct PEv {
    person: Key,
    ts: i64,
    cat: u32,
    price: u32,
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

#[derive(Clone, Copy, Default)]
struct Ev {
    ts: i64,
    cat: u32,
    price: u32,
}

#[derive(Clone, Copy, Default)]
struct Ck {
    ts: i64,
    cid: Key,
    camp: u32,
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

/// Calls `f` on every data line whose first byte lies in `[s, e)` of `data`
/// (the file's first line is its header and is skipped).
#[inline(always)]
fn for_lines(data: &[u8], s: usize, e: usize, mut f: impl FnMut(&[u8])) {
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
    while p < e && p < len {
        let nl = match memchr(b'\n', &data[p..]) {
            Some(i) => p + i,
            None => len,
        };
        f(&data[p..nl]);
        p = nl + 1;
    }
}

fn read_lookup(path: &Path) -> Vec<(Key, Vec<u8>)> {
    let data = io::read_small(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut out = Vec::new();
    for_lines(data, 0, data.len(), |line| {
        let mut f = [&b""[..]; 2];
        if csv::split(line, &mut f) && !f[0].is_empty() {
            out.push((key(f[0]), f[1].to_vec()));
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

struct Scratch {
    sess: Vec<Sess>,
    cats: Vec<(u32, u32)>,
    starts: Vec<i64>,
    run_end: Vec<u32>,
    cp: Vec<(u32, i64)>,
}

#[inline]
fn process_person(
    ctx: &Ctx,
    name: Key,
    evs: &mut [Ev],
    cks: &mut [Ck],
    sc: &mut Scratch,
    ud: &mut Vec<UdRow>,
    cd: &mut CampAcc,
) {
    if evs.len() > 1 {
        evs.sort_unstable_by_key(|e| e.ts);
    }
    let organic = ctx.camp_names.len() as u32;
    // sessions
    sc.sess.clear();
    let mut i = 0usize;
    let n = evs.len();
    while i < n {
        let start = evs[i].ts;
        let lo = i;
        let mut prev = start;
        let mut rev = 0u64;
        while i < n && evs[i].ts - prev <= GAP {
            prev = evs[i].ts;
            rev += evs[i].price as u64;
            i += 1;
        }
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
                    if e.price > 0 {
                        sc.cats.push((e.cat, e.price));
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
        cks.sort_unstable_by(|a, b| a.ts.cmp(&b.ts).then_with(|| key_cmp(a.cid, b.cid)));
    }
    let mut j = 0usize;
    let mut grp = 0usize; // first index of the run of equal ts ending at j-1
    for s in &sc.sess {
        while j < cks.len() && cks[j].ts <= s.start {
            if j == 0 || cks[j].ts != cks[j - 1].ts {
                grp = j;
            }
            j += 1;
        }
        let camp = if j > 0 && s.start - cks[j - 1].ts <= LOOKBACK { cks[grp].camp } else { organic };
        let c = cd.cell(camp, s.day);
        c[0] += 1;
        c[1] += (s.rev > 0) as u64;
        c[2] += s.rev;
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
    sc.cp.extend(cks.iter().map(|c| (c.camp, c.ts)));
    sc.cp.sort_unstable();
    let mut q = 0usize;
    while q < sc.cp.len() {
        let camp = sc.cp[q].0;
        let mut a = sc.cp[q].1;
        let mut b = a + LOOKBACK;
        q += 1;
        loop {
            let more = q < sc.cp.len() && sc.cp[q].0 == camp;
            if more && sc.cp[q].1 <= b.saturating_add(1) {
                b = sc.cp[q].1 + LOOKBACK;
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
            a = sc.cp[q].1;
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
        .min(64);
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

    let prod_rows = read_lookup(&data_dir.join("products.csv"));
    let mut cat_set: HashMap<Vec<u8>, u32> = HashMap::new();
    for (_, c) in &prod_rows {
        cat_set.entry(c.clone()).or_insert(0);
    }
    cat_set.entry(b"unknown".to_vec()).or_insert(0);
    let mut cat_names: Vec<Vec<u8>> = cat_set.keys().cloned().collect();
    cat_names.sort();
    for (i, c) in cat_names.iter().enumerate() {
        cat_set.insert(c.clone(), i as u32);
    }
    let unknown_cat = cat_set[&b"unknown"[..]];
    let mut products = KeyMap::with_capacity(prod_rows.len());
    for (k, c) in &prod_rows {
        products.insert(*k, cat_set[c]);
    }
    let camp_rows = read_lookup(&data_dir.join("campaigns.csv"));
    let mut campaigns: KeyMap<u32> = KeyMap::with_capacity(camp_rows.len());
    let mut camp_names = Vec::new();
    let mut camp_channels: Vec<Vec<u8>> = Vec::new();
    for (k, ch) in camp_rows {
        let idx = campaigns.get_or_insert(k, || {
            camp_names.push(k);
            camp_channels.push(Vec::new());
            (camp_names.len() - 1) as u32
        });
        camp_channels[idx as usize] = ch;
    }
    let ctx = Ctx {
        ws,
        we,
        cws: ws - LOOKBACK,
        day0,
        ndays,
        products,
        unknown_cat,
        cat_names,
        campaigns,
        camp_names,
        camp_channels,
        nthreads,
    };
    let ctx = &ctx;

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
    tm.lap("setup+map");

    // ---- links
    let mut chunks = Vec::new();
    plan_chunks(&link_files, 1 << 20, &mut chunks);
    let link_pairs: Vec<Vec<(Key, Key)>> = parallel(
        nthreads,
        chunks.len(),
        |_| Vec::new(),
        |v: &mut Vec<(Key, Key)>, i| {
            let c = &chunks[i];
            for_lines(c.data, c.s, c.e, |line| {
                let mut f = [&b""[..]; 2];
                if csv::split(line, &mut f) && !f[0].is_empty() && !f[1].is_empty() {
                    v.push((key(f[0]), key(f[1])));
                }
            });
        },
    );
    tm.lap("links parse");

    // ---- events + clicks parse (and persons built alongside)
    let est_lines = ev_bytes / 70 + 1;
    let ne = (est_lines / 40_000).next_power_of_two().clamp(16, 4096);
    let ne_shift = 64 - ne.trailing_zeros();
    let mut chunks = Vec::new();
    plan_chunks(&event_files, 4 << 20, &mut chunks);
    let n_ev_chunks = chunks.len();
    plan_chunks(&click_files, 4 << 20, &mut chunks);

    struct ParseOut {
        ev: Vec<Vec<EvRec>>,
        ck: Vec<CkRec>,
    }
    let persons_slot: Mutex<Option<Persons>> = Mutex::new(None);
    let parse_out: Vec<ParseOut> = std::thread::scope(|sc| {
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
            |_| ParseOut { ev: (0..ne).map(|_| Vec::new()).collect(), ck: Vec::new() },
            |st: &mut ParseOut, i| {
                let c = &chunks[i];
                if i < n_ev_chunks {
                    for_lines(c.data, c.s, c.e, |line| parse_event(ctx, line, &mut st.ev, ne_shift));
                } else {
                    for_lines(c.data, c.s, c.e, |line| parse_click(ctx, line, &mut st.ck));
                }
            },
        )
    });
    let persons = persons_slot.into_inner().unwrap().unwrap();
    let persons = &persons;
    tm.lap("events+clicks parse");

    // ---- dedup events by event_id, map to persons; scatter by person
    let total_ev: usize = parse_out.iter().map(|p| p.ev.iter().map(|b| b.len()).sum::<usize>()).sum();
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
    struct DedupOut {
        ev: Vec<Vec<PEv>>,
        ck: Vec<Vec<PCk>>,
        table: Vec<u64>,
        kept: usize,
    }
    let parse_ref = &parse_out;
    let dd: Vec<DedupOut> = parallel(
        nthreads,
        ne + ck_tasks.len(),
        |_| DedupOut {
            ev: (0..np).map(|_| Vec::new()).collect(),
            ck: (0..np).map(|_| Vec::new()).collect(),
            table: Vec::new(),
            kept: 0,
        },
        |st: &mut DedupOut, task| {
            if task < ne {
                let b = task;
                let n: usize = parse_ref.iter().map(|p| p.ev[b].len()).sum();
                if n == 0 {
                    return;
                }
                let cap = (n * 2).next_power_of_two();
                let mask = cap - 1;
                st.table.clear();
                st.table.resize(cap, u64::MAX);
                let get = |r: u64| -> &EvRec { &parse_ref[(r >> 32) as usize].ev[b][(r & 0xffff_ffff) as usize] };
                for (t, p) in parse_ref.iter().enumerate() {
                    for (i, r) in p.ev[b].iter().enumerate() {
                        let eid = r.eid;
                        let mut h = (hash(eid) as usize) & mask;
                        loop {
                            let s = st.table[h];
                            if s == u64::MAX {
                                st.table[h] = ((t as u64) << 32) | i as u64;
                                break;
                            }
                            let o = get(s);
                            if { o.eid } == eid {
                                if r.ingest < o.ingest {
                                    st.table[h] = ((t as u64) << 32) | i as u64;
                                }
                                break;
                            }
                            h = (h + 1) & mask;
                        }
                    }
                }
                for &s in st.table.iter() {
                    if s == u64::MAX {
                        continue;
                    }
                    let r = get(s);
                    if r.cat == INVALID {
                        continue;
                    }
                    let person = persons.name(r.user);
                    let pb = (hash(person) >> np_shift) as usize;
                    st.ev[pb].push(PEv { person, ts: r.ts, cat: r.cat, price: r.price });
                    st.kept += 1;
                }
            } else {
                let (t, s) = ck_tasks[task - ne];
                let v = &parse_ref[t].ck;
                for r in &v[s..(s + ck_task).min(v.len())] {
                    let person = persons.name(r.user);
                    let pb = (hash(person) >> np_shift) as usize;
                    st.ck[pb].push(PCk { person, cid: r.cid, ts: r.ts, camp: r.camp });
                }
            }
        },
    );
    let kept: usize = dd.iter().map(|d| d.kept).sum();
    log(&format!(
        "  events parsed {total_ev}, kept {kept}; clicks valid {total_ck}; eid buckets {ne}, person buckets {np}"
    ));
    tm.lap("dedup+person map");

    // ---- per person: sessions, credit, assists, aggregates
    struct AggOut {
        ud: Vec<UdRow>,
        cd: CampAcc,
        local: KeyMap<u32>,
        names: Vec<Key>,
        lids: Vec<u32>,
        cnt: Vec<u32>,
        evs: Vec<Ev>,
        ck_lids: Vec<u32>,
        ck_cnt: Vec<u32>,
        cks: Vec<Ck>,
        sc: Scratch,
    }
    let dd_ref = &dd;
    let ncamp = ctx.camp_names.len();
    let agg: Vec<AggOut> = parallel(
        nthreads,
        np,
        |_| AggOut {
            ud: Vec::new(),
            cd: CampAcc::new(ncamp, ndays),
            local: KeyMap::with_capacity(1024),
            names: Vec::new(),
            lids: Vec::new(),
            cnt: Vec::new(),
            evs: Vec::new(),
            ck_lids: Vec::new(),
            ck_cnt: Vec::new(),
            cks: Vec::new(),
            sc: Scratch { sess: Vec::new(), cats: Vec::new(), starts: Vec::new(), run_end: Vec::new(), cp: Vec::new() },
        },
        |st: &mut AggOut, p| {
            let n: usize = dd_ref.iter().map(|d| d.ev[p].len()).sum();
            if n == 0 {
                return;
            }
            st.local.clear_for(n / 4 + 16);
            st.names.clear();
            st.lids.clear();
            st.cnt.clear();
            for d in dd_ref {
                for e in &d.ev[p] {
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
            st.evs.resize(n, Ev::default());
            let mut pos: Vec<u32> = off[..np_].to_vec();
            let mut k = 0;
            for d in dd_ref {
                for e in &d.ev[p] {
                    let lid = st.lids[k] as usize;
                    k += 1;
                    st.evs[pos[lid] as usize] = Ev { ts: e.ts, cat: e.cat, price: e.price };
                    pos[lid] += 1;
                }
            }
            // clicks
            st.ck_lids.clear();
            st.ck_cnt.clear();
            st.ck_cnt.resize(np_, 0);
            let mut nck = 0usize;
            for d in dd_ref {
                for c in &d.ck[p] {
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
            st.cks.resize(nck, Ck::default());
            let mut cpos: Vec<u32> = coff[..np_].to_vec();
            let mut k = 0;
            for d in dd_ref {
                for c in &d.ck[p] {
                    let lid = st.ck_lids[k];
                    k += 1;
                    if lid != u32::MAX {
                        let l = lid as usize;
                        st.cks[cpos[l] as usize] = Ck { ts: c.ts, cid: c.cid, camp: c.camp };
                        cpos[l] += 1;
                    }
                }
            }
            for l in 0..np_ {
                let evs = &mut st.evs[off[l] as usize..off[l + 1] as usize];
                let cks = &mut st.cks[coff[l] as usize..coff[l + 1] as usize];
                process_person(ctx, st.names[l], evs, cks, &mut st.sc, &mut st.ud, &mut st.cd);
            }
        },
    );
    tm.lap("sessions+attribution");

    // ---- write outputs
    let ud_dir = out_dir.join("user_daily");
    let cd_dir = out_dir.join("campaign_daily");
    std::fs::create_dir_all(&ud_dir).expect("create user_daily");
    std::fs::create_dir_all(&cd_dir).expect("create campaign_daily");
    let total_ud: usize = agg.iter().map(|a| a.ud.len()).sum();
    let agg_ref = &agg;
    let accs: Vec<&CampAcc> = agg.iter().map(|a| &a.cd).collect();
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
            let rows = &agg_ref[i].ud;
            if rows.is_empty() && !(i == 0 && total_ud == 0) {
                return;
            }
            write_user_daily(ctx, rows, &ud_dir_ref.join(format!("part-{i:05}.parquet")));
        },
    );
    log(&format!("  user_daily rows {total_ud}"));
    tm.lap("write");
}

#[inline(always)]
fn parse_event(ctx: &Ctx, line: &[u8], out: &mut [Vec<EvRec>], shift: u32) {
    let mut f = [&b""[..]; 7];
    if !csv::split(line, &mut f) || f[0].is_empty() {
        return;
    }
    let Some(ingest) = ts::parse_f1(f[3]) else {
        return;
    };
    let eid = key(f[0]);
    let mut rec = EvRec { eid, user: 0, ingest, ts: 0, cat: INVALID, price: 0 };
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
        rec.ts = t;
        rec.price = price;
        rec.cat = ctx.products.get(key(pid)).unwrap_or(ctx.unknown_cat);
    }
    out[(hash(eid) >> shift) as usize].push(rec);
}

#[inline(always)]
fn parse_click(ctx: &Ctx, line: &[u8], out: &mut Vec<CkRec>) {
    let mut f = [&b""[..]; 4];
    if !csv::split(line, &mut f) || f[0].is_empty() || f[1].is_empty() || f[2].is_empty() {
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
