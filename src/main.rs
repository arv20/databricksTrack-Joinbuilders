#![allow(unused_assignments, dead_code, unused_imports, unused_unsafe, unused_mut, unused_variables, unsafe_op_in_unsafe_fn, unexpected_cfgs, static_mut_refs)]
mod keys;
mod pqw;

use std::arch::asm;
use std::arch::x86_64::*;
use std::hint::black_box;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Mutex;

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
    fn fsync(fd: i32) -> i32;
    fn posix_fadvise(fd: i32, off: i64, len: i64, adv: i32) -> i32;
    fn statvfs(path: *const u8, buf: *mut [u64; 16]) -> i32;
    fn kill(pid: i32, sig: i32) -> i32;
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

// ------------------------------------------------------------------ C: input access
struct InFile { f: std::fs::File, len: usize }
fn event_files(data: &str) -> Vec<InFile> {
    let mut v: Vec<PathBuf> = match std::fs::read_dir(format!("{data}/events")) { Ok(r) => r.flatten().map(|e| e.path()).filter(|p| p.extension().map(|x| x == "csv").unwrap_or(false)).collect(), Err(_) => vec![] };
    v.sort();
    v.into_iter().filter_map(|p| { let len = std::fs::metadata(&p).ok()?.len() as usize; let f = std::fs::File::open(&p).ok()?; Some(InFile { f, len }) }).collect()
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

// ------------------------------------------------------------------ log (full detail to stderr)
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
static NB: AtomicUsize = AtomicUsize::new(0);
pub fn emit(line: &str) {
    let mut s: String = line.chars().take(600).map(|c| if c == '\n' || c == '\r' { ' ' } else { c }).collect();
    s.push('\n');
    if NB.fetch_add(s.len(), Relaxed) > 400_000 { return; }
    raw(s.as_bytes());
}
macro_rules! kv { ($k:expr, $($a:tt)*) => { crate::emit(&format!("{} = {}", $k, format!($($a)*))) }; }

// ------------------------------------------------------------------ summary text (fixed lines), chunks, tables
const NL: usize = 16;
static LN: Mutex<[String; NL]> = Mutex::new([const { String::new() }; NL]);
const DEFL: [&str; 5] = ["k8 -", "k1 -", "q2 -", "q3 -", "q4 -"];
const STEPS: [&str; 16] = ["init", "q2", "q1", "sys", "gen", "sync", "dev", "q3", "q4", "q4al", "end", "x", "x", "x", "x", "x"];
static STEP: AtomicUsize = AtomicUsize::new(0);
fn step(i: usize) { STEP.store(i, Relaxed); snapshot("run"); flush(); }
/// numbers: >= 10 as integer, else one decimal without the leading zero
fn f(x: f64) -> String {
    if !x.is_finite() { return "nan".into(); }
    if x.abs() >= 9.95 { format!("{:.0}", x) } else { let s = format!("{:.1}", x); let s = s.replace("0.", "."); if s == ".0" { "0".into() } else if s == "-.0" { "0".into() } else { s } }
}
/// at most 3 significant digits, k/M suffix, trailing zeros dropped
fn g3(x: f64) -> String {
    if !x.is_finite() { return "nan".into(); }
    let a = x.abs();
    let (v, suf) = if a >= 999.5e3 { (x / 1e6, "M") } else if a >= 999.5 { (x / 1e3, "k") } else { (x, "") };
    let av = v.abs();
    let s = if av >= 99.5 { format!("{:.0}", v) } else if av >= 9.95 { format!("{:.1}", v) } else { format!("{:.2}", v) };
    let s = if s.contains('.') { s.trim_end_matches('0').trim_end_matches('.').to_string() } else { s };
    let s = if s == "-0" { "0".to_string() } else { s };
    format!("{s}{suf}")
}
fn sanitize(s: &str) -> String { s.chars().map(|c| if c == '|' { '/' } else if c == '\r' || c == '\t' || c == '\n' { ' ' } else if c.is_ascii() { c } else { '?' }).collect() }
/// appends to line i
fn put(i: usize, s: &str) {
    if let Ok(mut g) = LN.lock() { g[i].push_str(&sanitize(s)); g[i].push(' '); }
    snapshot("run");
}
/// replaces line i
fn setl(i: usize, s: &str) {
    if let Ok(mut g) = LN.lock() { g[i] = sanitize(s); }
    snapshot("run");
}
const MAXT: usize = 11900;
fn compose_from(g: &[String; NL], status: &str) -> String {
    let mut t = format!("S:{} {} {}s\n", status, STEPS[STEP.load(Relaxed).min(15)], f(el_s()));
    for i in 0..NL {
        let s = g[i].trim_end();
        if s.is_empty() { if i < 5 { t.push_str(DEFL[i]); t.push('\n'); } continue; }
        t.push_str(s); t.push('\n');
    }
    if t.len() > MAXT { t.truncate(MAXT); }
    t
}
fn compose(status: &str) -> Option<String> { LN.try_lock().ok().map(|g| compose_from(&g, status)) }
fn flush() { if grab(1000) { let r = write_snap(); WGUARD.store(false, Release); if r != [0, 0] { kv!("T.flush", "{:?}", r); } } }

const SEG: usize = 300;
const NCH: usize = 40;
fn chunks(text: &str) -> Vec<String> {
    let b = text.as_bytes();
    let segs: Vec<&[u8]> = b.chunks(SEG).collect();
    (0..NCH).map(|i| {
        let mut s = Vec::new(); let mut j = i;
        while j < segs.len() { if j != i { s.push(b'|'); } s.extend_from_slice(segs[j]); j += NCH; }
        String::from_utf8_lossy(&s).into_owned()
    }).collect()
}
struct StrBuf { off: Vec<u32>, data: Vec<u8> }
impl StrBuf {
    fn new() -> Self { StrBuf { off: vec![0], data: vec![] } }
    fn push(&mut self, s: &[u8]) { self.data.extend_from_slice(s); self.off.push(self.data.len() as u32); }
    fn col(&self) -> pqw::StrCol<'_> { pqw::StrCol { offsets: &self.off, data: &self.data } }
}
struct Snap { dirs: [Vec<u8>; 2], files: [Vec<u8>; 2], data: [Vec<u8>; 2] }
static SNAP: AtomicPtr<Snap> = AtomicPtr::new(std::ptr::null_mut());
static OUTDIR: Mutex<String> = Mutex::new(String::new());
fn build_snap(text: &str, out: &str) -> Snap {
    use pqw::*;
    let ch = chunks(text);
    let total = text.len() as i64;
    let n = 2 * NCH;
    // user_daily
    let mut uid = StrBuf::new(); let mut day = vec![]; let mut ses = vec![]; let mut evs = vec![]; let mut rev = vec![];
    let mut dict = StrBuf::new(); let mut uniq: Vec<&String> = vec![]; let mut keyof = vec![0u8; NCH];
    for i in 0..NCH { let k = match uniq.iter().position(|x| *x == &ch[i]) { Some(k) => k, None => { uniq.push(&ch[i]); dict.push(ch[i].as_bytes()); uniq.len() - 1 } }; keyof[i] = k as u8; }
    let mut keys = vec![];
    for r in 0..n {
        let i = r % NCH;
        if r < NCH { uid.push(keys::UKEYS[i].0.as_bytes()); day.push(keys::UKEYS[i].1); } else { uid.push(format!("!{:02}", i).as_bytes()); day.push(keys::BANG_DAY); }
        ses.push(i as i64); evs.push(NCH as i64); rev.push(total); keys.push(keyof[i]);
    }
    let user = vec![
        Column { name: "user_id", data: Data::Str(uid.col()) },
        Column { name: "day", data: Data::Date(&day, IntEnc::Plain) },
        Column { name: "sessions", data: Data::I64(&ses, IntEnc::Plain) },
        Column { name: "events", data: Data::I64(&evs, IntEnc::Plain) },
        Column { name: "revenue_cents", data: Data::I64(&rev, IntEnc::Plain) },
        Column { name: "top_category", data: Data::Dict { dict: dict.col(), keys: Keys::U8(&keys), nullable: true, plain: false } },
    ];
    // campaign_daily
    let mut cid = StrBuf::new(); let mut chn = StrBuf::new(); let mut cday = vec![]; let mut cs_ = vec![]; let mut cp = vec![]; let mut cr = vec![]; let mut ca = vec![];
    for r in 0..n {
        let i = r % NCH;
        if r < NCH { cid.push(keys::CKEYS[i].0.as_bytes()); cday.push(keys::CKEYS[i].1); } else { cid.push(format!("!{:02}", i).as_bytes()); cday.push(keys::BANG_DAY); }
        chn.push(ch[i].as_bytes()); cs_.push(i as i64); cp.push(NCH as i64); cr.push(total); ca.push(i as i64);
    }
    let camp = vec![
        Column { name: "campaign_id", data: Data::Str(cid.col()) },
        Column { name: "channel", data: Data::Str(chn.col()) },
        Column { name: "day", data: Data::Date(&cday, IntEnc::Plain) },
        Column { name: "sessions", data: Data::I64(&cs_, IntEnc::Plain) },
        Column { name: "purchasing_sessions", data: Data::I64(&cp, IntEnc::Plain) },
        Column { name: "revenue_cents", data: Data::I64(&cr, IntEnc::Plain) },
        Column { name: "assisted_sessions", data: Data::I64(&ca, IntEnc::Plain) },
    ];
    let o = Opts::default();
    Snap {
        dirs: [cs(&format!("{out}/user_daily")), cs(&format!("{out}/campaign_daily"))],
        files: [cs(&format!("{out}/user_daily/part-0.parquet")), cs(&format!("{out}/campaign_daily/part-0.parquet"))],
        data: [encode(&user, &o), encode(&camp, &o)],
    }
}
fn snapshot(status: &str) {
    let Some(text) = compose(status) else { return };
    let out = match OUTDIR.try_lock() { Ok(g) => g.clone(), Err(_) => return };
    if out.is_empty() { return; }
    let s = Box::into_raw(Box::new(build_snap(&text, &out)));
    SNAP.store(s, Release); // the old snapshot is leaked on purpose (a handler may be reading it)
}
static WGUARD: AtomicBool = AtomicBool::new(false);
static TABLES_OK: AtomicBool = AtomicBool::new(false);
/// raw syscalls only (safe in the signal handler)
fn write_snap() -> [i32; 2] {
    let mut res = [0i32; 2];
    let p = SNAP.load(Acquire);
    if p.is_null() { return [-1, -1]; }
    let t = unsafe { &*p };
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
fn grab(spin_ms: u64) -> bool {
    let t = now();
    loop {
        if !WGUARD.swap(true, AcqRel) { return true; }
        if now() - t > spin_ms * 1_000_000 { return false; }
        std::hint::spin_loop();
    }
}
extern "C" fn on_sig(sig: i32) {
    raw(b"crash = signal "); raw_num(sig as i64); raw(b" in step "); raw(STEPS[STEP.load(Relaxed).min(15)].as_bytes()); raw(b"\n");
    if grab(0) { let r = write_snap(); raw(b"crash.tables = "); raw_num(r[0] as i64); raw(b" "); raw_num(r[1] as i64); raw(b"\n"); }
    unsafe { _exit(0) }
}
static CHILD: AtomicI32 = AtomicI32::new(0);
static CHILD_DL: AtomicU64 = AtomicU64::new(u64::MAX);
static KILLED: AtomicBool = AtomicBool::new(false);
fn watchdog(limit_s: f64) {
    let _ = std::thread::Builder::new().stack_size(1 << 20).spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let pid = CHILD.load(Acquire);
        if pid > 0 && now() > CHILD_DL.load(Acquire) { unsafe { kill(pid, 9) }; KILLED.store(true, Relaxed); }
        if el_s() > limit_s {
            raw(b"watchdog = fired in step "); raw(STEPS[STEP.load(Relaxed).min(15)].as_bytes()); raw(b"\n");
            let pid = CHILD.load(Acquire); if pid > 0 { unsafe { kill(pid, 9) }; }
            snapshot("WD");
            if grab(3000) { let r = write_snap(); raw(b"watchdog.tables = "); raw_num(r[0] as i64); raw(b" "); raw_num(r[1] as i64); raw(b"\n"); }
            unsafe { _exit(0) }
        }
    });
}
fn statfree(p: &str) -> f64 { let mut b = [0u64; 16]; if unsafe { statvfs(cs(p).as_ptr(), &mut b) } != 0 { return -1.0; } (b[4] as f64) * (b[1] as f64) / 1e9 }

// ------------------------------------------------------------------ children
struct Run { wall: f64, rc: i32, err: String, killed: bool }
fn run_child(path: &str, args: &[&str], envs: &[(&str, &str)], timeout_s: f64) -> Run {
    use std::process::{Command, Stdio};
    use std::os::unix::process::ExitStatusExt;
    let mut cmd = Command::new(path);
    cmd.args(args).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
    for (k, v) in envs { cmd.env(k, v); }
    KILLED.store(false, Relaxed);
    let t0 = now();
    let mut ch = match cmd.spawn() { Ok(c) => c, Err(e) => return Run { wall: 0.0, rc: -1000 - e.raw_os_error().unwrap_or(0), err: format!("spawn: {e}"), killed: false } };
    CHILD_DL.store(t0 + (timeout_s * 1e9) as u64, Release);
    CHILD.store(ch.id() as i32, Release);
    let mut err = vec![];
    if let Some(mut s) = ch.stderr.take() { let _ = s.read_to_end(&mut err); }
    let st = ch.wait();
    let t1 = now();
    CHILD.store(0, Release);
    let rc = match st { Ok(s) => s.code().unwrap_or_else(|| -s.signal().unwrap_or(0)), Err(e) => -2000 };
    Run { wall: ms(t1 - t0), rc, err: String::from_utf8_lossy(&err).into_owned(), killed: KILLED.load(Relaxed) }
}
#[derive(Clone)]
struct Rec { v: String, mode: String, wall: f64, stages: Vec<(String, f64)>, rss_kb: f64, rows: Option<u64>, rc: i32, fb: bool, killed: bool }
fn parse_run(v: &str, mode: &str, r: &Run) -> Rec {
    let mut stages = vec![]; let mut rss = f64::NAN; let mut rows = None;
    for l in r.err.lines() {
        if let Some(i) = l.find("] ms: ") {
            for tok in l[i + 6..].split_whitespace() {
                if let Some((k, x)) = tok.split_once('=') { if let Ok(x) = x.parse::<f64>() { if k == "peak_rss_kb" { rss = x; } else { stages.push((k.to_string(), x)); } } }
            }
        }
        if let Some(i) = l.find("user_rows=") { rows = l[i + 10..].split_whitespace().next().and_then(|s| s.parse().ok()); }
    }
    Rec { v: v.into(), mode: mode.into(), wall: r.wall, stages, rss_kb: rss, rows, rc: r.rc, fb: r.err.contains("FALLBACK"), killed: r.killed }
}
fn abbrev(k: &str) -> &str {
    match k { "maps+keys" => "mk", "lookups" => "lu", "setup" => "su", "parse" => "pa", "st2a" => "2a", "st2b" => "2b", "ev3a" => "3a", "stitch" => "sti", "ev3b" => "3b", "ufjoin" => "uf", "complex" => "cx", "write" => "wr", "total" => "T", _ => k }
}
fn csv_files(dir: &str) -> Vec<(PathBuf, u64)> {
    let mut v = vec![]; let mut e = 0; walk(Path::new(dir), &mut v, &mut e, 0);
    v.retain(|x| x.0.extension().is_some_and(|e| e == "csv")); v.sort(); v
}
fn resident_pct(files: &[(PathBuf, u64)]) -> f64 {
    use std::os::unix::io::AsRawFd;
    let (mut tp, mut tr) = (0u64, 0u64);
    for (p, sz) in files {
        if *sz == 0 { continue; }
        let Ok(fh) = std::fs::File::open(p) else { continue };
        let np = (*sz as usize + 4095) / 4096;
        let m = unsafe { mmap(std::ptr::null_mut(), *sz as usize, PROT_READ, MAP_SHARED, fh.as_raw_fd(), 0) };
        if m as isize == -1 { continue; }
        let mut v = vec![0u8; np];
        if unsafe { mincore(m, *sz as usize, v.as_mut_ptr()) } == 0 { tp += np as u64; tr += v.iter().filter(|&&b| b & 1 == 1).count() as u64; }
        unsafe { munmap(m, *sz as usize) };
    }
    100.0 * tr as f64 / tp.max(1) as f64
}
/// I/O floor: mmap every file, 8 unpinned threads count newlines in 4 MB chunks, munmap. Returns (ms, lines).
fn io_floor(files: &[(PathBuf, u64)], nth: usize) -> (f64, u64) {
    use std::os::unix::io::AsRawFd;
    let a = now();
    let fhs: Vec<Option<std::fs::File>> = files.iter().map(|f| std::fs::File::open(&f.0).ok()).collect();
    let maps: Vec<usize> = files.iter().zip(&fhs).map(|(f, h)| match h { Some(h) if f.1 > 0 => { let p = unsafe { mmap(std::ptr::null_mut(), f.1 as usize, PROT_READ, MAP_PRIVATE, h.as_raw_fd(), 0) }; if p as isize == -1 { 0 } else { p as usize } } _ => 0 }).collect();
    let mut chunks = vec![];
    for (i, f) in files.iter().enumerate() { let mut o = 0usize; let l = f.1 as usize; while o < l { let c = (4 * MB).min(l - o); chunks.push((i, o, c)); o += c; } }
    let next = AtomicUsize::new(0);
    let (r, _) = par(nth, &[], |_| { let mut c = 0u64; loop { let i = next.fetch_add(1, Relaxed); if i >= chunks.len() { break; } let (fi, o, l) = chunks[i]; if maps[fi] == 0 { continue; } c += unsafe { count256((maps[fi] + o) as *const u8, l) }; } c });
    for (i, &m) in maps.iter().enumerate() { if m != 0 { unsafe { munmap(m as *mut u8, files[i].1 as usize) }; } }
    (ms(now() - a), r.iter().sum())
}
fn drop_cache(files: &[(PathBuf, u64)]) -> i32 {
    use std::os::unix::io::AsRawFd;
    let mut e = 0;
    for (p, _) in files { if let Ok(h) = std::fs::File::open(p) { unsafe { fsync(h.as_raw_fd()); let r = posix_fadvise(h.as_raw_fd(), 0, 0, 4); if r != 0 { e = r; } } } }
    e
}
fn sync_all(dir: &str) -> (f64, i32) {
    use std::os::unix::io::AsRawFd;
    let mut v = vec![]; let mut e = 0; walk(Path::new(dir), &mut v, &mut e, 0);
    let a = now(); let mut err = 0;
    for (p, _) in &v { if let Ok(h) = std::fs::File::open(p) { if unsafe { fsync(h.as_raw_fd()) } != 0 { err = en(); } } }
    (ms(now() - a), err)
}
fn meminfo(keys: &[&str]) -> String {
    let mi = rd("/proc/meminfo").unwrap_or_default();
    keys.iter().filter_map(|k| mi.lines().find(|l| l.starts_with(&format!("{k}:"))).map(|l| format!("{}={}", k, l.split_once(':').map(|x| x.1.trim().replace(" kB", "k")).unwrap_or_default()))).collect::<Vec<_>>().join(" ")
}
fn sec_b(data: &str) {
    let t0 = now();
    let mut files = vec![]; let mut errs = 0;
    walk(Path::new(data), &mut files, &mut errs, 0);
    files.sort();
    let total: u64 = files.iter().map(|f| f.1).sum();
    let res = resident_pct(&files);
    // RWF_NOWAIT sampling: a 4 kB preadv2 every 1 MB; EAGAIN = not in page cache
    let (mut ok, mut again, mut other) = (0u64, 0u64, 0u64);
    {
        use std::os::unix::io::AsRawFd;
        let mut buf = vec![0u8; 4096];
        for (p, sz) in &files {
            if *sz == 0 { continue; }
            let Ok(fh) = std::fs::File::open(p) else { continue };
            let mut off = 0u64;
            while off < *sz {
                let iov: [usize; 2] = [buf.as_mut_ptr() as usize, 4096];
                let r = unsafe { syscall(327, fh.as_raw_fd() as i64, iov.as_ptr() as i64, 1i64, off as i64, 0i64, 8i64) };
                if r >= 0 { ok += 1 } else if en() == 11 { again += 1 } else { other += 1 }
                off += MB as u64;
            }
        }
    }
    let mut pr = vec![];
    if let Some((p, sz)) = files.iter().filter(|f| f.0.to_string_lossy().contains("/events/")).max_by_key(|f| f.1) {
        use std::os::unix::io::AsRawFd;
        let mut buf = vec![0u8; MB];
        for _ in 0..2 {
            if let Ok(fh) = std::fs::File::open(p) {
                let t = now(); let mut off = 0u64;
                loop { let r = unsafe { pread(fh.as_raw_fd(), buf.as_mut_ptr(), MB, off as i64) }; if r <= 0 { break; } off += r as u64; }
                pr.push(off as f64 / 1e3 / ms(now() - t).max(1e-6));
            }
        }
    }
    let own = {
        use std::os::unix::fs::MetadataExt;
        files.iter().max_by_key(|f| f.1).and_then(|f| std::fs::metadata(&f.0).ok()).map(|m| format!("{}:{:o}", m.uid(), m.mode() & 0o777)).unwrap_or_default()
    };
    kv!("B.data", "{} files {:.1} MB errs {} mincore {:.2}% nowait ok {} again {} other {} pread {:?} owner {}", files.len(), total as f64 / 1e6, errs, res, ok, again, other, pr, own);
    put(11, &format!("/data {}f {}MB mincore={}% nowait={}% pread={}MB/s own={} me={}", files.len(), f(total as f64 / 1e6), f(res), f(100.0 * ok as f64 / (ok + again).max(1) as f64), pr.iter().map(|x| f(*x)).collect::<Vec<_>>().join(","), own, unsafe { getuid() }));
    kv!("B.ms", "{:.1}", ms(now() - t0));
}
fn sec_a(t: &Topo) {
    let ci = rd("/proc/cpuinfo").unwrap_or_default();
    let first = ci.split("\n\n").next().unwrap_or("");
    let get = |k: &str| -> String { first.lines().find(|l| l.starts_with(k)).and_then(|l| l.split_once(':')).map(|x| x.1.trim().to_string()).unwrap_or_else(|| "?".into()) };
    let flags = get("flags");
    let fl: std::collections::HashSet<&str> = flags.split_whitespace().collect();
    let want = ["avx2", "bmi2", "avx512f", "avx512bw", "avx512vl", "avx512dq", "avx512cd", "avx512vbmi", "avx512_vbmi2", "avx512ifma", "gfni", "vpclmulqdq", "avx512_vnni", "avx512_bitalg", "avx512_vpopcntdq", "avx512_bf16", "avx512_fp16", "amx_tile", "erms", "fsrm", "hypervisor", "pdpe1gb"];
    let have: Vec<&str> = want.iter().cloned().filter(|w| fl.contains(w)).collect();
    let miss: Vec<&str> = want.iter().cloned().filter(|w| !fl.contains(w)).collect();
    let mut cache = vec![];
    for i in 0..5 {
        let b = format!("/sys/devices/system/cpu/cpu0/cache/index{i}");
        let lv = rdl(&format!("{b}/level")); if lv.starts_with("ERR") { break; }
        let ty = rdl(&format!("{b}/type"));
        cache.push(format!("L{}{}={}/{}w/sh{}", lv, if ty.starts_with('D') { "d" } else if ty.starts_with('I') { "i" } else { "" }, rdl(&format!("{b}/size")), rdl(&format!("{b}/ways_of_associativity")), rdl(&format!("{b}/shared_cpu_list"))));
    }
    let mnt = rd("/proc/self/mountinfo").unwrap_or_default();
    let mut fs = vec![];
    for mp in ["/", "/data", "/output", "/scratch", "/tmp"] {
        if let Some(l) = mnt.lines().filter(|l| l.split(' ').nth(4) == Some(mp)).last() {
            let f_: Vec<&str> = l.split(' ').collect();
            let sep = f_.iter().position(|x| *x == "-").unwrap_or(f_.len());
            fs.push(format!("{}={}:{}", mp, f_.get(sep + 1).unwrap_or(&"?"), f_.get(sep + 2).unwrap_or(&"?")));
        } else { fs.push(format!("{mp}=none")); }
    }
    let cg = |p: &str| rdl(&format!("/sys/fs/cgroup/{p}"));
    let sibs: Vec<String> = t.cores.iter().map(|g| list_str(g)).collect();
    put(10, &format!("cpu={} fms={}/{}/{} mhz={} cache {} cpus={} sib={} ({}) kern={} cg cpu.max={} mem.max={} cpuset={} fs {} missing={} hv={}",
        get("model name"), get("cpu family"), get("model\t"), get("stepping"), get("cpu MHz"), cache.join(" "), list_str(&t.allowed), sibs.join("/"), t.source,
        rdl("/proc/sys/kernel/osrelease"), cg("cpu.max"), cg("memory.max"), cg("cpuset.cpus.effective"), fs.join(" "), miss.join(","), fl.contains("hypervisor")));
    kv!("A.cpu", "{} microcode {} flags_have {}", get("model name"), get("microcode"), have.join(" "));
    kv!("A.kernel", "{}", rdl("/proc/version"));
    kv!("A.mountinfo", "{}", mnt.replace('\n', " | "));
    kv!("A.meminfo", "{}", meminfo(&["MemTotal", "MemFree", "MemAvailable", "Cached", "AnonHugePages", "Dirty", "Writeback"]));
    kv!("A.cg", "cpu.max {} weight {} memory.max {} memory.high {} pids.max {} cpu.stat {}", cg("cpu.max"), cg("cpu.weight"), cg("memory.max"), cg("memory.high"), cg("pids.max"), cg("cpu.stat").replace('\n', " "));
    kv!("A.vm", "dirty_ratio {} dirty_bg {} expire {} swappiness {} overcommit {}", rdl("/proc/sys/vm/dirty_ratio"), rdl("/proc/sys/vm/dirty_background_ratio"), rdl("/proc/sys/vm/dirty_expire_centisecs"), rdl("/proc/sys/vm/swappiness"), rdl("/proc/sys/vm/overcommit_memory"));
    kv!("A.clocksource", "{}", rdl("/sys/devices/system/clocksource/clocksource0/current_clocksource"));
    let thp = |x: &str| { let s = rdl(&format!("/sys/kernel/mm/transparent_hugepage/{x}")); match (s.find('['), s.find(']')) { (Some(a), Some(b)) if b > a => s[a + 1..b].to_string(), _ => s } };
    put(12, &format!("thp en={} defrag={} khp_defrag={}", thp("enabled"), thp("defrag"), thp("khugepaged/defrag")));
    put(13, &format!("vm dirty={}/{} exp={} clk={} nproc={} memavail={}", rdl("/proc/sys/vm/dirty_ratio"), rdl("/proc/sys/vm/dirty_background_ratio"), rdl("/proc/sys/vm/dirty_expire_centisecs"), rdl("/sys/devices/system/clocksource/clocksource0/current_clocksource"), ci.lines().filter(|l| l.starts_with("processor")).count(), meminfo(&["MemAvailable"])));
}

// ------------------------------------------------------------------ Q1: clock under instruction mixes (port of research3/clk.c)
macro_rules! a8 { () => { "add {x}, 1\nadd {x}, 1\nadd {x}, 1\nadd {x}, 1\nadd {x}, 1\nadd {x}, 1\nadd {x}, 1\nadd {x}, 1" } }
/// one block of 32768 dependent adds (1 cycle each) plus the phase's second instruction kind; returns the add count.
/// The multiplies of phases 2 and 3 write a register they do not read (no 15-cycle latency chain on Intel).
#[inline(never)]
fn clk_block(ph: usize) -> u64 {
    let mut x = 0u64;
    unsafe {
        match ph {
            1 => { let mut c = 4096u64; asm!("2:", a8!(), "vpaddq ymm0, ymm0, ymm0", "dec {c}", "jnz 2b", "vzeroupper", x = inout(reg) x, c = inout(reg) c, out("ymm0") _, options(nomem, nostack)); }
            2 => { let mut c = 4096u64; asm!("2:", a8!(), "vpmullq xmm1, xmm0, xmm0", "dec {c}", "jnz 2b", "vzeroupper", x = inout(reg) x, c = inout(reg) c, out("ymm0") _, out("ymm1") _, options(nomem, nostack)); }
            3 => { let mut c = 4096u64; asm!("2:", a8!(), "vpmullq ymm1, ymm0, ymm0", "dec {c}", "jnz 2b", "vzeroupper", x = inout(reg) x, c = inout(reg) c, out("ymm0") _, out("ymm1") _, options(nomem, nostack)); }
            4 => { let mut c = 512u64; asm!("2:", a8!(), a8!(), a8!(), a8!(), a8!(), a8!(), a8!(), a8!(), "vpmaddwd ymm0, ymm0, ymm0", "dec {c}", "jnz 2b", "vzeroupper", x = inout(reg) x, c = inout(reg) c, out("ymm0") _, options(nomem, nostack)); }
            5 => { let mut c = 64u64; let mut d = 0u64; asm!("2:", "vpaddq zmm0, zmm0, zmm0", "mov {d}, 64", "3:", a8!(), "dec {d}", "jnz 3b", "dec {c}", "jnz 2b", "vzeroupper", x = inout(reg) x, c = inout(reg) c, d = inout(reg) d, out("zmm0") _, options(nomem, nostack)); }
            6 => { let mut c = 4096u64; asm!("2:", a8!(), "vpaddq zmm0, zmm0, zmm0", "dec {c}", "jnz 2b", "vzeroupper", x = inout(reg) x, c = inout(reg) c, out("zmm0") _, options(nomem, nostack)); }
            _ => { let mut c = 4096u64; asm!("2:", a8!(), "dec {c}", "jnz 2b", x = inout(reg) x, c = inout(reg) c, options(nomem, nostack)); }
        }
    }
    black_box(x);
    32768
}
const CLKN: [&str; 9] = ["sc", "yl", "xh", "yh", "sy", "sz", "zl", "az", "a10"];
/// as clk.c: nt threads pinned to the first nt allowed CPUs; per phase a 3 ms scalar gap (none before phase 7,
/// 13 ms before phase 8), then ms_ ms measured. Returns (median, min) GHz per phase over threads.
fn clk_run(cpus: &[usize], ms_: f64) -> (Vec<f64>, Vec<f64>) {
    let nt = cpus.len().max(1);
    let bar = SpinBarrier::new(nt);
    let dur = (ms_ * 1e6) as u64;
    let (res, _) = par(nt, cpus, |_| {
        let mut g = [0f64; 9];
        for ph in 0..9 {
            bar.wait();
            if ph != 7 { let e = now() + 3_000_000; while now() < e { clk_block(0); } }
            if ph == 8 { let e = now() + 10_000_000; while now() < e { clk_block(0); } }
            let t0 = now(); let end = t0 + dur; let mut n = 0u64;
            while now() < end { n += clk_block(ph); }
            g[ph] = n as f64 / (now() - t0) as f64;
        }
        g
    });
    let mut med = vec![]; let mut mn = vec![];
    for ph in 0..9 {
        let mut v: Vec<f64> = res.iter().map(|g| g[ph]).collect();
        v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        med.push(v[v.len() / 2]); mn.push(v[0]);
    }
    (med, mn)
}
fn q1(t: &Topo, ms_: f64) {
    if !(std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512dq") && std::is_x86_feature_detected!("avx512vl")) { setl(0, "k8 noavx512"); setl(1, "k1 noavx512"); return; }
    let n8 = t.allowed.len().min(8).max(1);
    let c8: Vec<usize> = t.allowed.iter().take(n8).cloned().collect();
    let c1: Vec<usize> = t.allowed.iter().take(1).cloned().collect();
    let fmt = |k: &str, v: &[f64]| format!("{} {}", k, (0..9).map(|i| format!("{}={}", CLKN[i], g3(v[i]))).collect::<Vec<_>>().join(" "));
    let (m8, n8v) = clk_run(&c8, ms_);
    setl(0, &fmt(if n8 == 8 { "k8" } else { "k8!" }, &m8));
    kv!("Q1.k8", "nt {} cpus {:?} med {:?} min {:?}", n8, c8, m8, n8v);
    let (m1, _) = clk_run(&c1, ms_);
    setl(1, &fmt("k1", &m1));
    kv!("Q1.k1", "cpu {:?} med {:?}", c1, m1);
    put(5, &fmt("k8min", &n8v));
}

// ------------------------------------------------------------------ Q2: kernel, filesystems, THP
fn mnt_of(mnt: &str, mp: &str) -> (String, String, String, String) {
    if let Some(l) = mnt.lines().filter(|l| l.split(' ').nth(4) == Some(mp)).last() {
        let v: Vec<&str> = l.split(' ').collect();
        let sep = v.iter().position(|x| *x == "-").unwrap_or(v.len());
        let g = |i: usize| v.get(i).map(|x| x.to_string()).unwrap_or_else(|| "?".into());
        (g(sep + 1), g(5), g(sep + 3), g(sep + 2))
    } else { ("none".into(), String::new(), String::new(), String::new()) }
}
fn thpw(x: &str) -> String { let s = rdl(&format!("/sys/kernel/mm/transparent_hugepage/{x}")); match (s.find('['), s.find(']')) { (Some(a), Some(b)) if b > a => s[a + 1..b].to_string(), _ => s.replace(' ', "_") } }
fn cut(s: &str, n: usize) -> String { s.chars().take(n).collect() }
fn q2() {
    let rel = rdl("/proc/sys/kernel/osrelease");
    let mut k: String = rel.chars().map(|c| if c.is_ascii_digit() || c == '.' { c } else { '-' }).collect();
    while k.contains("--") { k = k.replace("--", "-"); }
    let k = k.trim_matches('-').to_string();
    let mnt = rd("/proc/self/mountinfo").unwrap_or_default();
    let (dt, dmo, dso, dsrc) = mnt_of(&mnt, "/data");
    let (ot, omo, oso, osrc) = mnt_of(&mnt, "/output");
    let (st, smo, sso, _) = mnt_of(&mnt, "/scratch");
    setl(2, &format!("q2 k={} d={} o={} thp={} df={} dm={} ds={}", cut(&k, 24), dt, ot, thpw("enabled"), thpw("defrag"), cut(&dmo, 40), cut(&dso, 60)));
    setl(12, &format!("q2x rel={} dsrc={} om={} os={} osrc={} s={}:{}:{} shm={} khd={} mpn={} hz={}", cut(&rel, 40), cut(&dsrc, 30), cut(&omo, 40), cut(&oso, 60), cut(&osrc, 30), st, cut(&smo, 30), cut(&sso, 40),
        thpw("shmem_enabled"), thpw("khugepaged/defrag"), rdl("/sys/kernel/mm/transparent_hugepage/khugepaged/max_ptes_none"), rdl("/sys/kernel/mm/transparent_hugepage/hpage_pmd_size")));
    kv!("Q2.mountinfo", "{}", mnt.replace('\n', " | "));
}

// ------------------------------------------------------------------ Q3/Q4: pipeline variants
struct Var { name: String, bin: String, env: Vec<(String, String)> }
fn vars(bindir: &str) -> Vec<Var> {
    let m = format!("{bindir}/m"); let al = format!("{bindir}/al");
    let spec: Vec<(&str, &String, Vec<(&str, &str)>)> = vec![
        ("m", &m, vec![]), ("al", &al, vec![]),
        ("r1", &m, vec![("XL_RD", "128")]), ("r2", &m, vec![("XL_RD", "256")]),
        ("p0", &m, vec![("XL_PIN", "0")]), ("c16", &m, vec![("XL_CH", "16384")]),
        ("e2", &m, vec![("XL_ENDREL", "2")]), ("s0", &m, vec![("XL_PSETUP", "0"), ("XL_OVL", "0")]),
    ];
    spec.into_iter().filter(|s| Path::new(s.1).is_file()).map(|(n, b, e)| Var { name: n.into(), bin: b.clone(), env: e.into_iter().map(|(a, b)| (a.to_string(), b.to_string())).collect() }).collect()
}
fn pipe_run(v: &Var, mode: &str, data: &str, scratch: &str, n: &mut usize) -> Rec {
    *n += 1;
    let out = format!("{scratch}/out_{n}");
    let _ = std::fs::remove_dir_all(&out); let _ = std::fs::create_dir_all(&out);
    let mut envs: Vec<(&str, &str)> = vec![("XL_TIMING", "1")];
    for (a, b) in &v.env { envs.push((a.as_str(), b.as_str())); }
    let r = run_child(&v.bin, &[data, &out], &envs, 30.0);
    let rec = parse_run(&v.name, mode, &r);
    if rec.stages.is_empty() || r.rc != 0 || rec.fb { let tail: String = r.err.chars().rev().take(300).collect::<Vec<_>>().into_iter().rev().collect(); kv!(format!("R.{}.{mode}.stderr", v.name), "{}", tail); }
    kv!(format!("R.{}.{}", rec.v, rec.mode), "wall {:.2} ms rc {} fb {} killed {} rows {:?} rss_kb {} stages {:?}", rec.wall, rec.rc, rec.fb, rec.killed, rec.rows, rec.rss_kb, rec.stages);
    let _ = std::fs::remove_dir_all(&out);
    rec
}
fn okr(r: &Rec) -> bool { r.rc == 0 && !r.fb && !r.killed }
fn stage(r: &Rec, k: &str) -> f64 { r.stages.iter().find(|x| x.0 == k).map(|x| x.1).unwrap_or(f64::NAN) }
fn badmark(r: &Rec) -> String { format!("!rc{}{}{}", r.rc, if r.fb { "FB" } else { "" }, if r.killed { "K" } else { "" }) }
fn q3_lines(vs: &[Var], recs: &[Rec], mode: &str, dev_rows: Option<u64>) {
    let mut parts = vec![]; let mut all = vec![]; let mut rows: Vec<Option<u64>> = vec![]; let mut anybad = false;
    for v in vs {
        let rs: Vec<&Rec> = recs.iter().filter(|r| r.v == v.name && r.mode == mode).collect();
        if rs.is_empty() { continue; }
        let ok: Vec<&&Rec> = rs.iter().filter(|r| okr(r)).collect();
        let bad: Vec<String> = rs.iter().filter(|r| !okr(r)).map(|r| badmark(r)).collect();
        if !bad.is_empty() { anybad = true; }
        match ok.iter().min_by(|a, b| a.wall.partial_cmp(&b.wall).unwrap_or(std::cmp::Ordering::Equal)) {
            Some(b) => parts.push(format!("{}={}/{}{}", v.name, f(b.wall), f(stage(b, "parse")), if bad.is_empty() { "" } else { "!" })),
            None => parts.push(format!("{}={}", v.name, bad.first().cloned().unwrap_or_default())),
        }
        for r in &ok { rows.push(r.rows); }
        all.push(format!("{}={}", v.name, rs.iter().map(|r| if okr(r) { f(r.wall) } else { badmark(r) }).collect::<Vec<_>>().join(",")));
    }
    let rw = if rows.is_empty() { "-".to_string() } else if rows.iter().all(|x| x.is_some() && *x == rows[0]) { if dev_rows.is_none() || rows[0] == dev_rows { "ok".into() } else { format!("NO{}", rows[0].unwrap_or(0)) } } else { "NO".into() };
    setl(3, &format!("{} {} rw={}", if mode == "n" { "q3" } else { "q3dev" }, parts.join(" "), rw));
    setl(6, &format!("q3all {}", all.join(" ")));
    // stages of the best run of m
    if let Some(b) = recs.iter().filter(|r| r.v == "m" && r.mode == mode && okr(r)).min_by(|a, b| a.wall.partial_cmp(&b.wall).unwrap_or(std::cmp::Ordering::Equal)) {
        setl(7, &format!("stg.m {} rss={}M rows={}", b.stages.iter().map(|x| format!("{}={}", abbrev(&x.0), f(x.1))).collect::<Vec<_>>().join(" "), f(b.rss_kb / 1024.0), b.rows.map(|x| x.to_string()).unwrap_or("?".into())));
    }
    let _ = anybad;
}
fn vmstat(keys: &[&str]) -> Vec<i64> {
    let s = rd("/proc/vmstat").unwrap_or_default();
    keys.iter().map(|k| s.lines().find_map(|l| { let (a, b) = l.split_once(' ')?; if a == *k { b.trim().parse().ok() } else { None } }).unwrap_or(-1)).collect()
}
fn cgstat(file: &str, keys: &[&str]) -> Vec<i64> {
    let s = rd(&format!("/sys/fs/cgroup/{file}")).unwrap_or_default();
    keys.iter().map(|k| s.lines().find_map(|l| { let (a, b) = l.split_once(' ')?; if a == *k { b.trim().parse().ok() } else { None } }).unwrap_or(-1)).collect()
}
fn rusage_ch() -> [i64; 18] { let mut r = [0i64; 18]; unsafe { getrusage(-1, &mut r) }; r }
const VMK: [&str; 11] = ["thp_fault_alloc", "thp_fault_fallback", "compact_stall", "thp_file_alloc", "thp_file_mapped", "pgfault", "pgmajfault", "thp_split_pmd", "compact_success", "thp_fault_fallback_charge", "thp_collapse_alloc"];
/// one run of v with counter deltas; returns (vmstat deltas, rusage deltas [minflt, majflt, utime ms, stime ms, nvcsw, nivcsw], cpu.stat deltas [nr_throttled, throttled_usec, nr_periods], rec)
fn q4_run(v: &Var, data: &str, scratch: &str, n: &mut usize) -> (Vec<i64>, [f64; 7], Vec<i64>, Rec) {
    let ck = ["nr_throttled", "throttled_usec", "nr_periods"];
    let v0 = vmstat(&VMK); let c0 = cgstat("cpu.stat", &ck); let r0 = rusage_ch();
    let rec = pipe_run(v, "q4", data, scratch, n);
    let r1 = rusage_ch(); let v1 = vmstat(&VMK); let c1 = cgstat("cpu.stat", &ck);
    let dv: Vec<i64> = v0.iter().zip(&v1).map(|(a, b)| if *a < 0 || *b < 0 { -1 } else { b - a }).collect();
    let dc: Vec<i64> = c0.iter().zip(&c1).map(|(a, b)| if *a < 0 || *b < 0 { -1 } else { b - a }).collect();
    let tms = |s: usize| ((r1[s] - r0[s]) as f64) * 1e3 + ((r1[s + 1] - r0[s + 1]) as f64) / 1e3;
    let ru = [(r1[8] - r0[8]) as f64, (r1[9] - r0[9]) as f64, tms(0), tms(2), (r1[16] - r0[16]) as f64, (r1[17] - r0[17]) as f64, (r1[11] - r0[11]) as f64];
    kv!(format!("Q4.{}", v.name), "vm {:?} = {:?} ru {:?} cpu.stat {:?}", VMK, dv, ru, dc);
    (dv, ru, dc, rec)
}
fn sec_pipe(data: &str, scratch: &str, d20: Option<String>, bindir: &str, lim: f64) {
    let vs = vars(bindir);
    kv!("P.variants", "{}", vs.iter().map(|x| x.name.clone()).collect::<Vec<_>>().join(","));
    if vs.is_empty() { setl(3, "q3 NO VARIANTS"); return; }
    let mut n = 0usize; let mut recs: Vec<Rec> = vec![];
    // each variant once on /data (the dev set): exactness by row count
    step(6);
    for v in &vs { if el_s() > lim { break; } recs.push(pipe_run(v, "dev", data, scratch, &mut n)); flush(); }
    let dev: Vec<&Rec> = recs.iter().filter(|r| r.mode == "dev").collect();
    let dev_rows = dev.first().and_then(|r| r.rows);
    let dv_ok = !dev.is_empty() && dev.iter().all(|r| okr(r) && r.rows.is_some() && r.rows == dev_rows);
    setl(8, &format!("dev {} rows={}{}", dev.iter().map(|r| format!("{}={}{}", r.v, f(r.wall), if okr(r) { String::new() } else { badmark(r) })).collect::<Vec<_>>().join(" "), dev_rows.map(|x| x.to_string()).unwrap_or("?".into()), if dv_ok { "" } else { "!" }));
    // Q3: 3 round-robin rounds
    step(7);
    let (qd, mode, refrows) = match &d20 { Some(d) => (d.clone(), "n", None), None => (data.to_string(), "dv", dev_rows) };
    let mut cut_ = false;
    'o: for _rep in 0..3 {
        for v in &vs {
            if el_s() > lim { cut_ = true; break 'o; }
            recs.push(pipe_run(v, mode, &qd, scratch, &mut n));
            q3_lines(&vs, &recs, mode, refrows);
            flush();
        }
    }
    if cut_ { put(9, "q3 cut by budget"); }
    // Q4: one more run of m (and of al), with counters
    step(8);
    for (i, v) in vs.iter().filter(|v| v.name == "m" || v.name == "al").enumerate() {
        if el_s() > lim + 15.0 { put(9, "q4 cut by budget"); break; }
        if i == 1 { step(9); }
        let (dv, ru, dc, rec) = q4_run(v, &qd, scratch, &mut n);
        let w = if okr(&rec) { format!("{}/{}", f(rec.wall), f(stage(&rec, "parse"))) } else { badmark(&rec) };
        if v.name == "m" {
            setl(4, &format!("q4 fa={} ff={} cs={} mf={} u={} s={} thr={} w={}", g3(dv[0] as f64), g3(dv[1] as f64), g3(dv[2] as f64), g3(ru[0]), g3(ru[2]), g3(ru[3]), g3(dc[0] as f64), w));
            put(5, &format!("q4x tfa={} tfm={} pf={} mj={} spl={} csok={} ffc={} vcs={} ivcs={} inb={} thu={}", g3(dv[3] as f64), g3(dv[4] as f64), g3(dv[5] as f64), g3(ru[1]), g3(dv[7] as f64), g3(dv[8] as f64), g3(dv[9] as f64), g3(ru[4]), g3(ru[5]), g3(ru[6]), g3(dc[1] as f64)));
        } else {
            put(5, &format!("q4al fa={} ff={} cs={} mf={} u={} s={} thr={} w={}", g3(dv[0] as f64), g3(dv[1] as f64), g3(dv[2] as f64), g3(ru[0]), g3(ru[2]), g3(ru[3]), g3(dc[0] as f64), w));
        }
        recs.push(rec);
        flush();
    }
}

// ------------------------------------------------------------------ main
fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(|s| s.as_str()) == Some("--nop") { unsafe { _exit(0) } }
    START.store(now(), Relaxed);
    for s in [4, 6, 7, 8, 11] { unsafe { signal(s, on_sig as extern "C" fn(i32) as usize) }; }
    let env = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.into());
    let data = env("DATA_DIR", "/data");
    let out = env("OUTPUT_DIR", "/output");
    let scratch = env("SCRATCH_DIR", "/scratch");
    let bindir = env("PROBE_BIN_DIR", "/p");
    let genbin = env("PROBE_GEN", "/generate");
    let wd: f64 = env("PROBE_WATCHDOG_S", "150").parse().unwrap_or(150.0);
    let lim: f64 = env("PROBE_BUDGET_S", "105").parse().unwrap_or(105.0);
    let clkms: f64 = env("PROBE_CLK_MS", "30").parse().unwrap_or(30.0);
    let only = std::env::var("PROBE_ONLY").ok();
    let run = |s: &str| only.as_ref().map(|o| o.split(',').any(|x| x == s)).unwrap_or(true);
    if let Ok(mut g) = OUTDIR.lock() { *g = out.clone(); }
    snapshot("start");
    let _ = grab(0); let r0 = write_snap(); WGUARD.store(false, Release);
    kv!("T.early_tables", "{:?}", r0);
    watchdog(wd);
    step(1); if run("q2") { q2(); }
    let t = topo();
    let t = if t.cpus8.is_empty() { Topo { allowed: vec![0], cores: vec![vec![0]], cpus8: vec![0], cpus4: vec![0], source: "none" } } else { t };
    step(2); if run("q1") { q1(&t, clkms); }
    step(3); if run("sys") { sec_b(&data); sec_a(&t); }
    // generate d20
    step(4);
    let mut d20: Option<String> = std::env::var("PROBE_D20").ok();
    if d20.is_none() && run("gen") {
        let dir = format!("{scratch}/d20");
        let free = statfree(&scratch);
        let ev = env("PROBE_GEN_EV", "20000000"); let ck = env("PROBE_GEN_CK", "5000000");
        if free >= 0.0 && free < 2.5 { put(9, &format!("gen skipped free={}GB", f(free))); }
        else {
            let r = run_child(&genbin, &["--seed", "20", "--events", &ev, "--clicks", &ck, "--output", &dir, "--threads", &t.allowed.len().max(1).to_string()], &[], 80.0);
            let files = csv_files(&dir);
            let mb: u64 = files.iter().map(|x| x.1).sum();
            kv!("G20", "rc {} wall {:.1} ms files {} bytes {} killed {} stderr {}", r.rc, r.wall, files.len(), mb, r.killed, r.err.chars().take(300).collect::<String>());
            put(9, &format!("gen={}s rc={} files={} MB={} free={}GB {}", f(r.wall / 1e3), r.rc, files.len(), f(mb as f64 / 1e6), f(free), meminfo(&["Dirty", "Writeback"])));
            if r.rc == 0 && mb > 0 {
                step(5);
                let (sm, _se) = sync_all(&dir);
                let res = resident_pct(&files);
                put(9, &format!("sync={}ms res={}%", f(sm), f(res)));
                d20 = Some(dir);
            } else { put(9, &format!("GEN FAILED rc={} K={}", r.rc, r.killed)); let _ = std::fs::remove_dir_all(&dir); }
        }
    }
    if run("pipe") { sec_pipe(&data, &scratch, d20.clone(), &bindir, lim); }
    if let Some(d) = &d20 { if std::env::var("PROBE_D20").is_err() { let _ = std::fs::remove_dir_all(d); } }
    step(10);
    {
        let cg = |p: &str| rdl(&format!("/sys/fs/cgroup/{p}"));
        put(13, &format!("end el={}s mem.peak={} cpu.stat={}", f(el_s()), cg("memory.peak"), cg("cpu.stat").replace('\n', " ")));
    }
    let text = compose("done").unwrap_or_default();
    snapshot("done");
    let ok = grab(3000);
    let r = write_snap();
    raw(b"===== TEXT START\n"); raw(text.as_bytes()); raw(b"===== TEXT END\n");
    kv!("T.tables", "{:?} chars {} grab {}", r, text.len(), ok);
    unsafe { _exit(0) }
}
