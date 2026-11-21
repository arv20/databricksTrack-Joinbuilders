#![allow(dead_code, unused_imports, unused_unsafe, unused_mut, unused_variables, unsafe_op_in_unsafe_fn, unexpected_cfgs)]
// probe: measures the machine it runs on and prints "key = value unit" lines to stderr.

use std::arch::asm;
use std::arch::x86_64::*;
use std::hint::black_box;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::OnceLock;

// ------------------------------------------------------------------ libc
unsafe extern "C" {
    fn write(fd: i32, buf: *const u8, n: usize) -> isize;
    fn read(fd: i32, buf: *mut u8, n: usize) -> isize;
    fn open(path: *const i8, flags: i32, ...) -> i32;
    fn close(fd: i32) -> i32;
    fn pread(fd: i32, buf: *mut u8, n: usize, off: i64) -> isize;
    fn mkdir(path: *const u8, mode: u32) -> i32;
    fn unlink(path: *const u8) -> i32;
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn munmap(addr: *mut u8, len: usize) -> i32;
    fn madvise(addr: *mut u8, len: usize, adv: i32) -> i32;
    fn mincore(addr: *mut u8, len: usize, vec: *mut u8) -> i32;
    fn mlock(addr: *const u8, len: usize) -> i32;
    fn munlock(addr: *const u8, len: usize) -> i32;
    fn sched_setaffinity(pid: i32, sz: usize, mask: *const u64) -> i32;
    fn sched_getaffinity(pid: i32, sz: usize, mask: *mut u64) -> i32;
    fn sched_getcpu() -> i32;
    fn clock_gettime(clk: i32, ts: *mut [i64; 2]) -> i32;
    fn clock_getres(clk: i32, ts: *mut [i64; 2]) -> i32;
    fn getrlimit(res: i32, rl: *mut [u64; 2]) -> i32;
    fn getrusage(who: i32, ru: *mut [i64; 18]) -> i32;
    fn syscall(n: i64, ...) -> i64;
    fn _exit(code: i32) -> !;
    fn signal(sig: i32, h: usize) -> usize;
    fn getuid() -> u32;
    fn getgid() -> u32;
    fn stat(path: *const u8, st: *mut [u64; 18]) -> i32;
}
const PROT_READ: i32 = 1;
const PROT_WRITE: i32 = 2;
const MAP_SHARED: i32 = 1;
const MAP_PRIVATE: i32 = 2;
const MAP_ANON: i32 = 0x20;
const MAP_NORESERVE: i32 = 0x4000;
const MAP_HUGETLB: i32 = 0x40000;
const MADV_DONTNEED: i32 = 4;
const MADV_HUGEPAGE: i32 = 14;
const MADV_NOHUGEPAGE: i32 = 15;
const MB: usize = 1 << 20;
const MB2: usize = 2 << 20;
const GB: usize = 1 << 30;

fn en() -> i32 { std::io::Error::last_os_error().raw_os_error().unwrap_or(-1) }
fn cs(s: &str) -> Vec<u8> { let mut v = s.as_bytes().to_vec(); v.push(0); v }
fn now() -> u64 { let mut t = [0i64; 2]; unsafe { clock_gettime(1, &mut t) }; (t[0] as u64) * 1_000_000_000 + t[1] as u64 }
static START: AtomicU64 = AtomicU64::new(0);
fn el_s() -> f64 { (now().saturating_sub(START.load(Relaxed))) as f64 / 1e9 }
fn ms(ns: u64) -> f64 { ns as f64 / 1e6 }
fn rusage() -> [i64; 18] { let mut r = [0i64; 18]; unsafe { getrusage(0, &mut r) }; r }
fn minflt() -> i64 { rusage()[8] }

// ------------------------------------------------------------------ log
static NL: AtomicUsize = AtomicUsize::new(0);
static NB: AtomicUsize = AtomicUsize::new(0);
static TRUNC: AtomicBool = AtomicBool::new(false);
fn raw(b: &[u8]) {
    let mut o = 0;
    while o < b.len() { let r = unsafe { write(2, b.as_ptr().add(o), b.len() - o) }; if r <= 0 { break; } o += r as usize; }
}
fn raw_num(mut x: i64) {
    let mut b = [0u8; 24]; let mut i = 24; let neg = x < 0; if neg { x = -x; }
    loop { i -= 1; b[i] = b'0' + (x % 10) as u8; x /= 10; if x == 0 { break; } }
    if neg { i -= 1; b[i] = b'-'; }
    raw(&b[i..]);
}
pub fn emit(line: &str) {
    let mut s: String = line.chars().take(420).map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    s.push('\n');
    let n = NL.fetch_add(1, Relaxed);
    let b = NB.fetch_add(s.len(), Relaxed);
    if n >= 288 || b + s.len() > 29000 {
        if !TRUNC.swap(true, Relaxed) { raw(b"log = TRUNCATED line or byte budget\n"); }
        return;
    }
    raw(s.as_bytes());
}
macro_rules! kv { ($k:expr, $($a:tt)*) => { crate::emit(&format!("{} = {}", $k, format!($($a)*))) }; }

// ------------------------------------------------------------------ steps, crash handler, watchdog, tables
const STEPS: [&str; 16] = ["init", "B", "A", "J", "C", "D", "E", "F", "G", "H", "I", "tables", "K", "end", "watchdog", "-"];
static STEP: AtomicUsize = AtomicUsize::new(0);
fn step(i: usize) { STEP.store(i, Relaxed); }
struct Tables { dirs: [Vec<u8>; 2], files: [Vec<u8>; 2], data: [Vec<u8>; 2] }
static TABLES: OnceLock<Tables> = OnceLock::new();
static TABLES_OK: AtomicBool = AtomicBool::new(false);

fn build_tables(out: &str) {
    let t = Tables {
        dirs: [cs(&format!("{out}/user_daily")), cs(&format!("{out}/campaign_daily"))],
        files: [cs(&format!("{out}/user_daily/part-0.parquet")), cs(&format!("{out}/campaign_daily/part-0.parquet"))],
        data: [include_bytes!("u.bin").to_vec(), include_bytes!("c.bin").to_vec()],
    };
    let _ = TABLES.set(t);
}
/// Only raw syscalls: safe to call from the signal handler.
fn write_tables_raw() -> [i32; 2] {
    let mut res = [0i32; 2];
    let Some(t) = TABLES.get() else { return [-1, -1] };
    for i in 0..2 {
        unsafe {
            mkdir(t.dirs[i].as_ptr(), 0o755);
            let fd = open(t.files[i].as_ptr() as *const i8, 0o1 | 0o100 | 0o1000, 0o644);
            if fd < 0 { res[i] = en(); continue; }
            let d = &t.data[i]; let mut o = 0;
            while o < d.len() { let r = write(fd, d.as_ptr().add(o), d.len() - o); if r <= 0 { res[i] = en(); break; } o += r as usize; }
            close(fd);
        }
    }
    if res == [0, 0] { TABLES_OK.store(true, Relaxed); }
    res
}
extern "C" fn on_sig(sig: i32) {
    raw(b"crash = signal "); raw_num(sig as i64); raw(b" in step "); raw(STEPS[STEP.load(Relaxed).min(15)].as_bytes()); raw(b"\n");
    if !TABLES_OK.load(Relaxed) { let r = write_tables_raw(); raw(b"crash.tables = "); raw_num(r[0] as i64); raw(b" "); raw_num(r[1] as i64); raw(b" errno\n"); }
    unsafe { _exit(0) }
}
fn watchdog(limit_s: f64) {
    let _ = std::thread::Builder::new().stack_size(256 << 10).spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        if el_s() > limit_s {
            raw(b"watchdog = fired in step "); raw(STEPS[STEP.load(Relaxed).min(15)].as_bytes()); raw(b"\n");
            if !TABLES_OK.load(Relaxed) { let r = write_tables_raw(); raw(b"watchdog.tables = "); raw_num(r[0] as i64); raw(b" "); raw_num(r[1] as i64); raw(b" errno\n"); }
            unsafe { _exit(0) }
        }
    });
}
fn budget(name: &str, limit: f64) -> bool {
    let e = el_s();
    if e > limit { kv!(format!("{name}.skip"), "budget at {:.1} s", e); false } else { true }
}

// ------------------------------------------------------------------ files
fn rd(p: &str) -> Result<String, i32> { std::fs::read_to_string(p).map_err(|e| e.raw_os_error().unwrap_or(-1)) }
fn rdl(p: &str) -> String { match rd(p) { Ok(s) => s.trim().to_string(), Err(e) => format!("ERR {e}") } }
fn parse_list(s: &str) -> Vec<usize> {
    let mut v = vec![];
    for part in s.trim().split(',') {
        let part = part.trim(); if part.is_empty() { continue; }
        if let Some((a, b)) = part.split_once('-') {
            if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) { for x in a..=b.min(a + 4096) { v.push(x); } }
        } else if let Ok(a) = part.parse::<usize>() { v.push(a); }
    }
    v
}
fn list_str(v: &[usize]) -> String { v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",") }

// ------------------------------------------------------------------ memory
/// anonymous region of len bytes, 2 MB aligned; thp 0 default, 1 MADV_HUGEPAGE, 2 MADV_NOHUGEPAGE. null on failure.
struct Anon { raw: *mut u8, raw_len: usize, p: *mut u8, len: usize }
unsafe impl Send for Anon {} unsafe impl Sync for Anon {}
impl Anon {
    fn new(len: usize, thp: i32) -> Option<Anon> {
        let la = (len + MB2 - 1) & !(MB2 - 1);
        let raw_len = la + MB2;
        let raw = unsafe { mmap(std::ptr::null_mut(), raw_len, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_NORESERVE, -1, 0) };
        if raw as isize == -1 { return None; }
        let p = (((raw as usize) + MB2 - 1) & !(MB2 - 1)) as *mut u8;
        match thp { 1 => unsafe { madvise(p, la, MADV_HUGEPAGE); }, 2 => unsafe { madvise(p, la, MADV_NOHUGEPAGE); }, _ => {} }
        Some(Anon { raw, raw_len, p, len })
    }
    fn free(self) -> f64 { let t = now(); unsafe { munmap(self.raw, self.raw_len) }; ms(now() - t) }
}
#[derive(Clone, Copy)] struct Ptr(*mut u8);
unsafe impl Send for Ptr {} unsafe impl Sync for Ptr {}
impl Ptr { #[inline(always)] fn g(&self) -> *mut u8 { self.0 } }

// ------------------------------------------------------------------ threads
fn pin(cpu: usize) -> i32 {
    let mut m = [0u64; 16]; if cpu >= 1024 { return -1; }
    m[cpu / 64] |= 1u64 << (cpu % 64);
    unsafe { sched_setaffinity(0, 128, m.as_ptr()) }
}
fn getaff() -> Vec<usize> {
    let mut m = [0u64; 16];
    let r = unsafe { sched_getaffinity(0, 128, m.as_mut_ptr()) };
    if r < 0 { return vec![]; }
    let mut v = vec![]; for i in 0..1024 { if m[i / 64] >> (i % 64) & 1 == 1 { v.push(i); } } v
}
/// Runs f(i) for i in 0..n on n threads (thread i pinned to cpus[i % len] when cpus is non-empty).
/// All threads start together; returns the results and the wall time from start to the last finish (s).
fn par<R: Send + Default, F: Fn(usize) -> R + Sync>(n: usize, cpus: &[usize], f: F) -> (Vec<R>, f64) {
    let ready = AtomicUsize::new(0); let go = AtomicBool::new(false);
    let mut res: Vec<R> = Vec::new(); let mut tmax = 0u64; let mut t0 = 0u64;
    std::thread::scope(|s| {
        let mut hs = vec![]; let mut failed = vec![];
        for i in 0..n {
            let (f, ready, go) = (&f, &ready, &go);
            let cpu = if cpus.is_empty() { None } else { Some(cpus[i % cpus.len()]) };
            match std::thread::Builder::new().stack_size(4 << 20).spawn_scoped(s, move || {
                if let Some(c) = cpu { pin(c); }
                ready.fetch_add(1, Release);
                while !go.load(Acquire) { std::hint::spin_loop(); }
                let r = f(i); (r, now())
            }) { Ok(h) => hs.push((i, h)), Err(_) => failed.push(i) }
        }
        while ready.load(Acquire) < hs.len() { std::hint::spin_loop(); }
        t0 = now(); go.store(true, Release);
        let mut out: Vec<Option<R>> = (0..n).map(|_| None).collect();
        for &i in &failed { out[i] = Some(f(i)); tmax = tmax.max(now()); }
        for (i, h) in hs { if let Ok((r, e)) = h.join() { out[i] = Some(r); tmax = tmax.max(e); } }
        if !failed.is_empty() { emit(&format!("par.spawn_failed = {} threads", failed.len())); }
        res = out.into_iter().map(|x| x.unwrap_or_default()).collect();
    });
    (res, (tmax.saturating_sub(t0)) as f64 / 1e9)
}
struct SpinBarrier { n: usize, count: AtomicUsize, gn: AtomicUsize }
impl SpinBarrier {
    fn new(n: usize) -> Self { SpinBarrier { n, count: AtomicUsize::new(0), gn: AtomicUsize::new(0) } }
    fn wait(&self) {
        let g = self.gn.load(Acquire);
        if self.count.fetch_add(1, AcqRel) + 1 == self.n { self.count.store(0, Relaxed); self.gn.fetch_add(1, Release); }
        else { let t = now(); while self.gn.load(Acquire) == g { std::hint::spin_loop(); if now() - t > 5_000_000_000 { break; } } }
    }
}
fn median(v: &mut Vec<f64>) -> f64 { if v.is_empty() { return f64::NAN; } v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal)); v[v.len() / 2] }
fn minf(v: &[f64]) -> f64 { v.iter().cloned().fold(f64::INFINITY, f64::min) }

#[inline(always)] fn splitmix(mut x: u64) -> u64 { x = x.wrapping_add(0x9E3779B97F4A7C15); x = (x ^ (x >> 30)).wrapping_mul(0xBF58476D1CE4E5B9); x = (x ^ (x >> 27)).wrapping_mul(0x94D049BB133111EB); x ^ (x >> 31) }

#[inline(never)]
unsafe fn count256(p: *const u8, n: usize) -> u64 {
    let nl = _mm256_set1_epi8(10); let mut c = 0u64; let mut i = 0usize;
    while i + 128 <= n {
        let a = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(i).cast()), nl)) as u32;
        let b = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(i + 32).cast()), nl)) as u32;
        let d = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(i + 64).cast()), nl)) as u32;
        let e = _mm256_movemask_epi8(_mm256_cmpeq_epi8(_mm256_loadu_si256(p.add(i + 96).cast()), nl)) as u32;
        c += (a.count_ones() + b.count_ones() + d.count_ones() + e.count_ones()) as u64; i += 128;
    }
    while i < n { c += (*p.add(i) == 10) as u64; i += 1; }
    c
}

// ------------------------------------------------------------------ topology
struct Topo { allowed: Vec<usize>, cores: Vec<Vec<usize>>, cpus8: Vec<usize>, cpus4: Vec<usize>, source: &'static str }
fn topo() -> Topo {
    let allowed = getaff();
    let mut seen = std::collections::HashSet::new(); let mut cores: Vec<Vec<usize>> = vec![];
    for &c in &allowed {
        if seen.contains(&c) { continue; }
        let sib = parse_list(&rdl(&format!("/sys/devices/system/cpu/cpu{c}/topology/thread_siblings_list")));
        let mut g: Vec<usize> = sib.into_iter().filter(|x| allowed.contains(x)).collect();
        if !g.contains(&c) { g = vec![c]; }
        for &x in &g { seen.insert(x); }
        cores.push(g);
    }
    let mut source = "sysfs";
    if allowed.len() > 1 && cores.iter().all(|g| g.len() == 1) {
        // fallback: group by (physical id, core id) from /proc/cpuinfo
        let ci = rd("/proc/cpuinfo").unwrap_or_default();
        let mut key: Vec<(usize, String)> = vec![];
        for blk in ci.split("\n\n") {
            let g = |k: &str| blk.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_once(':')).map(|x| x.1.trim().to_string());
            if let (Some(p), Some(pid), Some(cid)) = (g("processor"), g("physical id"), g("core id")) { if let Ok(p) = p.parse::<usize>() { key.push((p, format!("{pid}/{cid}"))); } }
        }
        if !key.is_empty() {
            let mut gs: Vec<(String, Vec<usize>)> = vec![];
            for &c in &allowed { if let Some((_, k)) = key.iter().find(|x| x.0 == c) { if let Some(e) = gs.iter_mut().find(|e| &e.0 == k) { e.1.push(c); } else { gs.push((k.clone(), vec![c])); } } else { gs.push((format!("?{c}"), vec![c])); } }
            if gs.iter().any(|g| g.1.len() > 1) { cores = gs.into_iter().map(|g| g.1).collect(); source = "cpuinfo"; } else { source = "none (no siblings found)"; }
        }
    }
    let use_cores: Vec<&Vec<usize>> = if allowed.len() > 8 { cores.iter().take(4).collect() } else { cores.iter().collect() };
    let mut cpus8 = vec![]; let maxw = use_cores.iter().map(|g| g.len()).max().unwrap_or(0);
    for k in 0..maxw { for g in &use_cores { if k < g.len() { cpus8.push(g[k]); } } }
    cpus8.truncate(8);
    let cpus4: Vec<usize> = use_cores.iter().take(4).map(|g| g[0]).collect();
    Topo { allowed, cores, cpus8, cpus4, source }
}

// ------------------------------------------------------------------ B: page cache state
fn walk(dir: &Path, out: &mut Vec<(PathBuf, u64)>, errs: &mut u32, depth: u32) {
    if depth > 6 { return; }
    match std::fs::read_dir(dir) {
        Ok(rd) => for e in rd.flatten() {
            let p = e.path();
            match e.file_type() {
                Ok(ft) if ft.is_dir() => walk(&p, out, errs, depth + 1),
                Ok(ft) if ft.is_file() => { let sz = e.metadata().map(|m| m.len()).unwrap_or(0); out.push((p, sz)); }
                _ => {}
            }
        },
        Err(_) => *errs += 1,
    }
}
fn sec_b(data: &str) {
    let t0 = now();
    let mut files = vec![]; let mut errs = 0;
    walk(Path::new(data), &mut files, &mut errs, 0);
    files.sort();
    let total: u64 = files.iter().map(|f| f.1).sum();
    kv!("B.files", "{} files {:.1} MB (walk errors {})", files.len(), total as f64 / 1e6, errs);
    // mincore
    let groups = ["events", "clicks", "links", "other"];
    let mut pages = [0u64; 4]; let mut res = [0u64; 4]; let mut merr = 0; let mut last_err = 0;
    let grp = |p: &Path| -> usize { let s = p.strip_prefix(data).ok().and_then(|r| r.components().next()).map(|c| c.as_os_str().to_string_lossy().to_string()).unwrap_or_default(); match s.as_str() { "events" => 0, "clicks" => 1, "links" => 2, _ => 3 } };
    let mut fds: Vec<std::fs::File> = vec![];
    for (p, sz) in &files {
        let g = grp(p);
        let np = (*sz as usize + 4095) / 4096; pages[g] += np as u64;
        if *sz == 0 { continue; }
        let f = match std::fs::File::open(p) { Ok(f) => f, Err(e) => { merr += 1; last_err = e.raw_os_error().unwrap_or(-1); continue; } };
        use std::os::unix::io::AsRawFd;
        let m = unsafe { mmap(std::ptr::null_mut(), *sz as usize, PROT_READ, MAP_SHARED, f.as_raw_fd(), 0) };
        if m as isize == -1 { merr += 1; last_err = en(); continue; }
        let mut v = vec![0u8; np];
        if unsafe { mincore(m, *sz as usize, v.as_mut_ptr()) } != 0 { merr += 1; last_err = en(); }
        else { res[g] += v.iter().filter(|&&b| b & 1 == 1).count() as u64; }
        unsafe { munmap(m, *sz as usize) };
    }
    let tp: u64 = pages.iter().sum(); let tr: u64 = res.iter().sum();
    kv!("B.mincore.all", "{:.1} % resident of {} pages (errors {}, last errno {})", 100.0 * tr as f64 / tp.max(1) as f64, tp, merr, last_err);
    for g in 0..4 { if pages[g] > 0 { kv!(format!("B.mincore.{}", groups[g]), "{:.1} % of {:.1} MB", 100.0 * res[g] as f64 / pages[g] as f64, pages[g] as f64 * 4096.0 / 1e6); } }
    // ownership: mincore reports page-cache state only for files we own or may write
    if let Some((p, _)) = files.iter().filter(|f| grp(&f.0) == 0).max_by_key(|f| f.1) {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(p) {
            Ok(m) => kv!("B.owner", "file uid {} gid {} mode {:o}, we are uid {} gid {} (mincore sees the page cache only for files we own or may write)", m.uid(), m.gid(), m.mode() & 0o7777, unsafe { getuid() }, unsafe { getgid() }),
            Err(e) => kv!("B.owner", "ERR {}", e.raw_os_error().unwrap_or(-1)),
        }
    }
    // RWF_NOWAIT sampling: a 4 kB preadv2 every 1 MB; EAGAIN = not in page cache
    {
        let mut ok = 0u64; let mut again = 0u64; let mut other = 0u64; let mut oe = 0;
        let mut buf = vec![0u8; 4096];
        for (p, sz) in &files {
            if *sz == 0 { continue; }
            let f = match std::fs::File::open(p) { Ok(f) => f, Err(_) => continue };
            use std::os::unix::io::AsRawFd;
            let mut off = 0u64;
            while off < *sz {
                let iov: [usize; 2] = [buf.as_mut_ptr() as usize, 4096];
                let r = unsafe { syscall(327, f.as_raw_fd() as i64, iov.as_ptr() as i64, 1i64, off as i64, 0i64, 8i64) };
                if r >= 0 { ok += 1 } else { let e = en(); if e == 11 { again += 1 } else { other += 1; oe = e; } }
                off += MB as u64;
            }
        }
        kv!("B.nowait", "{:.1} % of {} samples in cache (EAGAIN {}, other errors {} errno {})", 100.0 * ok as f64 / (ok + again).max(1) as f64, ok + again + other, again, other, oe);
    }
    // timed sequential pread of the largest events file, twice
    if let Some((p, sz)) = files.iter().filter(|f| grp(&f.0) == 0).max_by_key(|f| f.1) {
        let mut buf = vec![0u8; MB];
        for k in 0..2 {
            match std::fs::File::open(p) {
                Ok(f) => {
                    use std::os::unix::io::AsRawFd;
                    let t = now(); let mut off = 0u64; let mut e = 0;
                    loop { let r = unsafe { pread(f.as_raw_fd(), buf.as_mut_ptr(), MB, off as i64) }; if r < 0 { e = en(); break; } if r == 0 { break; } off += r as u64; }
                    let dt = ms(now() - t);
                    kv!(format!("B.pread_largest.{}", k + 1), "{:.2} ms for {:.1} MB = {:.0} MB/s (err {})", dt, off as f64 / 1e6, off as f64 / 1e3 / dt.max(1e-6), e);
                }
                Err(e) => kv!(format!("B.pread_largest.{}", k + 1), "ERR {}", e.raw_os_error().unwrap_or(-1)),
            }
        }
    }
    kv!("B.ms", "{:.1} ms", ms(now() - t0));
}

// ------------------------------------------------------------------ A: system facts
fn sec_a(t: &Topo) {
    let t0 = now();
    let ci = rd("/proc/cpuinfo").unwrap_or_default();
    let first = ci.split("\n\n").next().unwrap_or("");
    let get = |k: &str| -> String { first.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_once(':')).map(|x| x.1.trim().to_string()).unwrap_or_else(|| "?".into()) };
    kv!("A.cpu.model", "{}", get("model name"));
    kv!("A.cpu.family_model_stepping", "{} {} {} microcode {}", get("cpu family"), get("model\t"), get("stepping"), get("microcode"));
    kv!("A.cpu.cache_size", "{}", get("cache size"));
    kv!("A.cpu.siblings_cores", "siblings {} cpu_cores {}", get("siblings"), get("cpu cores"));
    let mhz: Vec<String> = ci.lines().filter(|l| l.starts_with("cpu MHz")).filter_map(|l| l.split_once(':')).map(|x| x.1.trim().to_string()).collect();
    kv!("A.cpu.mhz", "{} (all processors)", mhz.join(" "));
    kv!("A.cpu.processors", "{}", ci.lines().filter(|l| l.starts_with("processor")).count());
    let flags = get("flags");
    let fl: std::collections::HashSet<&str> = flags.split_whitespace().collect();
    let want = ["avx2", "bmi2", "avx512f", "avx512bw", "avx512vl", "avx512dq", "avx512cd", "avx512vbmi", "avx512_vbmi2", "avx512ifma", "gfni", "vpclmulqdq", "avx512_vnni", "avx512_bitalg", "avx512_vpopcntdq", "avx512_bf16", "avx512_fp16", "amx_tile", "erms", "fsrm", "hypervisor", "constant_tsc", "nonstop_tsc", "tsc_known_freq", "pdpe1gb", "invpcid", "pcid"];
    let have: Vec<&str> = want.iter().cloned().filter(|w| fl.contains(w)).collect();
    let miss: Vec<&str> = want.iter().cloned().filter(|w| !fl.contains(w)).collect();
    kv!("A.cpu.flags_have", "{}", have.join(" "));
    kv!("A.cpu.flags_missing", "{}", miss.join(" "));
    kv!("A.cpu.bugs", "{}", get("bugs"));
    // caches of cpu0
    for i in 0..5 {
        let b = format!("/sys/devices/system/cpu/cpu0/cache/index{i}");
        let lv = rdl(&format!("{b}/level")); if lv.starts_with("ERR") { if i == 0 { kv!("A.cache", "{}", lv); } break; }
        kv!(format!("A.cache.index{i}"), "L{} {} {} ways {} line {} shared {}", lv, rdl(&format!("{b}/type")), rdl(&format!("{b}/size")), rdl(&format!("{b}/ways_of_associativity")), rdl(&format!("{b}/coherency_line_size")), rdl(&format!("{b}/shared_cpu_list")));
    }
    let l3s: Vec<String> = t.allowed.iter().map(|c| rdl(&format!("/sys/devices/system/cpu/cpu{c}/cache/index3/shared_cpu_list"))).collect();
    let mut l3u = l3s.clone(); l3u.dedup();
    kv!("A.cache.l3_groups", "{}", l3u.join(" | "));
    let sib: Vec<String> = t.allowed.iter().take(32).map(|c| format!("{}:{}", c, rdl(&format!("/sys/devices/system/cpu/cpu{c}/topology/thread_siblings_list")))).collect();
    kv!("A.topo.siblings", "{}", sib.join(" "));
    kv!("A.topo.core_ids", "{}", t.allowed.iter().take(32).map(|c| rdl(&format!("/sys/devices/system/cpu/cpu{c}/topology/core_id"))).collect::<Vec<_>>().join(","));
    kv!("A.topo.allowed", "{} cpus: {}", t.allowed.len(), list_str(&t.allowed));
    kv!("A.topo.cpus8", "{} (thread i runs on the i-th; the first 4 are on distinct cores; grouping from {})", list_str(&t.cpus8), t.source);
    kv!("A.topo.cpus4", "{}", list_str(&t.cpus4));
    kv!("A.topo.online", "{}", rdl("/sys/devices/system/cpu/online"));
    kv!("A.numa.nodes", "{}", rdl("/sys/devices/system/node/online"));
    let st = rd("/proc/self/status").unwrap_or_default();
    for k in ["Cpus_allowed_list", "Mems_allowed_list"] { kv!(format!("A.status.{k}"), "{}", st.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_once(':')).map(|x| x.1.trim()).unwrap_or("?")); }
    kv!("A.kernel", "{}", rdl("/proc/version"));
    kv!("A.cmdline", "{}", rdl("/proc/cmdline"));
    for f in ["enabled", "defrag", "shmem_enabled", "hpage_pmd_size", "khugepaged/defrag"] { kv!(format!("A.thp.{f}"), "{}", rdl(&format!("/sys/kernel/mm/transparent_hugepage/{f}"))); }
    let mi = rd("/proc/meminfo").unwrap_or_default();
    let mut mv = vec![];
    for k in ["MemTotal", "MemFree", "MemAvailable", "Cached", "AnonHugePages", "ShmemHugePages", "FileHugePages", "HugePages_Total", "HugePages_Free", "Hugepagesize", "SwapTotal", "Dirty", "Writeback"] {
        if let Some(l) = mi.lines().find(|l| l.starts_with(&format!("{k}:"))) { mv.push(format!("{}={}", k, l.split_once(':').map(|x| x.1.trim().replace(" kB", "k")).unwrap_or_default())); }
    }
    kv!("A.meminfo", "{}", mv.join(" "));
    // cgroup
    let cg = rdl("/proc/self/cgroup");
    kv!("A.cgroup.self", "{}", cg);
    let v2 = std::path::Path::new("/sys/fs/cgroup/cgroup.controllers").exists();
    kv!("A.cgroup.version", "{}", if v2 { "v2" } else { "v1 or none" });
    if v2 {
        for f in ["cpu.max", "cpu.weight", "memory.max", "memory.high", "memory.swap.max", "memory.current", "cpuset.cpus.effective", "cpuset.mems.effective", "pids.max", "cgroup.controllers"] {
            kv!(format!("A.cg.{f}"), "{}", rdl(&format!("/sys/fs/cgroup/{f}")));
        }
        kv!("A.cg.cpu.stat", "{}", rdl("/sys/fs/cgroup/cpu.stat").replace('\n', " "));
    } else {
        for f in ["cpu/cpu.cfs_quota_us", "cpu/cpu.cfs_period_us", "memory/memory.limit_in_bytes", "cpuset/cpuset.effective_cpus", "cpuset/cpuset.cpus"] {
            kv!(format!("A.cg1.{f}"), "{}", rdl(&format!("/sys/fs/cgroup/{f}")));
        }
    }
    // mounts
    let mnt = rd("/proc/self/mountinfo").unwrap_or_default();
    for mp in ["/", "/data", "/output", "/scratch", "/tmp", "/dev/shm", "/sys/fs/cgroup"] {
        let lines: Vec<&str> = mnt.lines().filter(|l| l.split(' ').nth(4) == Some(mp)).collect();
        if let Some(l) = lines.last() {
            let f: Vec<&str> = l.split(' ').collect();
            let sep = f.iter().position(|x| *x == "-").unwrap_or(f.len());
            let fstype = f.get(sep + 1).unwrap_or(&"?"); let src = f.get(sep + 2).unwrap_or(&"?"); let sopt = f.get(sep + 3).unwrap_or(&"?");
            let sopt: String = sopt.chars().take(160).collect();
            kv!(format!("A.mount.{mp}"), "{} src {} opts {} super {}", fstype, src, f.get(5).unwrap_or(&"?"), sopt);
        } else { kv!(format!("A.mount.{mp}"), "none"); }
    }
    let mut sv = vec![];
    for k in ["dirty_ratio", "dirty_background_ratio", "dirty_bytes", "dirty_background_bytes", "dirty_expire_centisecs", "dirty_writeback_centisecs"] { kv!(format!("A.vm.{k}"), "{}", rdl(&format!("/proc/sys/vm/{k}"))); }
    for k in ["vm/swappiness", "vm/overcommit_memory", "vm/max_map_count", "vm/nr_hugepages", "vm/zone_reclaim_mode", "kernel/perf_event_paranoid", "kernel/numa_balancing", "kernel/sched_autogroup_enabled", "kernel/randomize_va_space"] { sv.push(format!("{}={}", k.rsplit('/').next().unwrap_or(k), rdl(&format!("/proc/sys/{k}")))); }
    kv!("A.sysctl", "{}", sv.join(" "));
    kv!("A.clocksource", "{} (available: {})", rdl("/sys/devices/system/clocksource/clocksource0/current_clocksource"), rdl("/sys/devices/system/clocksource/clocksource0/available_clocksource"));
    let mut vul = vec![];
    if let Ok(rdir) = std::fs::read_dir("/sys/devices/system/cpu/vulnerabilities") {
        let mut v: Vec<_> = rdir.flatten().map(|e| e.path()).collect(); v.sort();
        for p in v { let n = p.file_name().map(|x| x.to_string_lossy().to_string()).unwrap_or_default(); let s = rdl(&p.to_string_lossy()); let w: String = s.split(|c| c == ':' || c == ';' || c == ',').next().unwrap_or("").chars().take(24).collect(); if !w.starts_with("Not affected") { vul.push(format!("{}={}", n, w.replace(' ', "_"))); } }
    }
    kv!("A.vulns", "{} (entries not listed are Not affected)", vul.join(" "));
    kv!("A.cpufreq", "driver {} governor {} max {}", rdl("/sys/devices/system/cpu/cpu0/cpufreq/scaling_driver"), rdl("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"), rdl("/sys/devices/system/cpu/cpu0/cpufreq/cpuinfo_max_freq"));
    let mut env: Vec<String> = std::env::vars().map(|(k, v)| format!("{k}={v}")).collect(); env.sort();
    kv!("A.env", "{}", env.join(" "));
    kv!("A.ids", "uid {} gid {} pid {}", unsafe { getuid() }, unsafe { getgid() }, std::process::id());
    for (n, r) in [("NOFILE", 7), ("MEMLOCK", 8), ("NPROC", 6), ("STACK", 3), ("AS", 9)] {
        let mut l = [0u64; 2];
        if unsafe { getrlimit(r, &mut l) } == 0 { let f = |x: u64| if x == u64::MAX { "inf".to_string() } else { x.to_string() }; kv!(format!("A.rlimit.{n}"), "soft {} hard {}", f(l[0]), f(l[1])); }
        else { kv!(format!("A.rlimit.{n}"), "ERR {}", en()); }
    }
    let mut res = [0i64; 2]; unsafe { clock_getres(1, &mut res) };
    // clock_gettime cost and minimal step
    let mut mn = u64::MAX; let t = now(); let mut last = now(); let mut k = 0u64;
    while k < 200000 { let x = now(); if x > last { mn = mn.min(x - last); } last = x; k += 1; }
    let per = (now() - t) as f64 / 200000.0;
    kv!("A.clock", "getres {} ns, clock_gettime {:.1} ns per call, min step {} ns", res[1], per, mn);
    kv!("A.uptime", "{}", rdl("/proc/uptime"));
    kv!("A.loadavg", "{}", rdl("/proc/loadavg"));
    kv!("A.stat.cpu", "{}", rd("/proc/stat").unwrap_or_default().lines().next().unwrap_or(""));
    for p in ["cpu", "memory", "io"] { kv!(format!("A.pressure.{p}"), "{}", rdl(&format!("/proc/pressure/{p}")).replace('\n', " | ")); }
    kv!("A.ms", "{:.1} ms", ms(now() - t0));
}

// ------------------------------------------------------------------ J: process costs and syscall availability
fn sec_j(t: &Topo) {
    let t0 = now();
    let mut v = vec![];
    for _ in 0..20 { let a = now(); let hs: Vec<_> = (0..8).filter_map(|_| std::thread::Builder::new().stack_size(256 << 10).spawn(|| black_box(1u64)).ok()).collect(); for h in hs { let _ = h.join(); } v.push(ms(now() - a)); }
    kv!("J.spawn_join_8", "{:.3} ms min, {:.3} median", minf(&v), median(&mut v));
    // affinity
    if t.allowed.len() >= 2 {
        let (c1, c2) = (t.allowed[0], t.allowed[t.allowed.len() - 1]);
        let r = std::thread::Builder::new().spawn(move || {
            let mut ok = 0; let mut fail = 0; let mut e = 0;
            let a = now();
            for i in 0..200 { let c = if i % 2 == 0 { c1 } else { c2 }; if pin(c) == 0 { if unsafe { sched_getcpu() } == c as i32 { ok += 1 } else { fail += 1 } } else { fail += 1; e = en(); } }
            ((now() - a) as f64 / 200.0, ok, fail, e)
        }).ok().and_then(|h| h.join().ok());
        if let Some((ns, ok, fail, e)) = r { kv!("J.setaffinity", "{:.0} ns per call (incl. migration), {} ok {} failed errno {}", ns, ok, fail, e); }
    }
    // syscall cost
    let a = now(); for _ in 0..20000 { unsafe { syscall(110) }; } kv!("J.syscall_getppid", "{:.0} ns", (now() - a) as f64 / 20000.0);
    // perf_event_open (cycles, user only)
    let mut attr = [0u64; 16];
    attr[0] = (64u64) << 32; // type 0 (HARDWARE), size 64
    attr[1] = 0; // CPU_CYCLES
    attr[5] = (1 << 5) | (1 << 6); // exclude_kernel, exclude_hv
    let fd = unsafe { syscall(298, attr.as_ptr() as i64, 0i64, -1i64, -1i64, 0i64) };
    if fd >= 0 {
        let a = now(); let n = chain_run(0, 20_000_000);
        let mut c = 0u64; unsafe { read(fd as i32, (&mut c as *mut u64).cast(), 8) };
        let dt = now() - a;
        kv!("J.perf_event_open", "ok, cycles {} over {:.1} ms = {:.3} GHz real (chain says {:.3})", c, ms(dt), c as f64 / dt as f64, 8.0 * 20_000_000.0 / dt as f64);
        black_box(n); unsafe { close(fd as i32) };
    } else { kv!("J.perf_event_open", "ERR {}", en()); }
    let mut params = [0u32; 30];
    let fd = unsafe { syscall(425, 4i64, params.as_mut_ptr() as i64) };
    if fd >= 0 { kv!("J.io_uring_setup", "ok"); unsafe { close(fd as i32) }; } else { kv!("J.io_uring_setup", "ERR {}", en()); }
    let p = unsafe { mmap(std::ptr::null_mut(), MB2, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANON | MAP_HUGETLB, -1, 0) };
    if p as isize == -1 { kv!("J.map_hugetlb", "ERR {}", en()); } else { kv!("J.map_hugetlb", "ok"); unsafe { munmap(p, MB2) }; }
    let b = vec![1u8; MB];
    if unsafe { mlock(b.as_ptr(), MB) } == 0 { kv!("J.mlock_1MB", "ok"); unsafe { munlock(b.as_ptr(), MB) }; } else { kv!("J.mlock_1MB", "ERR {}", en()); }
    kv!("J.ms", "{:.1} ms", ms(now() - t0));
}

// ------------------------------------------------------------------ C: input access
struct InFile { f: std::fs::File, len: usize }
fn event_files(data: &str) -> Vec<InFile> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(format!("{data}/events")) { Ok(r) => r.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "csv").unwrap_or(false)).collect(), Err(_) => vec![] };
    v.sort();
    v.into_iter().filter_map(|p| { let len = std::fs::metadata(&p).ok()?.len() as usize; let f = std::fs::File::open(&p).ok()?; Some(InFile { f, len }) }).collect()
}
fn sec_c(t: &Topo, data: &str) {
    let t0 = now();
    use std::os::unix::io::AsRawFd;
    let files = event_files(data);
    let total: usize = files.iter().map(|f| f.len).sum();
    if files.is_empty() || total == 0 { kv!("C.files", "ERR no event files"); return; }
    let chunk = 4 * MB;
    let mut chunks = vec![];
    for (fi, f) in files.iter().enumerate() { let mut o = 0; while o < f.len { let l = chunk.min(f.len - o); chunks.push((fi, o, l)); o += l; } }
    kv!("C.files", "{} files {:.1} MB {} chunks of 4 MB, 8 threads", files.len(), total as f64 / 1e6, chunks.len());
    let passes = 12;
    let nth = t.cpus8.len().max(1);
    for mode in ["mmap_touch", "mmap_count", "mmap_count_unmapchunk", "pread256k_count"] {
        let mut gb = vec![]; let mut un = vec![]; let mut nl = 0u64; let mut flt = 0i64; let mut first = 0.0;
        for pass in 0..passes {
            let f0 = minflt();
            let a = now();
            let mut maps: Vec<usize> = vec![];
            if mode.starts_with("mmap") {
                for f in &files { let p = if f.len == 0 { std::ptr::null_mut() } else { unsafe { mmap(std::ptr::null_mut(), f.len, PROT_READ, MAP_PRIVATE, f.f.as_raw_fd(), 0) } }; maps.push(if p as isize == -1 { 0 } else { p as usize }); }
            }
            let next = AtomicUsize::new(0);
            let (r, _) = par(nth, &t.cpus8, |_| {
                let mut c = 0u64;
                let mut buf = if mode.starts_with("pread") { vec![0u8; 256 << 10] } else { vec![] };
                loop {
                    let i = next.fetch_add(1, Relaxed); if i >= chunks.len() { break; }
                    let (fi, off, l) = chunks[i];
                    if mode.starts_with("pread") {
                        let mut d = 0;
                        while d < l { let w = (256usize << 10).min(l - d); let r = unsafe { pread(files[fi].f.as_raw_fd(), buf.as_mut_ptr(), w, (off + d) as i64) }; if r <= 0 { break; } c += unsafe { count256(buf.as_ptr(), r as usize) }; d += r as usize; }
                    } else {
                        let base = maps[fi]; if base == 0 { continue; }
                        let p = (base + off) as *const u8;
                        if mode == "mmap_touch" { let mut s = 0u64; let mut o = 0; while o < l { s += unsafe { *p.add(o) } as u64; o += 4096; } c += black_box(s) & 0; }
                        else { c += unsafe { count256(p, l) }; }
                        if mode == "mmap_count_unmapchunk" { unsafe { munmap(p as *mut u8, (l + 4095) & !4095) }; }
                    }
                }
                c
            });
            let work = now() - a;
            let b = now();
            if mode.starts_with("mmap") && mode != "mmap_count_unmapchunk" { for (fi, &m) in maps.iter().enumerate() { if m != 0 { unsafe { munmap(m as *mut u8, files[fi].len) }; } } }
            let unm = now() - b;
            if pass == 0 { first = ms(work); }
            nl = r.iter().sum(); flt = minflt() - f0;
            gb.push(total as f64 / work as f64); un.push(ms(unm));
        }
        let best = gb.iter().cloned().fold(0.0, f64::max);
        kv!(format!("C.{mode}"), "{:.2} GB/s best, {:.2} median, first pass {:.2} ms, munmap {:.2} ms min, {} minflt/pass ({:.1} per MB), check {}", best, median(&mut gb), first, minf(&un), flt, flt as f64 / (total as f64 / 1e6), nl);
    }
    kv!("C.ms", "{:.1} ms", ms(now() - t0));
}

// ------------------------------------------------------------------ D: anonymous memory faults
fn anon_huge_kb() -> String {
    let s = rd("/proc/self/smaps_rollup").unwrap_or_default();
    s.lines().find(|l| l.starts_with("AnonHugePages")).and_then(|l| l.split_once(':')).map(|x| x.1.trim().to_string()).unwrap_or_else(|| "?".into())
}
fn touch_par(t: &Topo, a: &Anon) -> f64 {
    let nth = t.cpus8.len().max(1); let p = Ptr(a.p); let len = a.len;
    let (_, w) = par(nth, &t.cpus8, |i| { let lo = len * i / nth & !4095; let hi = if i + 1 == nth { len } else { len * (i + 1) / nth & !4095 }; let mut o = lo; while o < hi { unsafe { p.g().add(o).write_volatile(1) }; o += 4096; } 0u8 });
    w * 1e3
}
fn sec_d(t: &Topo) {
    let t0 = now();
    for (name, thp) in [("default", 0), ("madv_huge", 1), ("madv_nohuge", 2)] {
        let mut tt = vec![]; let mut tu = vec![]; let mut flt = 0; let mut ah = String::new();
        for rep in 0..2 {
            let Some(a) = Anon::new(GB, thp) else { kv!(format!("D.{name}"), "ERR {}", en()); break };
            let f0 = minflt();
            tt.push(touch_par(t, &a));
            flt = minflt() - f0;
            if rep == 0 { ah = anon_huge_kb(); }
            tu.push(a.free());
        }
        kv!(format!("D.touch.{name}"), "{:.1} ms per GB (8 threads, min of 2), {} faults, AnonHugePages after touch {}", minf(&tt), flt, ah);
        kv!(format!("D.munmap.{name}"), "{:.1} ms per GB (1 thread)", minf(&tu));
    }
    for (name, thp) in [("default", 0), ("madv_nohuge", 2)] {
        let Some(a) = Anon::new(GB, thp) else { continue };
        touch_par(t, &a);
        let nth = t.cpus8.len().max(1); let p = Ptr(a.p); let len = a.len;
        let (_, w) = par(nth, &t.cpus8, |i| { let lo = len * i / nth & !4095; let hi = if i + 1 == nth { len } else { len * (i + 1) / nth & !4095 }; unsafe { madvise(p.g().add(lo), hi - lo, MADV_DONTNEED) }; 0u8 });
        let u = a.free();
        kv!(format!("D.dontneed_par.{name}"), "{:.1} ms per GB (8 threads), then munmap {:.1} ms", w * 1e3, u);
    }
    kv!("D.ms", "{:.1} ms", ms(now() - t0));
}

// ------------------------------------------------------------------ E: memory bandwidth
#[inline(never)]
unsafe fn bw_read(p: *const u8, len: usize) -> u64 {
    let mut a = _mm256_setzero_si256(); let mut b = a; let mut c = a; let mut d = a; let mut i = 0;
    while i + 128 <= len {
        a = _mm256_add_epi64(a, _mm256_load_si256(p.add(i).cast())); b = _mm256_add_epi64(b, _mm256_load_si256(p.add(i + 32).cast()));
        c = _mm256_add_epi64(c, _mm256_load_si256(p.add(i + 64).cast())); d = _mm256_add_epi64(d, _mm256_load_si256(p.add(i + 96).cast()));
        i += 128;
    }
    let s = _mm256_add_epi64(_mm256_add_epi64(a, b), _mm256_add_epi64(c, d));
    (_mm256_extract_epi64(s, 0) + _mm256_extract_epi64(s, 1) + _mm256_extract_epi64(s, 2) + _mm256_extract_epi64(s, 3)) as u64
}
#[inline(never)]
unsafe fn bw_store(p: *mut u8, len: usize, v: i64) { let x = _mm256_set1_epi64x(v); let mut i = 0; while i + 128 <= len { _mm256_store_si256(p.add(i).cast(), x); _mm256_store_si256(p.add(i + 32).cast(), x); _mm256_store_si256(p.add(i + 64).cast(), x); _mm256_store_si256(p.add(i + 96).cast(), x); i += 128; } }
#[inline(never)]
unsafe fn bw_nt(p: *mut u8, len: usize, v: i64) { let x = _mm256_set1_epi64x(v); let mut i = 0; while i + 128 <= len { _mm256_stream_si256(p.add(i).cast(), x); _mm256_stream_si256(p.add(i + 32).cast(), x); _mm256_stream_si256(p.add(i + 64).cast(), x); _mm256_stream_si256(p.add(i + 96).cast(), x); i += 128; } _mm_sfence(); }
fn sec_e(t: &Topo) {
    let t0 = now();
    let per = 256 * MB; let n8 = t.cpus8.len().max(1);
    let Some(a) = Anon::new(per * n8, 1) else { kv!("E.alloc", "ERR {}", en()); return };
    let p = Ptr(a.p);
    let (_, w) = par(n8, &t.cpus8, |i| { unsafe { std::ptr::write_bytes(p.g().add(i * per), 1, per) }; 0u8 });
    kv!("E.prefault", "{:.1} ms for {} MB (8 threads, memset of fresh MADV_HUGEPAGE memory)", w * 1e3, per * n8 / MB);
    let sets: Vec<(&str, Vec<usize>)> = vec![("t1", t.cpus8[..1.min(n8)].to_vec()), ("t4", t.cpus4.clone()), ("t8", t.cpus8.clone())];
    for (tn, cpus) in &sets {
        let n = cpus.len(); if n == 0 { continue; }
        for k in ["read", "store", "memcpy", "ntstore"] {
            let mut best = 0.0f64;
            for _ in 0..3 {
                let (_, w) = par(n, cpus, |i| unsafe {
                    let q = p.g().add(i * per);
                    match k { "read" => { black_box(bw_read(q, per)); } "store" => bw_store(q, per, 7), "memcpy" => std::ptr::copy_nonoverlapping(q, q.add(per / 2), per / 2), _ => bw_nt(q, per, 9) }
                    0u8
                });
                best = best.max((per * n) as f64 / w / 1e9);
            }
            kv!(format!("E.{k}.{tn}"), "{:.1} GB/s{}", best, if k == "memcpy" { " (read+write bytes)" } else { "" });
        }
    }
    a.free();
    kv!("E.ms", "{:.1} ms", ms(now() - t0));
}

// ------------------------------------------------------------------ F: radix and random reads
fn sec_f(t: &Topo) {
    let t0 = now();
    let n: usize = 20_000_000; const KB: usize = 25; let mask = (1u64 << KB) - 1;
    let nth = t.cpus8.len().max(1); let cpus = &t.cpus8;
    let maxp = 1usize << 11;
    let partlen = n * 16 + nth * maxp * 64 + 4096;
    let (Some(inp), Some(out), Some(part)) = (Anon::new(n * 16, 1), Anon::new(n * 16, 1), Anon::new(partlen, 1)) else { kv!("F.alloc", "ERR {}", en()); return };
    let (ip, op, pp) = (Ptr(inp.p), Ptr(out.p), Ptr(part.p));
    let lo = |th: usize| th * n / nth;
    let (_, w) = par(nth, cpus, |th| unsafe {
        let a = (partlen * th / nth) & !4095; let b = if th + 1 == nth { partlen } else { (partlen * (th + 1) / nth) & !4095 };
        std::ptr::write_bytes(pp.g().add(a), 0, b - a);
        let q = op.g() as *mut [u64; 2]; for i in lo(th)..lo(th + 1) { q.add(i).write([0, 0]); }
        let q = ip.g() as *mut [u64; 2]; for i in lo(th)..lo(th + 1) { q.add(i).write([splitmix(i as u64) & mask, i as u64]); }
        0u8
    });
    kv!("F.setup", "{:.1} ms (fault 960 MB + generate 20M records)", w * 1e3);
    // direct scatter: each record to a random slot
    let mut best = f64::MAX;
    for _ in 0..3 {
        let (_, w) = par(nth, cpus, |th| unsafe {
            let ipp = ip.g() as *const [u64; 2]; let opp = op.g() as *mut [u64; 2];
            for i in lo(th)..lo(th + 1) { let r = *ipp.add(i); let d = ((splitmix(r[1] ^ 0x5555) as u128 * n as u128) >> 64) as usize; opp.add(d).write(r); }
            0u8
        });
        best = best.min(w * 1e3);
    }
    kv!("F.radix.direct", "{:.1} ms (20M x 16 B, random slot, normal stores)", best);
    for pb in [9usize, 10, 11] {
        let np = 1usize << pb; let shift = KB - pb;
        let mut bt = [f64::MAX; 4]; let mut ok = true;
        for rep in 0..3 {
            let a0 = now();
            let (hist, _) = par(nth, cpus, |th| { let ipp = ip.g() as *const [u64; 2]; let mut h = vec![0u32; np]; for i in lo(th)..lo(th + 1) { h[(unsafe { (*ipp.add(i))[0] } >> shift) as usize] += 1; } h });
            let mut off = vec![vec![0usize; np]; nth]; let mut cur = 0usize;
            for b in 0..np { for th in 0..nth { off[th][b] = cur; cur += (hist[th][b] as usize + 3) & !3; } }
            if cur * 16 > partlen { ok = false; break; }
            let mut gstart = vec![0usize; np + 1]; for b in 0..np { gstart[b + 1] = gstart[b] + (0..nth).map(|th| hist[th][b] as usize).sum::<usize>(); }
            let a1 = now();
            #[repr(C, align(64))] #[derive(Clone, Copy)] struct L([[u64; 2]; 4]);
            let offr = &off;
            par(nth, cpus, |th| unsafe {
                let mut dst = offr[th].clone();
                let ipp = ip.g() as *const [u64; 2]; let ppp = pp.g() as *mut [u64; 2];
                let mut bufs = vec![L([[0; 2]; 4]); np]; let mut fill = vec![0u8; np];
                for i in lo(th)..lo(th + 1) { let r = *ipp.add(i); let b = (r[0] >> shift) as usize; let f = fill[b] as usize; bufs[b].0[f] = r;
                    if f == 3 { let v = _mm512_load_si512(bufs[b].0.as_ptr().cast()); _mm512_stream_si512(ppp.add(dst[b]).cast(), v); dst[b] += 4; fill[b] = 0; } else { fill[b] = (f + 1) as u8; } }
                for b in 0..np { for f in 0..fill[b] as usize { ppp.add(dst[b] + f).write(bufs[b].0[f]); } }
                _mm_sfence(); 0u8
            });
            let a2 = now();
            let lowbits = KB - pb; let lmask = (1u64 << lowbits) - 1; let next = AtomicUsize::new(0);
            let (histr, gs) = (&hist, &gstart);
            par(nth, cpus, |_| unsafe {
                let ppp = pp.g() as *const [u64; 2]; let opp = op.g() as *mut [u64; 2]; let mut c = vec![0u32; 1 << lowbits];
                loop { let b = next.fetch_add(1, Relaxed); if b >= np { break; } c.fill(0);
                    for th in 0..nth { let o = offr[th][b]; for j in 0..histr[th][b] as usize { c[((*ppp.add(o + j))[0] & lmask) as usize] += 1; } }
                    let mut x = 0u32; for v in c.iter_mut() { let y = *v; *v = x; x += y; }
                    let ob = opp.add(gs[b]);
                    for th in 0..nth { let o = offr[th][b]; for j in 0..histr[th][b] as usize { let r = *ppp.add(o + j); let k = (r[0] & lmask) as usize; ob.add(c[k] as usize).write(r); c[k] += 1; } } }
                0u8
            });
            let a3 = now();
            let v = [ms(a1 - a0), ms(a2 - a1), ms(a3 - a2), ms(a3 - a0)];
            for k in 0..4 { bt[k] = bt[k].min(v[k]); }
            if rep == 0 && pb == 10 {
                let q = op.g() as *const [u64; 2]; let mut prev = 0u64; let mut sum = 0u64; let mut sorted = true;
                for i in 0..n { let r = unsafe { *q.add(i) }; if r[0] < prev { sorted = false; } prev = r[0]; sum = sum.wrapping_add(r[1]); }
                kv!("F.radix.verify", "{}", if sorted && sum == (n as u64) * (n as u64 - 1) / 2 { "ok" } else { "FAILED" });
            }
        }
        if !ok { kv!(format!("F.radix.part{pb}"), "ERR overflow"); continue; }
        kv!(format!("F.radix.part{pb}"), "{:.1} ms total (hist {:.1}, partition SWWC+NT {:.1}, in-bucket scatter {:.1}); bucket {} kB", bt[3], bt[0], bt[1], bt[2], n * 16 / np / 1024);
    }
    inp.free(); out.free(); part.free();
    // random reads
    let tl = 256 * MB;
    let Some(tab) = Anon::new(tl, 1) else { kv!("F.rand", "ERR {}", en()); return };
    let tp = Ptr(tab.p);
    for &sz in &[256usize << 10, MB, 4 * MB, 32 * MB, 256 * MB] {
        if el_s() > 38.0 { kv!("F.rand.skip", "budget"); break; }
        let nlines = sz / 64; let q = tab.p as *mut u64;
        // random cycle over lines (Sattolo), one 8-byte slot per line at a random position
        let mut perm: Vec<u32> = (0..nlines as u32).collect();
        let mut s = 0x1234_5678_9abc_def1u64 ^ sz as u64;
        for i in (1..nlines).rev() { s ^= s << 13; s ^= s >> 7; s ^= s << 17; let j = (s % i as u64) as usize; perm.swap(i, j); }
        let slot = |l: usize| l * 8 + (splitmix(l as u64) & 7) as usize;
        for i in 0..nlines { unsafe { q.add(slot(perm[i] as usize)).write(slot(perm[(i + 1) % nlines] as usize) as u64) }; }
        let chase = |start: usize| -> f64 { let q = tp.g() as *const u64; let mut x = start as u64; let mut steps = 0u64; let a = now();
            loop { for _ in 0..4096 { x = unsafe { *q.add(x as usize) }; } steps += 4096; if now() - a > 40_000_000 { break; } }
            black_box(x); (now() - a) as f64 / steps as f64 };
        let (r1, _) = par(1, &t.cpus8[..1], |_| chase(slot(perm[0] as usize)));
        let (r8, _) = par(nth, cpus, |i| chase(slot(perm[i * nlines / nth] as usize)));
        let nslots = sz / 8;
        let (ri, _) = par(nth, cpus, |i| { let q = tp.g() as *const u64; let mut s = splitmix(i as u64 + 1); let mut acc = 0u64; let mut steps = 0u64; let a = now();
            loop { for _ in 0..4096 { s ^= s << 13; s ^= s >> 7; s ^= s << 17; acc = acc.wrapping_add(unsafe { *q.add((s as usize) & (nslots - 1)) }); } steps += 4096; if now() - a > 40_000_000 { break; } }
            black_box(acc); (now() - a) as f64 / steps as f64 });
        let avg = |v: &[f64]| v.iter().sum::<f64>() / v.len().max(1) as f64;
        kv!(format!("F.rand.{}k", sz >> 10), "chase {:.2} ns/read 1T, {:.2} ns/read 8T; independent {:.2} ns/read per thread 8T ({:.2} ns aggregate)", r1[0], avg(&r8), avg(&ri), avg(&ri) / nth as f64);
    }
    tab.free();
    kv!("F.ms", "{:.1} ms", ms(now() - t0));
}

// ------------------------------------------------------------------ G/H: SIMD width, clock, hyperthreads
#[inline(never)]
fn k_scalar(buf: &[u8], tab: &[u32; 256]) -> u64 {
    let mut acc = 0u64; let mut cnt = 0u64;
    let mut i = 0usize;
    while i < buf.len() {
        let b = buf[i];
        if b == b'\n' || b == b',' { cnt += 1; acc = acc.wrapping_add(tab[(i as u8 ^ b) as usize] as u64); }
        i += 1;
        // keep LLVM from vectorizing this loop: an opaque, free dependency
        unsafe { asm!("/* {0} */", inout(reg) i, options(nomem, nostack, preserves_flags)); }
    }
    acc.wrapping_add(cnt << 40)
}
#[inline(never)]
unsafe fn k_avx2(buf: &[u8], tab: &[u32; 256]) -> u64 {
    let (nl, cm) = (_mm256_set1_epi8(10), _mm256_set1_epi8(44));
    let p = buf.as_ptr(); let n = buf.len() & !31; let mut acc = 0u64; let mut cnt = 0u64; let mut i = 0;
    while i < n {
        let v = _mm256_loadu_si256(p.add(i).cast());
        let mut m = (_mm256_movemask_epi8(_mm256_or_si256(_mm256_cmpeq_epi8(v, nl), _mm256_cmpeq_epi8(v, cm))) as u32) as u64;
        cnt += m.count_ones() as u64;
        while m != 0 { let z = m.trailing_zeros() as usize; let j = i + z; acc = acc.wrapping_add(*tab.get_unchecked((j as u8 ^ *p.add(j)) as usize) as u64); m &= m - 1; }
        i += 32;
    }
    acc.wrapping_add(cnt << 40)
}
#[inline(never)]
#[target_feature(enable = "avx512bw")]
unsafe fn k_avx512(buf: &[u8], tab: &[u32; 256]) -> u64 {
    let (nl, cm) = (_mm512_set1_epi8(10), _mm512_set1_epi8(44));
    let p = buf.as_ptr(); let n = buf.len() & !63; let mut acc = 0u64; let mut cnt = 0u64; let mut i = 0;
    while i < n {
        let v = _mm512_loadu_si512(p.add(i).cast());
        let mut m = _mm512_cmpeq_epi8_mask(v, nl) | _mm512_cmpeq_epi8_mask(v, cm);
        cnt += m.count_ones() as u64;
        while m != 0 { let z = m.trailing_zeros() as usize; let j = i + z; acc = acc.wrapping_add(*tab.get_unchecked((j as u8 ^ *p.add(j)) as usize) as u64); m &= m - 1; }
        i += 64;
    }
    acc.wrapping_add(cnt << 40)
}
static HAS512: AtomicBool = AtomicBool::new(false);
fn kern(k: usize, buf: &[u8], tab: &[u32; 256]) -> u64 {
    match k { 0 => k_scalar(buf, tab), 1 => unsafe { k_avx2(buf, tab) }, _ => if HAS512.load(Relaxed) { unsafe { k_avx512(buf, tab) } } else { 0 } }
}
/// runs kernel k on a 64 kB buffer for dur_ns; returns bytes/s
fn kern_for(k: usize, buf: &[u8], tab: &[u32; 256], dur: u64) -> f64 {
    let a = now(); let mut bytes = 0u64; let mut s = 0u64;
    loop { s = s.wrapping_add(kern(k, black_box(buf), tab)); bytes += buf.len() as u64; if now() - a > dur { break; } }
    black_box(s); bytes as f64 / (now() - a) as f64 * 1e9
}
/// windows (ms) of kernel k right after a switch; returns GB/s per window
fn kern_windows(k: usize, buf: &[u8], tab: &[u32; 256], edges: &[f64]) -> Vec<f64> {
    let sw = now(); let end = (edges[edges.len() - 1] * 1e6) as u64;
    let mut wb = vec![0u64; edges.len() - 1]; let mut wt = vec![0u64; edges.len() - 1];
    loop {
        let a = now(); black_box(kern(k, black_box(buf), tab)); let b = now();
        let mid = ((a + b) / 2 - sw) as f64 / 1e6;
        for w in 0..edges.len() - 1 { if mid >= edges[w] && mid < edges[w + 1] { wb[w] += buf.len() as u64; wt[w] += b - a; } }
        if b - sw > end { break; }
    }
    (0..wb.len()).map(|w| if wt[w] > 0 { wb[w] as f64 / wt[w] as f64 } else { f64::NAN }).collect()
}
#[inline(never)]
fn chain(n: u64, mode: u32) {
    let mut a = 0u64; let mut c = n.max(1);
    unsafe {
        match mode {
            0 => asm!("2:", "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "dec {c}", "jnz 2b",
                      a = inout(reg) a, b = in(reg) 1u64, c = inout(reg) c, options(nomem, nostack)),
            1 => asm!("vpxor ymm0, ymm0, ymm0", "vpxor ymm1, ymm1, ymm1", "vpxor ymm2, ymm2, ymm2", "vpxor ymm3, ymm3, ymm3", "vpxor ymm4, ymm4, ymm4",
                      "2:", "add {a}, {b}", "vpaddd ymm0, ymm0, ymm4", "add {a}, {b}", "vpaddd ymm1, ymm1, ymm4", "add {a}, {b}", "vpaddd ymm2, ymm2, ymm4", "add {a}, {b}", "vpaddd ymm3, ymm3, ymm4",
                      "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "dec {c}", "jnz 2b", "vzeroupper",
                      a = inout(reg) a, b = in(reg) 1u64, c = inout(reg) c, out("ymm0") _, out("ymm1") _, out("ymm2") _, out("ymm3") _, out("ymm4") _, options(nomem, nostack)),
            2 => asm!("vpxor ymm0, ymm0, ymm0", "vpxor ymm1, ymm1, ymm1", "vpxor ymm2, ymm2, ymm2", "vpxor ymm3, ymm3, ymm3", "vpxor ymm4, ymm4, ymm4", "vpxor ymm5, ymm5, ymm5",
                      "2:", "add {a}, {b}", "vfmadd231ps ymm0, ymm4, ymm5", "add {a}, {b}", "vfmadd231ps ymm1, ymm4, ymm5", "add {a}, {b}", "vfmadd231ps ymm2, ymm4, ymm5", "add {a}, {b}", "vfmadd231ps ymm3, ymm4, ymm5",
                      "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "dec {c}", "jnz 2b", "vzeroupper",
                      a = inout(reg) a, b = in(reg) 1u64, c = inout(reg) c, out("ymm0") _, out("ymm1") _, out("ymm2") _, out("ymm3") _, out("ymm4") _, out("ymm5") _, options(nomem, nostack)),
            3 => asm!("vpxord zmm0, zmm0, zmm0", "vpxord zmm1, zmm1, zmm1", "vpxord zmm2, zmm2, zmm2", "vpxord zmm3, zmm3, zmm3", "vpxord zmm4, zmm4, zmm4",
                      "2:", "add {a}, {b}", "vpaddd zmm0, zmm0, zmm4", "add {a}, {b}", "vpaddd zmm1, zmm1, zmm4", "add {a}, {b}", "vpaddd zmm2, zmm2, zmm4", "add {a}, {b}", "vpaddd zmm3, zmm3, zmm4",
                      "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "dec {c}", "jnz 2b", "vzeroupper",
                      a = inout(reg) a, b = in(reg) 1u64, c = inout(reg) c, out("zmm0") _, out("zmm1") _, out("zmm2") _, out("zmm3") _, out("zmm4") _, options(nomem, nostack)),
            _ => asm!("vpxord zmm0, zmm0, zmm0", "vpxord zmm1, zmm1, zmm1", "vpxord zmm2, zmm2, zmm2", "vpxord zmm3, zmm3, zmm3", "vpxord zmm4, zmm4, zmm4", "vpxord zmm5, zmm5, zmm5",
                      "2:", "add {a}, {b}", "vfmadd231ps zmm0, zmm4, zmm5", "add {a}, {b}", "vfmadd231ps zmm1, zmm4, zmm5", "add {a}, {b}", "vfmadd231ps zmm2, zmm4, zmm5", "add {a}, {b}", "vfmadd231ps zmm3, zmm4, zmm5",
                      "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "add {a}, {b}", "dec {c}", "jnz 2b", "vzeroupper",
                      a = inout(reg) a, b = in(reg) 1u64, c = inout(reg) c, out("zmm0") _, out("zmm1") _, out("zmm2") _, out("zmm3") _, out("zmm4") _, out("zmm5") _, options(nomem, nostack)),
        }
    }
    black_box(a);
}
/// runs the chain for dur_ns in 1M-iteration calls; returns GHz (8 dependent adds per iteration)
fn chain_for(mode: u32, dur: u64) -> f64 { let a = now(); let mut it = 0u64; loop { chain(1_000_000, mode); it += 1_000_000; if now() - a > dur { break; } } 8.0 * it as f64 / (now() - a) as f64 }
fn chain_run(mode: u32, iters: u64) -> u64 { chain(iters, mode); iters }
fn chain_windows(edges: &[f64]) -> Vec<f64> {
    let sw = now(); let end = (edges[edges.len() - 1] * 1e6) as u64;
    let mut wi = vec![0u64; edges.len() - 1]; let mut wt = vec![0u64; edges.len() - 1];
    loop { let a = now(); chain(10_000, 0); let b = now(); let mid = ((a + b) / 2 - sw) as f64 / 1e6;
        for w in 0..edges.len() - 1 { if mid >= edges[w] && mid < edges[w + 1] { wi[w] += 10_000; wt[w] += b - a; } }
        if b - sw > end { break; } }
    (0..wi.len()).map(|w| if wt[w] > 0 { 8.0 * wi[w] as f64 / wt[w] as f64 } else { f64::NAN }).collect()
}
fn fmt_v(v: &[f64]) -> String { v.iter().map(|x| format!("{:.3}", x)).collect::<Vec<_>>().join(" ") }
fn make_buf(data: &str) -> Vec<u8> {
    let mut b = vec![0u8; 64 << 10];
    let files = event_files(data);
    use std::os::unix::io::AsRawFd;
    let mut got = 0;
    if let Some(f) = files.iter().max_by_key(|f| f.len) { let r = unsafe { pread(f.f.as_raw_fd(), b.as_mut_ptr(), b.len(), 4096) }; if r > 0 { got = r as usize; } }
    if got < b.len() { let pat = b"e1abc,u2def,2026-10-03T10:00:00Z,2026-10-03T10:00:05Z,view,p123,\n"; for i in got..b.len() { b[i] = pat[i % pat.len()]; } }
    b
}
fn sec_g(t: &Topo, data: &str) {
    let t0 = now();
    let has = std::is_x86_feature_detected!("avx512bw") && std::is_x86_feature_detected!("avx512f");
    HAS512.store(has, Relaxed);
    kv!("G.avx512bw_runtime", "{}", has);
    let src = make_buf(data);
    let mut tab = [0u32; 256]; for i in 0..256 { tab[i] = splitmix(i as u64) as u32; }
    let n8 = t.cpus8.len().max(1); let c1 = &t.cpus8[..1];
    // checks: all three kernels agree
    let r: Vec<u64> = (0..3).map(|k| kern(k, &src, &tab)).collect();
    kv!("G.check", "scalar {:x} avx2 {:x} avx512 {:x} (equal = {})", r[0], r[1], r[2], r[0] == r[1] && (!has || r[1] == r[2]));
    let names = ["scalar", "avx2", "avx512"];
    for k in 0..3 {
        if k == 2 && !has { break; }
        let (a, _) = par(1, c1, |_| { let b = src.clone(); kern_for(k, &b, &tab, 300_000_000) });
        let (v, _) = par(n8, &t.cpus8, |_| { let b = src.clone(); kern_for(k, &b, &tab, 300_000_000) });
        kv!(format!("G.kernel.{}", names[k]), "{:.3} GB/s 1T, {:.3} GB/s per thread 8T ({:.2} GB/s total)", a[0] / 1e9, v.iter().sum::<f64>() / n8 as f64 / 1e9, v.iter().sum::<f64>() / 1e9);
    }
    if has {
        let edges = [0.0, 1.0, 2.0, 5.0, 10.0, 50.0];
        for (tn, cpus) in [("t1", c1.to_vec()), ("t8", t.cpus8.clone())] {
            let (v, _) = par(cpus.len(), &cpus, |_| { let b = src.clone(); kern_for(2, &b, &tab, 200_000_000); kern_windows(0, &b, &tab, &edges) });
            let avg: Vec<f64> = (0..5).map(|w| v.iter().map(|x| x.get(w).cloned().unwrap_or(f64::NAN)).sum::<f64>() / v.len() as f64).collect();
            kv!(format!("G.scalar_after_avx512.{tn}"), "{} GB/s per thread in windows 0-1,1-2,2-5,5-10,10-50 ms", fmt_v(&avg));
        }
    }
    // clock: dependent add chain (1 add per cycle), alone and with independent vector work
    let modes = ["scalar", "ymm_int", "ymm_fma", "zmm_int", "zmm_fma"];
    for m in 0..5u32 {
        if m >= 3 && !has { break; }
        let (a, _) = par(1, c1, |_| chain_for(m, 60_000_000));
        let (v, _) = par(n8, &t.cpus8, |_| chain_for(m, 60_000_000));
        kv!(format!("G.ghz.{}", modes[m as usize]), "{:.3} GHz 1T, {:.3} GHz 8T avg (min {:.3})", a[0], v.iter().sum::<f64>() / n8 as f64, minf(&v));
    }
    if has {
        let edges = [0.0, 0.5, 1.0, 2.0, 5.0, 10.0];
        for (m, tn, cpus) in [(3u32, "zmm_int.t1", c1.to_vec()), (3, "zmm_int.t8", t.cpus8.clone()), (4, "zmm_fma.t8", t.cpus8.clone())] {
            let (v, _) = par(cpus.len(), &cpus, |_| { chain_for(m, 100_000_000); chain_windows(&edges) });
            let avg: Vec<f64> = (0..5).map(|w| v.iter().map(|x| x.get(w).cloned().unwrap_or(f64::NAN)).sum::<f64>() / v.len() as f64).collect();
            kv!(format!("G.linger.{tn}"), "{} GHz scalar in windows 0-0.5,0.5-1,1-2,2-5,5-10 ms after", fmt_v(&avg));
        }
    }
    kv!("G.ms", "{:.1} ms", ms(now() - t0));
}
fn sec_h(t: &Topo, data: &str) {
    let t0 = now();
    let src = make_buf(data);
    let mut tab = [0u32; 256]; for i in 0..256 { tab[i] = splitmix(i as u64) as u32; }
    for k in [0usize, 1] {
        for (tn, cpus) in [("t1", t.cpus8[..1].to_vec()), ("t4", t.cpus4.clone()), ("t8", t.cpus8.clone())] {
            if cpus.is_empty() { continue; }
            let (v, _) = par(cpus.len(), &cpus, |_| { let b = src.clone(); kern_for(k, &b, &tab, 200_000_000) });
            kv!(format!("H.{}.{tn}", if k == 0 { "scalar" } else { "avx2" }), "{:.2} GB/s total, {:.3} per thread", v.iter().sum::<f64>() / 1e9, v.iter().sum::<f64>() / v.len() as f64 / 1e9);
        }
    }
    kv!("H.ms", "{:.1} ms", ms(now() - t0));
}

// ------------------------------------------------------------------ I: output writes
fn write_file_raw(path: &str, len: usize, block: usize, buf: &[u8]) -> Result<(f64, f64), i32> {
    let c = cs(path);
    let a = now();
    let fd = unsafe { open(c.as_ptr() as *const i8, 0o1 | 0o100 | 0o1000, 0o644) };
    if fd < 0 { return Err(en()); }
    let mut o = 0; let mut err = 0;
    while o < len { let w = block.min(len - o); let r = unsafe { write(fd, buf.as_ptr(), w) }; if r <= 0 { err = en(); break; } o += r as usize; }
    let b = now();
    unsafe { close(fd) };
    let cl = now() - b;
    if err != 0 { return Err(err); }
    Ok((ms(now() - a), ms(cl)))
}
fn sec_i(t: &Topo, out: &str, scratch: &str) {
    let t0 = now();
    let buf = vec![0x5au8; MB];
    let n8 = t.cpus8.len().max(1);
    let (r, w) = par(8, &t.cpus8, |i| match write_file_raw(&format!("{out}/.probe_w{i}.bin"), 8 * MB, MB, &buf) { Ok(x) => (0, x.1), Err(e) => (e, 0.0) });
    let errs: Vec<i32> = r.iter().map(|x| x.0).filter(|&e| e != 0).collect();
    if errs.is_empty() { kv!("I.out_8x8MB", "{:.2} ms wall (8 threads, 1 MB writes), max close {:.3} ms", w * 1e3, r.iter().map(|x| x.1).fold(0.0, f64::max)); }
    else { kv!("I.out_8x8MB", "ERR {}", errs[0]); }
    let a = now(); let mut ue = 0; for i in 0..8 { if unsafe { unlink(cs(&format!("{out}/.probe_w{i}.bin")).as_ptr()) } != 0 { ue = en(); } }
    kv!("I.out_unlink8", "{:.3} ms (errno {})", ms(now() - a), ue);
    for (key, dir, size) in [("I.out_1x64MB", out, 64 * MB), ("I.scratch_1x64MB", scratch, 64 * MB), ("I.tmp_1x32MB", "/tmp", 32 * MB)] {
        let p = format!("{dir}/.probe_big.bin");
        match write_file_raw(&p, size, MB, &buf) {
            Ok((tt, cl)) => { let a = now(); let r = unsafe { unlink(cs(&p).as_ptr()) }; kv!(key, "{:.2} ms ({:.2} GB/s), close {:.3} ms, unlink {:.3} ms (rc {})", tt, size as f64 / tt / 1e6, cl, ms(now() - a), r); }
            Err(e) => kv!(key, "ERR {}", e),
        }
    }
    kv!("I.ms", "{:.1} ms", ms(now() - t0));
}

// ------------------------------------------------------------------ K: parse kernels
fn load_table(dir: &str, sub: &str) -> Vec<u8> {
    let mut names: Vec<PathBuf> = match std::fs::read_dir(format!("{dir}/{sub}")) { Ok(r) => r.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "csv") && p.is_file()).collect(), Err(_) => vec![] };
    names.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    let mut out = Vec::new();
    for p in names {
        let Ok(data) = std::fs::read(&p) else { continue };
        let Some(nl) = data.iter().position(|&b| b == b'\n') else { continue };
        let body = &data[nl + 1..];
        if body.is_empty() { continue; }
        if !out.is_empty() && *out.last().unwrap_or(&b'\n') != b'\n' { out.push(b'\n'); }
        out.extend_from_slice(body);
    }
    out
}
#[cfg(not(feature = "nok"))]
struct KCtx<'a> { buf: &'a [u8], pieces: Vec<(usize, usize)>, lines: usize, cpus: Vec<usize>, ref_chk: (u64, u64) }
#[cfg(not(feature = "nok"))]
fn kbench<K: iface::Kernel>(c: &KCtx) -> (u64, u64) {
    use sinks::Checksum;
    let buf = c.buf;
    let (r, _) = par(1, &c.cpus[..1], |_| {
        let mut best = u64::MAX; let mut chk = (0u64, 0u64); let st = now(); let mut k = 0;
        while k < 7 || (k < 40 && now() - st < 250_000_000) {
            let mut s = Checksum::default(); let a = now(); K::events(black_box(buf), &mut s); let d = now() - a;
            best = best.min(d); chk = (s.n, s.h); k += 1;
        }
        (best, chk, k)
    });
    let (best1, chk, k1) = r[0];
    let n = c.pieces.len();
    let passes = ((200_000_000u64 / (best1 / n as u64).max(1)) as usize).clamp(5, 60);
    let bar = SpinBarrier::new(n);
    let (rr, _) = par(n, &c.cpus, |i| {
        let (a, b) = c.pieces[i]; let mut best = u64::MAX; let mut rows = 0u64;
        for _ in 0..passes {
            bar.wait(); let t = now();
            let mut s = Checksum::default(); K::events(black_box(&buf[a..b]), &mut s); rows = s.n;
            bar.wait(); best = best.min(now() - t);
        }
        (best, rows)
    });
    let best8 = rr.iter().map(|x| x.0).max().unwrap_or(0);
    let rows8: u64 = rr.iter().map(|x| x.1).sum();
    let ok = if c.ref_chk.0 == 0 { "" } else if chk == c.ref_chk { " same as ref" } else { " DIFFERS FROM REF" };
    kv!(format!("K.{}.t1", K::NAME), "{:.3} ns/line (min of {} passes, {:.2} ms; rows {} chk {:016x}{})", best1 as f64 / c.lines as f64, k1, ms(best1), chk.0, chk.1, ok);
    kv!(format!("K.{}.t8", K::NAME), "{:.3} ns/line (min of {} passes, {:.3} ms, {:.2}x of 1T; rows {}{})", best8 as f64 / c.lines as f64, passes, ms(best8), best1 as f64 / best8.max(1) as f64, rows8, if rows8 == chk.0 { "" } else { " ROWS DIFFER" });
    chk
}
#[cfg(feature = "nok")]
fn sec_k(_t: &Topo, _data: &str) { kv!("K.skip", "built without parse kernels (feature nok)"); }
#[cfg(not(feature = "nok"))]
fn sec_k(t: &Topo, data: &str) {
    let t0 = now();
    let content = load_table(data, "events");
    if content.is_empty() { kv!("K.load", "ERR no events"); return; }
    let len = content.len();
    let mut padded = vec![0u8; len + 256];
    padded[64..64 + len].copy_from_slice(&content);
    drop(content);
    let buf = &padded[64..64 + len];
    let lines = buf.iter().filter(|&&b| b == b'\n').count() + usize::from(*buf.last().unwrap_or(&b'\n') != b'\n');
    let n = t.cpus8.len().max(1);
    let mut pieces = vec![]; let mut s = 0usize;
    for i in 1..=n {
        let mut e = if i == n { len } else { (len * i / n).max(s) };
        if i < n { while e < len && buf[e - 1] != b'\n' { e += 1; } }
        pieces.push((s, e)); s = e;
    }
    kv!("K.load", "{:.1} MB {} lines, {} pieces, load {:.1} ms", len as f64 / 1e6, lines, n, ms(now() - t0));
    let mut c = KCtx { buf, pieces, lines, cpus: t.cpus8.clone(), ref_chk: (0, 0) };
    let has = HAS512.load(Relaxed);
    c.ref_chk = kbench::<k_ref::Ref>(&c);
    macro_rules! kb { ($k:ty) => { if el_s() < 52.0 { step(12); kbench::<$k>(&c); } else { kv!("K.skip", "budget"); } }; }
    kb!(k_ymm::Ymm);
    if has { kb!(k_free::Free256); kb!(k_free::FreeC256); kb!(k_free::Free); kb!(k_free::FreeC); kb!(k_zmm::Zmm); kb!(k_zmm1::Zmm1); } else { kv!("K.avx512_kernels", "skipped, no avx512bw"); }
    kv!("K.ms", "{:.1} ms", ms(now() - t0));
}

// ------------------------------------------------------------------ end summary
fn summary(tag: &str) {
    let r = rusage();
    kv!(format!("{tag}.rusage"), "user {:.3} s sys {:.3} s maxrss {} MB minflt {} majflt {} inblock {} nvcsw {} nivcsw {}", r[0] as f64 + r[1] as f64 * 1e-6, r[2] as f64 + r[3] as f64 * 1e-6, r[4] / 1024, r[8], r[9], r[11], r[16], r[17]);
    let st = rd("/proc/self/status").unwrap_or_default();
    let g = |k: &str| st.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_once(':')).map(|x| x.1.trim().to_string()).unwrap_or_default();
    kv!(format!("{tag}.status"), "VmHWM {} VmRSS {} Threads {}", g("VmHWM"), g("VmRSS"), g("Threads"));
    kv!(format!("{tag}.cg.memory"), "peak {} current {}", rdl("/sys/fs/cgroup/memory.peak"), rdl("/sys/fs/cgroup/memory.current"));
    kv!(format!("{tag}.cg.cpu.stat"), "{}", rdl("/sys/fs/cgroup/cpu.stat").replace('\n', " "));
    kv!(format!("{tag}.stat.cpu"), "{}", rd("/proc/stat").unwrap_or_default().lines().next().unwrap_or(""));
    kv!(format!("{tag}.elapsed"), "{:.3} s", el_s());
}

// ------------------------------------------------------------------ main
fn main() {
    START.store(now(), Relaxed);
    for s in [4, 6, 7, 8, 11] { unsafe { signal(s, on_sig as extern "C" fn(i32) as usize) }; } // ILL ABRT BUS FPE SEGV
    let data = std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into());
    let out = std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into());
    let scratch = std::env::var("SCRATCH_DIR").unwrap_or_else(|_| "/scratch".into());
    let only: Option<String> = std::env::var("PROBE_ONLY").ok();
    let wd: f64 = std::env::var("PROBE_WATCHDOG_S").ok().and_then(|s| s.parse().ok()).unwrap_or(55.0);
    let lim: f64 = std::env::var("PROBE_BUDGET_S").ok().and_then(|s| s.parse().ok()).unwrap_or(40.0);
    build_tables(&out);
    watchdog(wd);
    kv!("probe.version", "3");
    let run = |s: &str| only.as_ref().map(|o| o.split(',').any(|x| x == s)).unwrap_or(true);
    step(1); if run("B") { sec_b(&data); }
    let t = topo();
    if t.cpus8.is_empty() { kv!("topo", "ERR no cpus from sched_getaffinity"); }
    let t = if t.cpus8.is_empty() { Topo { allowed: vec![0], cores: vec![vec![0]], cpus8: vec![], cpus4: vec![], source: "none" } } else { t };
    step(2); if run("A") { sec_a(&t); }
    step(3); if run("J") && budget("J", lim) { sec_j(&t); }
    step(4); if run("C") && budget("C", lim) { sec_c(&t, &data); }
    step(5); if run("D") && budget("D", lim) { sec_d(&t); }
    step(6); if run("E") && budget("E", lim) { sec_e(&t); }
    step(7); if run("F") && budget("F", lim) { sec_f(&t); }
    step(8); if run("G") && budget("G", lim) { sec_g(&t, &data); }
    step(9); if run("H") && budget("H", lim) { sec_h(&t, &data); }
    step(10); if run("I") && budget("I", lim) { sec_i(&t, &out, &scratch); }
    summary("Y");
    step(11);
    let r = write_tables_raw();
    kv!("T.tables", "{}", if r == [0, 0] { "ok".to_string() } else { format!("ERR {} {}", r[0], r[1]) });
    step(12); if run("K") && budget("K", lim + 8.0) { HAS512.store(std::is_x86_feature_detected!("avx512bw"), Relaxed); sec_k(&t, &data); }
    step(13);
    kv!("Z.elapsed", "{:.3} s", el_s());
    unsafe { _exit(0) }
}
