#![allow(dead_code)]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

extern "C" {
    pub fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    pub fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
    pub fn _exit(code: i32) -> !;
    fn execve(path: *const c_char, argv: *const *const c_char, envp: *const *const c_char) -> i32;
    static environ: *const *const c_char;
}
use std::ffi::{c_char, CString};
pub const PROT_READ: i32 = 1;
pub const PROT_WRITE: i32 = 2;
pub const MAP_PRIVATE: i32 = 2;
pub const MAP_FIXED: i32 = 0x10;
pub const MAP_ANONYMOUS: i32 = 0x20;
pub const MAP_NORESERVE: i32 = 0x4000;
pub const MADV_DONTNEED: i32 = 4;
pub const MADV_HUGEPAGE: i32 = 14;
pub const MADV_NOHUGEPAGE: i32 = 15;
const HP: usize = 2 << 20;

pub static HUGE: AtomicBool = AtomicBool::new(true);

pub static WORKERS: AtomicBool = AtomicBool::new(false);

struct ExecInfo {
    argv0: CString,
    _args: Vec<CString>,
    ptrs: Vec<*const c_char>,
    data: String,
    out: String,
    ncpu: usize,
}
static mut EXEC: Option<ExecInfo> = None;

pub fn exec_init(args: &[String], data: &str, out: &str, ncpu: usize) {
    let cs = |s: &str| CString::new(s).unwrap_or_else(|_| CString::new("pipeline").unwrap());
    let argv0 = cs(args.first().map_or("pipeline", |s| s.as_str()));
    let mut a = vec![argv0.clone(), cs("--generic")];
    if args.len() >= 3 {
        a.push(cs(data));
        a.push(cs(out));
    }
    let mut ptrs: Vec<*const c_char> = a.iter().map(|c| c.as_ptr()).collect();
    ptrs.push(std::ptr::null());
    unsafe {
        EXEC = Some(ExecInfo { argv0, _args: a, ptrs, data: data.to_string(), out: out.to_string(), ncpu });
    }
}

#[cold]
#[inline(never)]
pub fn fallback(why: &str) -> ! {
    eprintln!("[v2b] FALLBACK: {why}");
    unsafe {
        #[allow(static_mut_refs)]
        if let Some(e) = EXEC.as_ref() {
            execve(c"/proc/self/exe".as_ptr(), e.ptrs.as_ptr(), environ);
            execve(e.argv0.as_ptr(), e.ptrs.as_ptr(), environ);
            eprintln!("[v2b] exec failed");
            if !WORKERS.load(Ordering::SeqCst) && std::thread::current().name() == Some("main") {
                crate::run_generic(&e.data, &e.out, e.ncpu);
            }
        }
        _exit(3)
    }
}

pub fn anon(len: usize) -> *mut u8 {
    let len = (len + HP - 1) & !(HP - 1);
    unsafe {
        let p = mmap(std::ptr::null_mut(), len + HP, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0);
        if p as isize == -1 {
            fallback("mmap failed");
        }
        let a = ((p as usize + HP - 1) & !(HP - 1)) as *mut u8;
        madvise(a, len, if HUGE.load(Ordering::Relaxed) { MADV_HUGEPAGE } else { MADV_NOHUGEPAGE });
        a
    }
}

pub struct Arena {
    base: *mut u8,
    cap: usize,
    next: AtomicUsize,
}
unsafe impl Sync for Arena {}
unsafe impl Send for Arena {}
impl Arena {
    pub fn new(cap: usize) -> Arena {
        Arena { base: anon(cap), cap, next: AtomicUsize::new(0) }
    }
    #[inline]
    pub fn alloc(&self, n: usize) -> *mut u8 {
        let off = self.next.fetch_add(n, Ordering::Relaxed);
        if off + n > self.cap {
            fallback("arena full");
        }
        unsafe { self.base.add(off) }
    }
    pub fn used(&self) -> usize {
        self.next.load(Ordering::Relaxed)
    }
}

pub struct AVec<T> {
    p: *mut T,
    len: usize,
    cap: usize,
}
unsafe impl<T> Send for AVec<T> {}
impl<T: Copy> AVec<T> {
    pub fn new(cap: usize) -> AVec<T> {
        let cap = cap.max(16);
        AVec { p: anon(cap * std::mem::size_of::<T>()) as *mut T, len: 0, cap }
    }
    pub const fn empty() -> AVec<T> {
        AVec { p: std::ptr::NonNull::dangling().as_ptr(), len: 0, cap: 0 }
    }
    #[inline(always)]
    pub fn push(&mut self, v: T) {
        if self.len >= self.cap {
            fallback("avec full");
        }
        unsafe { self.p.add(self.len).write(v) };
        self.len += 1;
    }
    #[inline(always)]
    pub fn extend(&mut self, s: &[T]) {
        if self.len + s.len() > self.cap {
            fallback("avec full");
        }
        unsafe { std::ptr::copy_nonoverlapping(s.as_ptr(), self.p.add(self.len), s.len()) };
        self.len += s.len();
    }
    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }
    #[inline(always)]
    pub fn as_slice(&self) -> &[T] {
        unsafe { std::slice::from_raw_parts(self.p, self.len) }
    }
    #[inline(always)]
    pub fn as_mut_ptr(&mut self) -> *mut T {
        self.p
    }
    #[inline(always)]
    pub fn as_ptr(&self) -> *const T {
        self.p
    }
    pub fn clear(&mut self) {
        self.len = 0;
    }
    pub fn truncate(&mut self, n: usize) {
        self.len = n;
    }
}

pub fn run_par<F: Fn(usize) + Sync>(nt: usize, f: F) {
    WORKERS.store(true, Ordering::SeqCst);
    std::thread::scope(|s| {
        for t in 1..nt {
            let f = &f;
            s.spawn(move || f(t));
        }
        f(0);
    });
}

pub struct U64Map {
    keys: Vec<u64>,
    vals: Vec<u32>,
    mask: usize,
    len: usize,
}
impl U64Map {
    pub fn new() -> U64Map {
        U64Map { keys: vec![0; 1024], vals: vec![0; 1024], mask: 1023, len: 0 }
    }
    #[inline(always)]
    fn h(k: u64, mask: usize) -> usize {
        (k.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 20) as usize & mask
    }
    #[inline]
    pub fn get(&self, k: u64) -> Option<u32> {
        let k1 = k + 1;
        let mut i = Self::h(k, self.mask);
        loop {
            let s = unsafe { *self.keys.get_unchecked(i) };
            if s == k1 {
                return Some(unsafe { *self.vals.get_unchecked(i) });
            }
            if s == 0 {
                return None;
            }
            i = (i + 1) & self.mask;
        }
    }
    pub fn get_or_insert(&mut self, k: u64, v: u32) -> u32 {
        if self.len * 2 >= self.keys.len() {
            self.grow();
        }
        let k1 = k + 1;
        let mut i = Self::h(k, self.mask);
        loop {
            if self.keys[i] == k1 {
                return self.vals[i];
            }
            if self.keys[i] == 0 {
                self.keys[i] = k1;
                self.vals[i] = v;
                self.len += 1;
                return v;
            }
            i = (i + 1) & self.mask;
        }
    }
    fn grow(&mut self) {
        let n = self.keys.len() * 2;
        let ok = std::mem::replace(&mut self.keys, vec![0; n]);
        let ov = std::mem::replace(&mut self.vals, vec![0; n]);
        self.mask = n - 1;
        for (k1, v) in ok.into_iter().zip(ov) {
            if k1 != 0 {
                let mut i = Self::h(k1 - 1, self.mask);
                while self.keys[i] != 0 {
                    i = (i + 1) & self.mask;
                }
                self.keys[i] = k1;
                self.vals[i] = v;
            }
        }
    }
}

pub static mut LAPS: Vec<(&'static str, f64)> = Vec::new();
pub static mut LAST: Option<std::time::Instant> = None;
#[allow(static_mut_refs)]
pub fn lap(name: &'static str) {
    unsafe {
        let now = std::time::Instant::now();
        if let Some(l) = LAST {
            LAPS.push((name, (now - l).as_secs_f64() * 1e3));
        }
        LAST = Some(now);
    }
}
