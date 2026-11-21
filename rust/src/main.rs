// Shopping Sessions pipeline in Rust. This is the submitted pipeline (root Dockerfile).
// Env: DATA_DIR, OUTPUT_DIR, NCPU, TIMING (stage times on stderr), FORCE_SLOW (use the fallback).
//
// Fast engine: input files are memory-mapped; strings are identified by a 64-bit hash plus an
// independent 32-bit check; matching hashes are verified against the original bytes. Events are scattered by event-id hash for
// dedup, then by person key into partitions; each partition is sorted, sessionized, credited and
// aggregated on its own. Any hash collision or unsupported input
// sends the run to the previous exact engine in slow.rs, so the output never depends on luck.
mod slow;

use ahash::AHashMap as HashMap;
use rayon::prelude::*;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Instant;

const DAY: i64 = 86_400_000;
const GAP: i64 = 1_800_000;
const LOOK: i64 = 7 * DAY;

static mut T0: Option<Instant> = None;
fn lap(name: &str) {
    if std::env::var_os("TIMING").is_some() {
        #[allow(static_mut_refs)]
        let t = unsafe { T0.unwrap() };
        eprintln!("{:8.1} ms  {}", t.elapsed().as_secs_f64() * 1000.0, name);
    }
}

// ------------------------------------------------------------------ files
fn csv_files(dir: &str) -> Vec<String> {
    let mut v: Vec<String> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.extension().map(|x| x == "csv").unwrap_or(false) && p.is_file())
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
        Err(_) => vec![],
    };
    v.sort();
    v
}

/// Memory-map every file (empty files map to nothing).
fn map_all(files: &[String]) -> Vec<Option<memmap2::Mmap>> {
    files
        .par_iter()
        .map(|f| {
            let file = std::fs::File::open(f).unwrap();
            if file.metadata().map(|m| m.len()).unwrap_or(0) == 0 {
                return None;
            }
            Some(unsafe { memmap2::Mmap::map(&file).unwrap() })
        })
        .collect()
}

fn as_slices(maps: &[Option<memmap2::Mmap>]) -> Vec<&[u8]> {
    maps.iter().map(|m| m.as_ref().map(|m| &m[..]).unwrap_or(&[])).collect()
}

/// Byte ranges (data lines only, header skipped) of roughly `target` bytes, split at '\n'.
fn chunks<'a>(bufs: &[&'a [u8]], target: usize) -> Vec<&'a [u8]> {
    let mut out = vec![];
    for &b in bufs {
        let start = match memchr::memchr(b'\n', b) {
            Some(i) => i + 1,
            None => continue,
        };
        let mut s = start;
        while s < b.len() {
            let mut e = (s + target).min(b.len());
            if e < b.len() {
                e = match memchr::memchr(b'\n', &b[e..]) {
                    Some(i) => e + i + 1,
                    None => b.len(),
                };
            }
            out.push(&b[s..e]);
            s = e;
        }
    }
    out
}

/// Iterate lines of a chunk ('\n' only; a trailing '\n' does not start a new line).
#[inline]
fn for_lines<'a>(c: &'a [u8], mut f: impl FnMut(&'a [u8])) {
    let mut s = 0;
    for i in memchr::memchr_iter(b'\n', c) {
        f(&c[s..i]);
        s = i + 1;
    }
    if s < c.len() {
        f(&c[s..]);
    }
}

/// Split a line into exactly N fields, unquoting; None if malformed.
#[inline]
fn fields<'a, const N: usize>(line: &'a [u8]) -> Option<[&'a [u8]; N]> {
    let mut out: [&[u8]; N] = [&[]; N];
    let mut k = 0;
    let mut s = 0;
    let n = line.len();
    let mut i = 0;
    loop {
        while i < n && line[i] != b',' {
            i += 1;
        }
        if k >= N {
            return None;
        }
        let mut f = &line[s..i];
        if memchr::memchr(b'"', f).is_some() {
            if f.len() < 2 || f[0] != b'"' || f[f.len() - 1] != b'"' {
                return None;
            }
            f = &f[1..f.len() - 1];
            if memchr::memchr(b'"', f).is_some() {
                return None;
            }
        }
        out[k] = f;
        k += 1;
        if i >= n {
            break;
        }
        i += 1;
        s = i;
    }
    if k == N && !line.is_empty() { Some(out) } else { None }
}

/// Address ranges of the mapped files, to recover a whole line from a pointer into it.
struct Lines {
    ranges: Vec<(usize, usize)>,
}
impl Lines {
    fn new(bufs: &[&[u8]]) -> Lines {
        let mut ranges: Vec<(usize, usize)> =
            bufs.iter().filter(|b| !b.is_empty()).map(|b| (b.as_ptr() as usize, b.as_ptr() as usize + b.len())).collect();
        ranges.sort();
        Lines { ranges }
    }
    /// The line that starts at `p` (p must be a line start inside a mapped file).
    fn line<'a>(&self, p: u64) -> &'a [u8] {
        let p = p as usize;
        let i = self.ranges.partition_point(|r| r.0 <= p) - 1;
        let end = self.ranges[i].1;
        let s = unsafe { std::slice::from_raw_parts(p as *const u8, end - p) };
        match memchr::memchr(b'\n', s) {
            Some(j) => &s[..j],
            None => s,
        }
    }
}

// ------------------------------------------------------------------ timestamps
#[inline]
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}
#[inline]
fn leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}
#[inline]
fn d2(s: &[u8], i: usize) -> Option<i64> {
    let a = s[i].wrapping_sub(b'0');
    let b = s[i + 1].wrapping_sub(b'0');
    if a < 10 && b < 10 { Some((a * 10 + b) as i64) } else { None }
}
/// ms since epoch, or None if bad. Overlong F3 saturates to i64::MAX (out of any window).
#[inline]
fn parse_ts(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    if s.iter().all(|c| c.is_ascii_digit()) {
        let mut v: i64 = 0;
        for &c in s {
            v = match v.checked_mul(10).and_then(|x| x.checked_add((c - b'0') as i64)) {
                Some(x) => x,
                None => return Some(i64::MAX),
            };
        }
        return Some(v);
    }
    let off = match s.len() {
        20 if s[19] == b'Z' => 0,
        25 if (s[19] == b'+' || s[19] == b'-') && s[22] == b':' => {
            let oh = d2(s, 20)?;
            let om = d2(s, 23)?;
            if oh > 23 || om > 59 {
                return None;
            }
            let o = (oh * 60 + om) * 60_000;
            if s[19] == b'+' { o } else { -o }
        }
        _ => return None,
    };
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let y = d2(s, 0)? * 100 + d2(s, 2)?;
    let mo = d2(s, 5)?;
    let d = d2(s, 8)?;
    let h = d2(s, 11)?;
    let mi = d2(s, 14)?;
    let se = d2(s, 17)?;
    if !(1..=12).contains(&mo) || h > 23 || mi > 59 || se > 59 || d < 1 {
        return None;
    }
    let dim = [31, if leap(y) { 29 } else { 28 }, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31][(mo - 1) as usize];
    if d > dim {
        return None;
    }
    Some((days_from_civil(y, mo, d) * 86400 + h * 3600 + mi * 60 + se) * 1000 - off)
}

fn manifest_ts(m: &str, key: &str) -> i64 {
    let i = m.find(key).unwrap() + key.len();
    let rest = &m[i..];
    let a = rest.find('"').unwrap();
    let rest = &rest[a + 1..];
    let b = rest.find('"').unwrap();
    parse_ts(rest[..b].as_bytes()).unwrap()
}

/// price_cents of a purchase: digits only, 1..=10,000,000.
#[inline]
fn parse_price(p: &[u8]) -> Option<u32> {
    if p.is_empty() || !p.iter().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let z = p.iter().take_while(|&&c| c == b'0').count();
    let p = &p[z..];
    if p.len() > 8 {
        return None;
    }
    let v: u32 = p.iter().fold(0, |a, &c| a * 10 + (c - b'0') as u32);
    if (1..=10_000_000).contains(&v) { Some(v) } else { None }
}

// ------------------------------------------------------------------ hashing
#[inline]
fn mum(a: u64, b: u64) -> u64 {
    let r = (a as u128) * (b as u128);
    (r as u64) ^ ((r >> 64) as u64)
}
#[inline]
fn rd8(b: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(b[i..i + 8].try_into().unwrap())
}
#[inline]
fn rd4(b: &[u8], i: usize) -> u64 {
    u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as u64
}
const K0: u64 = 0xa076_1d64_78bd_642f;
const K1: u64 = 0xe703_7ed1_a0b4_28db;
const K2: u64 = 0x8ebc_6af0_9c88_c6e3;
const K3: u64 = 0x5899_65cc_7537_4cc3;
const K4: u64 = 0x1d8e_4e27_c47d_124f;

/// (64-bit key, independent 32-bit check) of a byte string. For strings up to 16 bytes the
/// (a, c, len) triple is the string itself, so a false match needs both mixes to collide.
#[inline]
fn hs(b: &[u8], seed: u64) -> (u64, u32) {
    let n = b.len();
    let (a, c);
    if n <= 16 {
        if n >= 8 {
            a = rd8(b, 0);
            c = rd8(b, n - 8);
        } else if n >= 4 {
            a = rd4(b, 0);
            c = rd4(b, n - 4);
        } else if n > 0 {
            a = ((b[0] as u64) << 16) | ((b[n >> 1] as u64) << 8) | b[n - 1] as u64;
            c = 0;
        } else {
            a = 0;
            c = 0;
        }
    } else {
        // two independent lanes over 16-byte blocks, then the last 16 bytes
        let mut s1 = seed ^ K0;
        let mut s2 = seed ^ K3;
        let mut i = 0;
        while i + 16 < n {
            let x = rd8(b, i);
            let y = rd8(b, i + 8);
            s1 = mum(x ^ s1 ^ K1, y ^ K2);
            s2 = mum(y ^ s2 ^ K3, x ^ K4);
            i += 16;
        }
        a = s1 ^ rd8(b, n - 16);
        c = s2 ^ rd8(b, n - 8);
    }
    let l = n as u64;
    let key = mum(a ^ seed ^ K0, c ^ seed.rotate_left(17) ^ K1 ^ l);
    let chk = mum(a ^ seed.rotate_left(31) ^ K2, c ^ K3 ^ l.rotate_left(32)) as u32;
    (key, chk)
}

/// Open-addressing map from a 64-bit key (plus check) to a u32 value.
struct Tab {
    slots: Vec<(u64, u64)>, // (key, (chk << 32) | val); key 0 = empty
    mask: usize,
}
impl Tab {
    fn new(n: usize) -> Tab {
        let cap = (n * 2).next_power_of_two().max(16);
        Tab { slots: vec![(0, 0); cap], mask: cap - 1 }
    }
    /// Returns (value, inserted). Err on a collision.
    fn get_or_insert(&mut self, k: u64, c: u32, v: u32) -> Result<(u32, bool), ()> {
        let k = if k == 0 { 1 } else { k };
        let mut i = (k as usize) & self.mask;
        loop {
            let (kk, vv) = self.slots[i];
            if kk == 0 {
                self.slots[i] = (k, ((c as u64) << 32) | v as u64);
                return Ok((v, true));
            }
            if kk == k {
                if (vv >> 32) as u32 != c {
                    return Err(());
                }
                return Ok((vv as u32, false));
            }
            i = (i + 1) & self.mask;
        }
    }
    #[inline]
    fn get(&self, k: u64, c: u32, name: &[u8], ids: &[&[u8]], coll: &AtomicBool) -> Option<u32> {
        let k = if k == 0 { 1 } else { k };
        let mut i = (k as usize) & self.mask;
        loop {
            let (kk, vv) = self.slots[i];
            if kk == 0 {
                return None;
            }
            if kk == k {
                if (vv >> 32) as u32 != c || ids[vv as u32 as usize] != name {
                    coll.store(true, Relaxed);
                    return None;
                }
                return Some(vv as u32);
            }
            i = (i + 1) & self.mask;
        }
    }
}

/// An exact byte identity. Short IDs live directly in the record; long IDs borrow
/// their immutable input mapping. Byte 15 is the inline length, or 255 for a pointer.
/// All borrowed values must be consumed before their mapping/owned buffer is dropped.
#[derive(Clone, Copy)]
struct Id([u8; 16]);
impl Id {
    fn new(bytes: &[u8]) -> Self {
        let mut value = [0; 16];
        if bytes.len() <= 15 {
            value[..bytes.len()].copy_from_slice(bytes);
            value[15] = bytes.len() as u8;
        } else {
            assert!(bytes.len() < (1usize << 56));
            value[..8].copy_from_slice(&(bytes.as_ptr() as u64).to_ne_bytes());
            let length = (bytes.len() as u64).to_le_bytes();
            value[8..15].copy_from_slice(&length[..7]);
            value[15] = 255;
        }
        Self(value)
    }
    fn is_inline(self) -> bool { self.0[15] != 255 }
    fn bytes(&self) -> &[u8] {
        if self.is_inline() { return &self.0[..self.0[15] as usize]; }
        let pointer = u64::from_ne_bytes(self.0[..8].try_into().unwrap());
        let mut length = [0; 8];
        length[..7].copy_from_slice(&self.0[8..15]);
        // The caller retains the input mappings (or owned output name) for this Id.
        unsafe { std::slice::from_raw_parts(pointer as *const u8, u64::from_le_bytes(length) as usize) }
    }
}
impl PartialEq for Id {
    fn eq(&self, other: &Self) -> bool {
        if self.is_inline() || other.is_inline() { self.0 == other.0 }
        else { self.bytes() == other.bytes() }
    }
}
impl Eq for Id {}

// ------------------------------------------------------------------ records
/// One parsed copy of an event, kept until dedup. ts == u32::MAX marks a copy that fails
/// validation (it can still win dedup and so drop the event).
#[derive(Clone, Copy)]
struct EvA {
    hid: u64,
    pkey: u64,
    id: Id,
    name: Id,
    ingest: i64,
    ts: u32,    // ms after window_start; MAX means invalid, still eligible for dedup
    price: u32, // price in low 24 bits, category in high 8 bits
}
/// A surviving event, routed to its person's partition.
#[derive(Clone, Copy)]
struct EvB {
    pkey: u64,
    name: Id,
    ts: u32,
    price: u32,
}
/// A valid click, routed to its person's partition.
#[derive(Clone, Copy)]
struct CkB {
    pkey: u64,
    ts: i64,
    line: u64,
    name: Id,
    camp: u32,
}

const SHB: u32 = 10; // dedup shards by event-id hash
const SH: usize = 1 << SHB;
const PB: u32 = 10; // person partitions
const P: usize = 1 << PB;

struct UdRow {
    name: Id, // inline, or borrowed from the owning worker's boxed name
    day: i32,
    ns: i64,
    ne: i64,
    nr: i64,
    top: u32, // category index, u32::MAX = null
}

fn main() {
    unsafe { T0 = Some(Instant::now()) };
    if std::env::var_os("FORCE_SLOW").is_some() || !fast(0x243f_6a88_85a3_08d3, hs) {
        if std::env::var_os("TIMING").is_some() {
            eprintln!("fast engine declined; running the exact fallback");
        }
        slow::run();
    }
    std::process::exit(0);
}

/// Runs the fast engine. Returns false (having written nothing) if it cannot guarantee exactness.
fn fast(seed: u64, hash: impl Fn(&[u8], u64) -> (u64, u32) + Sync) -> bool {
    let data = std::env::var("DATA_DIR").unwrap_or("/data".into());
    let out = std::env::var("OUTPUT_DIR").unwrap_or("/output".into());
    let ncpu: usize = std::env::var("NCPU").ok().and_then(|s| s.parse().ok()).unwrap_or(8);
    rayon::ThreadPoolBuilder::new().num_threads(ncpu).build_global().ok();

    let man = std::fs::read_to_string(format!("{data}/manifest.json")).unwrap();
    let w0 = manifest_ts(&man, "\"window_start\"");
    let w1 = manifest_ts(&man, "\"window_end\"");
    if w1 <= w0 || w1 - w0 >= u32::MAX as i64 {
        return false;
    }
    let day0 = w0.div_euclid(DAY);
    let ndays = ((w1 - 1).div_euclid(DAY) - day0 + 1) as usize;

    let evf = csv_files(&format!("{data}/events"));
    let ckf = csv_files(&format!("{data}/clicks"));
    let lkf = csv_files(&format!("{data}/links"));
    let evm = map_all(&evf);
    let ckm = map_all(&ckf);
    let lkm = map_all(&lkf);
    let smm = map_all(&[format!("{data}/products.csv"), format!("{data}/campaigns.csv")]);
    let evb = as_slices(&evm);
    let ckb = as_slices(&ckm);
    let lkb = as_slices(&lkm);
    let smb = as_slices(&smm);
    let ck_lines = Lines::new(&ckb);
    lap("mapped files");

    // products / campaigns
    let skip_header = |b: &[u8]| -> usize { memchr::memchr(b'\n', b).map(|i| i + 1).unwrap_or(b.len()) };
    let mut cat_names: Vec<&[u8]> = vec![];
    let mut cat_idx: HashMap<&[u8], u32> = HashMap::new();
    let mut prod: HashMap<&[u8], u32> = HashMap::new();
    for_lines(&smb[0][skip_header(smb[0])..], |l| {
        if let Some([p, c]) = fields::<2>(l) {
            let n = cat_names.len() as u32;
            let ci = *cat_idx.entry(c).or_insert_with(|| {
                cat_names.push(c);
                n
            });
            prod.entry(p).or_insert(ci);
        }
    });
    let unknown = cat_names.len() as u32;
    cat_names.push(b"unknown");
    if cat_names.len() > 255 {
        return false; // category must fit in 8 bits next to the price
    }
    let mut camp_names: Vec<&[u8]> = vec![];
    let mut camp_chan: Vec<&[u8]> = vec![];
    let mut camp: HashMap<&[u8], u32> = HashMap::new();
    for_lines(&smb[1][skip_header(smb[1])..], |l| {
        if let Some([c, ch]) = fields::<2>(l) {
            if !c.is_empty() && !camp.contains_key(c) {
                camp.insert(c, camp_names.len() as u32);
                camp_names.push(c);
                camp_chan.push(ch);
            }
        }
    });
    let ncamp = camp_names.len();

    // links: union-find over hashed ids
    let lch = chunks(&lkb, 1 << 20);
    let edges: Vec<(&[u8], &[u8])> = lch
        .par_iter()
        .flat_map_iter(|c| {
            let mut v = vec![];
            for_lines(c, |l| {
                if let Some([a, b]) = fields::<2>(l) {
                    if !a.is_empty() && !b.is_empty() {
                        v.push((a, b));
                    }
                }
            });
            v
        })
        .collect();
    let mut ltab = Tab::new(edges.len() * 2 + 16);
    let mut ids: Vec<&[u8]> = vec![];
    let mut parent: Vec<u32> = vec![];
    fn find(p: &mut [u32], mut x: u32) -> u32 {
        while p[x as usize] != x {
            let g = p[p[x as usize] as usize];
            p[x as usize] = g;
            x = g;
        }
        x
    }
    for &(a, b) in &edges {
        let mut idx = [0u32; 2];
        for (j, s) in [a, b].into_iter().enumerate() {
            let (k, c) = hash(s, seed);
            match ltab.get_or_insert(k, c, ids.len() as u32) {
                Err(()) => return false,
                Ok((i, ins)) => {
                    if !ins && ids[i as usize] != s { return false; }
                    if ins {
                        ids.push(s);
                        parent.push(i);
                    }
                    idx[j] = i;
                }
            }
        }
        let (ra, rb) = (find(&mut parent, idx[0]), find(&mut parent, idx[1]));
        if ra != rb {
            parent[ra.max(rb) as usize] = ra.min(rb);
        }
    }
    let mut pname: Vec<&[u8]> = vec![];
    let mut root_p: Vec<u32> = vec![u32::MAX; ids.len()];
    let mut link_person: Vec<u32> = vec![0; ids.len()];
    for i in 0..ids.len() {
        let r = find(&mut parent, i as u32) as usize;
        if root_p[r] == u32::MAX {
            root_p[r] = pname.len() as u32;
            pname.push(ids[i]);
        }
        let p = root_p[r];
        if ids[i] < pname[p as usize] {
            pname[p as usize] = ids[i];
        }
        link_person[i] = p;
    }
    // Keep link-table values as exact ID indices, so lookups can verify bytes.
    let pkeys: Vec<(u64, u32)> = pname.iter().map(|n| hash(n, seed)).collect();
    let coll = AtomicBool::new(false);
    // person key of a user id: linked ids share their person's key; others are their own person
    let person = |u: &[u8]| -> (u64, Id) {
        let (k, c) = hash(u, seed);
        let (key, name) = match ltab.get(k, c, u, &ids, &coll) {
            Some(i) => {
                let p = link_person[i as usize] as usize;
                (pkeys[p].0, pname[p])
            }
            None => (k, u),
        };
        (key, Id::new(name))
    };
    lap(&format!("links: {} edges, {} ids, {} persons", edges.len(), ids.len(), pname.len()));

    drop(edges);
    drop(parent);
    drop(root_p);

    // events phase A: parse + validate every copy, scatter by event-id hash
    let ech = chunks(&evb, 1 << 20);
    let accs: Vec<Vec<Vec<EvA>>> = pt(ech.par_iter(),
            || (0..SH).map(|_| Vec::new()).collect::<Vec<Vec<EvA>>>(),
            |mut acc, c| {
                for_lines(c, |l| {
                    let Some([id, user, ts, ing, et, pr, pc]) = fields::<7>(l) else { return };
                    if id.is_empty() {
                        return;
                    }
                    let (hid, _) = hash(id, seed);
                    let ingest = parse_ts(ing).unwrap_or(i64::MAX);
                    let mut r = EvA { hid, pkey: 0, id: Id::new(id), ingest, name: Id::new(b""), ts: u32::MAX, price: 0 };
                    'v: {
                        if user.is_empty() || pr.is_empty() || ingest == i64::MAX {
                            break 'v;
                        }
                        let purchase = match et {
                            b"view" | b"cart" => false,
                            b"purchase" => true,
                            _ => break 'v,
                        };
                        let t = match parse_ts(ts) {
                            Some(t) if t >= w0 && t < w1 => t,
                            _ => break 'v,
                        };
                        if ingest - t > DAY {
                            break 'v;
                        }
                        if purchase {
                            let Some(p) = parse_price(pc) else { break 'v };
                            let cat = *prod.get(pr).unwrap_or(&unknown);
                            r.price = p | (cat << 24);
                        }
                        let (pk, name) = person(user);
                        r.pkey = pk;
                        r.name = name;
                        r.ts = (t - w0) as u32;
                    }
                    acc[(hid >> (64 - SHB)) as usize].push(r);
                });
                acc
            },
        );
    lap(&format!("events parsed ({} folds)", accs.len()));

    // Transfer ownership without copying records. Each consumed shard is freed
    // while surviving records are routed, bounding the overlap of both stages.
    let mut shards: Vec<Vec<Vec<EvA>>> = (0..SH).map(|_| Vec::new()).collect();
    for acc in accs {
        for (shard, rows) in shards.iter_mut().zip(acc) { shard.push(rows); }
    }

    // events phase B: dedup each shard (smallest ingest wins), route survivors by person
    let paccs: Vec<Vec<Vec<EvB>>> = pt(shards.into_par_iter(),
            || ((0..P).map(|_| Vec::new()).collect::<Vec<Vec<EvB>>>(), Vec::<u64>::new(), Vec::<u32>::new(), Vec::<EvA>::new()),
            |(mut out, mut keys, mut slot, mut win), inputs| {
                let n: usize = inputs.iter().map(Vec::len).sum();
                let cap = (n * 2).next_power_of_two().max(16);
                let mask = cap - 1;
                keys.clear();
                keys.resize(cap, 0);
                slot.resize(cap, 0);
                win.clear();
                for a in &inputs {
                    for r in a {
                        let k = if r.hid == 0 { 1 } else { r.hid };
                        let mut i = (k as usize) & mask;
                        loop {
                            let kk = keys[i];
                            if kk == 0 {
                                keys[i] = k;
                                slot[i] = win.len() as u32;
                                win.push(*r);
                                break;
                            }
                            if kk == k {
                                let w = &mut win[slot[i] as usize];
                                if w.id != r.id {
                                    coll.store(true, Relaxed);
                                } else if r.ingest < w.ingest {
                                    *w = *r;
                                }
                                break;
                            }
                            i = (i + 1) & mask;
                        }
                    }
                }
                for w in &win {
                    if w.ts != u32::MAX {
                        out[(w.pkey >> (64 - PB)) as usize].push(EvB {
                            pkey: w.pkey, name: w.name, ts: w.ts, price: w.price,
                        });
                    }
                }
                (out, keys, slot, win)
            },
        )
        .into_iter()
        .map(|x| x.0)
        .collect();
    lap("events dedup + validate");

    // clicks: parse + validate, route by person. Copies of a click are identical, so a duplicate
    // can change neither the chosen click nor the set of assisting campaigns: no dedup needed.
    let cch = chunks(&ckb, 1 << 20);
    let caccs: Vec<Vec<Vec<CkB>>> = pt(cch.par_iter(),
            || (0..P).map(|_| Vec::new()).collect::<Vec<Vec<CkB>>>(),
            |mut acc, c| {
                for_lines(c, |l| {
                    let Some([cid, user, cmp, ts]) = fields::<4>(l) else { return };
                    if cid.is_empty() || user.is_empty() {
                        return;
                    }
                    let Some(&ci) = camp.get(cmp) else { return };
                    let t = match parse_ts(ts) {
                        Some(t) if t >= w0 - LOOK && t < w1 => t,
                        _ => return,
                    };
                    let (pk, name) = person(user);
                    acc[(pk >> (64 - PB)) as usize].push(CkB { pkey: pk, ts: t, line: l.as_ptr() as u64, name, camp: ci });
                });
                acc
            },
        );
    lap("clicks parsed");

    // Consume each partition once. Largest partitions start early, reducing
    // the tail caused by whale users and shared-device connected components.
    let mut jobs: Vec<(Vec<Vec<EvB>>, Vec<Vec<CkB>>)> = (0..P).map(|_| (vec![], vec![])).collect();
    for acc in paccs {
        for (job, rows) in jobs.iter_mut().zip(acc) { job.0.push(rows); }
    }
    for acc in caccs {
        for (job, rows) in jobs.iter_mut().zip(acc) { job.1.push(rows); }
    }
    jobs.sort_unstable_by_key(|(ev, ck)| std::cmp::Reverse(
        ev.iter().map(Vec::len).sum::<usize>() + ck.iter().map(Vec::len).sum::<usize>()));

    // per partition: sort, sessionize, credit, aggregate
    let ncs = ncamp + 1; // index ncamp = organic
    struct St {
        cd: Vec<[i64; 4]>,
        cnt: Vec<u32>,
        inact: Vec<bool>,
        active: Vec<u32>,
        catsum: Vec<i64>,
        touched: Vec<u32>,
        rows: Vec<UdRow>,
        names: Vec<Box<[u8]>>,
        ev: Vec<EvB>,
        ck: Vec<CkB>,
    }
    let ncat = cat_names.len();
    let cid_of = |line: u64| -> &[u8] { fields::<4>(ck_lines.line(line)).unwrap()[0] };
    let states: Vec<St> = pt(jobs.into_par_iter(),
            || St {
                cd: vec![[0; 4]; ncs * ndays],
                cnt: vec![0; ncamp],
                inact: vec![false; ncamp],
                active: vec![],
                catsum: vec![0; ncat],
                touched: vec![],
                rows: vec![],
                names: vec![],
                ev: vec![],
                ck: vec![],
            },
            |mut st, (events, clicks)| {
                st.ev.clear();
                for a in events { st.ev.extend(a); }
                st.ck.clear();
                for a in clicks { st.ck.extend(a); }
                st.ev.sort_unstable_by_key(|e| (e.pkey, e.ts));
                st.ck.sort_unstable_by_key(|c| (c.pkey, c.ts));
                let St { cd, cnt, inact, active, catsum, touched, rows, names, ev, ck } = &mut st;
                let (ev, ck) = (&ev[..], &ck[..]);
                let mut ci = 0;
                let mut i = 0;
                while i < ev.len() {
                    let pk = ev[i].pkey;
                    let person_name = ev[i].name;
                    let mut j = i;
                    while j < ev.len() && ev[j].pkey == pk {
                        if ev[j].name != person_name {
                            coll.store(true, Relaxed);
                        }
                        j += 1;
                    }
                    while ci < ck.len() && ck[ci].pkey < pk {
                        ci += 1;
                    }
                    let c0 = ci;
                    while ci < ck.len() && ck[ci].pkey == pk {
                        if ck[ci].name != person_name {
                            coll.store(true, Relaxed);
                        }
                        ci += 1;
                    }
                    let c1 = ci;
                    // Save the name once per person, before input mappings are released.
                    let saved_name = if person_name.is_inline() { person_name } else {
                        names.push(person_name.bytes().into());
                        Id::new(names.last().unwrap())
                    };
                    // this person's sessions
                    let (mut lo, mut hi) = (c0, c0);
                    let mut row: Option<UdRow> = None;
                    let mut k = i;
                    while k < j {
                        let start = ev[k].ts;
                        let mut m = k + 1;
                        while m < j && ev[m].ts - ev[m - 1].ts <= GAP as u32 {
                            m += 1;
                        }
                        let nev = (m - k) as i64;
                        let sabs = w0 + start as i64;
                        let day = sabs.div_euclid(DAY);
                        let di = (day - day0) as usize;
                        // user_daily: sessions in start order, so days only move forward.
                        // Flush the previous day before this session's purchases are summed.
                        if row.as_ref().map(|r| r.day as i64 != day).unwrap_or(false) {
                            let mut r = row.take().unwrap();
                            r.top = top_cat(catsum, touched, &cat_names);
                            rows.push(r);
                        }
                        let mut rev = 0i64;
                        for e in &ev[k..m] {
                            if e.price != 0 {
                                rev += (e.price & 0xff_ffff) as i64;
                                let c = e.price >> 24;
                                if catsum[c as usize] == 0 {
                                    touched.push(c);
                                }
                                catsum[c as usize] += (e.price & 0xff_ffff) as i64;
                            }
                        }
                        let purch = rev > 0;
                        let r = row.get_or_insert(UdRow { name: saved_name, day: day as i32, ns: 0, ne: 0, nr: 0, top: u32::MAX });
                        r.ns += 1;
                        r.ne += nev;
                        r.nr += rev;
                        // sliding 7-day click window [sabs - LOOK, sabs]
                        while hi < c1 && ck[hi].ts <= sabs {
                            let c = ck[hi].camp as usize;
                            cnt[c] += 1;
                            if !inact[c] {
                                inact[c] = true;
                                active.push(c as u32);
                            }
                            hi += 1;
                        }
                        while lo < hi && ck[lo].ts < sabs - LOOK {
                            cnt[ck[lo].camp as usize] -= 1;
                            lo += 1;
                        }
                        let mut credit = ncamp;
                        if hi > lo {
                            let t = ck[hi - 1].ts;
                            let mut best = hi - 1;
                            let mut x = hi - 1;
                            while x > lo && ck[x - 1].ts == t {
                                x -= 1;
                                if ck[x].line != ck[best].line && cid_of(ck[x].line) < cid_of(ck[best].line) {
                                    best = x;
                                }
                            }
                            credit = ck[best].camp as usize;
                        }
                        let e = &mut cd[credit * ndays + di];
                        e[0] += 1;
                        e[1] += purch as i64;
                        e[2] += rev;
                        active.retain(|&a| {
                            if cnt[a as usize] > 0 {
                                true
                            } else {
                                inact[a as usize] = false;
                                false
                            }
                        });
                        for &a in active.iter() {
                            cd[a as usize * ndays + di][3] += 1;
                        }
                        k = m;
                    }
                    if let Some(mut r) = row.take() {
                        r.top = top_cat(catsum, touched, &cat_names);
                        rows.push(r);
                    }
                    for &a in active.iter() {
                        cnt[a as usize] = 0;
                        inact[a as usize] = false;
                    }
                    for x in &ck[lo..hi] {
                        cnt[x.camp as usize] = 0;
                    }
                    active.clear();
                    i = j;
                }
                st
            },
        );
    // Output rows now refer only to owned names, not to the event/click files.
    drop(evm);
    drop(ckm);
    if coll.load(Relaxed) {
        return false;
    }
    lap("sessions + credit + aggregate");

    // merge campaign_daily
    let mut cd = vec![[0i64; 4]; ncs * ndays];
    for s in &states {
        for (a, b) in cd.iter_mut().zip(&s.cd) {
            for k in 0..4 {
                a[k] += b[k];
            }
        }
    }
    let rows: Vec<&UdRow> = states.iter().flat_map(|s| s.rows.iter()).collect();
    lap(&format!("merged: user_daily {}", rows.len()));

    // write parquet: user_daily in parallel parts, campaign_daily in one file
    use arrow_array::*;
    use arrow_array::builder::StringBuilder;
    use arrow_schema::{DataType, Field, Schema};
    std::fs::create_dir_all(format!("{out}/user_daily")).unwrap();
    std::fs::create_dir_all(format!("{out}/campaign_daily")).unwrap();
    let props = parquet::file::properties::WriterProperties::builder()
        .set_compression(parquet::basic::Compression::UNCOMPRESSED)
        .set_dictionary_enabled(false)
        .build();
    let write = |path: String, batch: RecordBatch| {
        let f = std::fs::File::create(path).unwrap();
        let mut w = parquet::arrow::ArrowWriter::try_new(f, batch.schema(), Some(props.clone())).unwrap();
        w.write(&batch).unwrap();
        w.close().unwrap();
    };
    let s1 = Arc::new(Schema::new(vec![
        Field::new("user_id", DataType::Utf8, false),
        Field::new("day", DataType::Date32, false),
        Field::new("sessions", DataType::Int64, false),
        Field::new("events", DataType::Int64, false),
        Field::new("revenue_cents", DataType::Int64, false),
        Field::new("top_category", DataType::Utf8, true),
    ]));
    let s2 = Arc::new(Schema::new(vec![
        Field::new("campaign_id", DataType::Utf8, false),
        Field::new("channel", DataType::Utf8, false),
        Field::new("day", DataType::Date32, false),
        Field::new("sessions", DataType::Int64, false),
        Field::new("purchasing_sessions", DataType::Int64, false),
        Field::new("revenue_cents", DataType::Int64, false),
        Field::new("assisted_sessions", DataType::Int64, false),
    ]));
    fn st(b: &[u8]) -> &str { unsafe { std::str::from_utf8_unchecked(b) } }
    let nparts = ncpu.max(1);
    let per = rows.len().div_ceil(nparts).max(1);
    let parts: Vec<&[&UdRow]> = rows.chunks(per).collect();
    let user_job = || {
        parts.par_iter().enumerate().for_each(|(pi, part)| {
            let mut uid = StringBuilder::with_capacity(part.len(), part.len() * 12);
            let mut top = StringBuilder::with_capacity(part.len(), part.len() * 8);
            for r in part.iter() {
                uid.append_value(st(r.name.bytes()));
                if r.top == u32::MAX {
                    top.append_null();
                } else {
                    top.append_value(st(cat_names[r.top as usize]));
                }
            }
            let b = RecordBatch::try_new(
                s1.clone(),
                vec![
                    Arc::new(uid.finish()),
                    Arc::new(Date32Array::from(part.iter().map(|r| r.day).collect::<Vec<_>>())),
                    Arc::new(Int64Array::from(part.iter().map(|r| r.ns).collect::<Vec<_>>())),
                    Arc::new(Int64Array::from(part.iter().map(|r| r.ne).collect::<Vec<_>>())),
                    Arc::new(Int64Array::from(part.iter().map(|r| r.nr).collect::<Vec<_>>())),
                    Arc::new(top.finish()),
                ],
            )
            .unwrap();
            write(format!("{out}/user_daily/part-{pi}.parquet"), b);
        });
        if parts.is_empty() {
            write(format!("{out}/user_daily/part-0.parquet"), RecordBatch::new_empty(s1.clone()));
        }
    };
    let camp_job = || {
        let (mut cid, mut cch2, mut cday) = (StringBuilder::new(), StringBuilder::new(), vec![]);
        let mut cv: [Vec<i64>; 4] = Default::default();
        for c in 0..ncs {
            for d in 0..ndays {
                let v = cd[c * ndays + d];
                if v[0] == 0 && v[3] == 0 {
                    continue;
                }
                let (n, ch): (&[u8], &[u8]) = if c == ncamp { (b"organic", b"organic") } else { (camp_names[c], camp_chan[c]) };
                cid.append_value(st(n));
                cch2.append_value(st(ch));
                cday.push((day0 + d as i64) as i32);
                for k in 0..4 {
                    cv[k].push(v[k]);
                }
            }
        }
        let [a, b, c, d] = cv;
        let batch = RecordBatch::try_new(
            s2.clone(),
            vec![
                Arc::new(cid.finish()),
                Arc::new(cch2.finish()),
                Arc::new(Date32Array::from(cday)),
                Arc::new(Int64Array::from(a)),
                Arc::new(Int64Array::from(b)),
                Arc::new(Int64Array::from(c)),
                Arc::new(Int64Array::from(d)),
            ],
        )
        .unwrap();
        write(format!("{out}/campaign_daily/part-0.parquet"), batch);
    };
    rayon::join(user_job, camp_job);
    if coll.load(Relaxed) {
        // a name lookup hit a collision while writing: discard and redo exactly
        let _ = std::fs::remove_dir_all(format!("{out}/user_daily"));
        let _ = std::fs::remove_dir_all(format!("{out}/campaign_daily"));
        return false;
    }
    lap("parquet written");
    true
}

/// Like rayon's fold, but exactly one accumulator per worker thread (fold makes one per split,
/// hundreds here), with dynamic load balancing over the items.
fn pt<I, T: Send>(items: impl ParallelIterator<Item = I>, init: impl Fn() -> T + Sync + Send, f: impl Fn(T, I) -> T + Sync + Send) -> Vec<T> {
    let n = rayon::current_num_threads();
    let slots: Vec<std::sync::Mutex<Option<T>>> = (0..n).map(|_| std::sync::Mutex::new(None)).collect();
    items.for_each(|it| {
        let t = rayon::current_thread_index().unwrap_or(0) % n;
        let mut g = slots[t].lock().unwrap();
        let acc = g.take().unwrap_or_else(&init);
        *g = Some(f(acc, it));
    });
    slots.into_iter().filter_map(|m| m.into_inner().unwrap()).collect()
}

/// Highest summed price; ties to the byte-wise smallest name. Resets the sums.
fn top_cat(catsum: &mut [i64], touched: &mut Vec<u32>, names: &[&[u8]]) -> u32 {
    let mut top = u32::MAX;
    let mut best = -1i64;
    for &c in touched.iter() {
        let v = catsum[c as usize];
        if v > best || (v == best && names[c as usize] < names[top as usize]) {
            best = v;
            top = c;
        }
        catsum[c as usize] = 0;
    }
    touched.clear();
    top
}

#[cfg(test)]
mod tests;
