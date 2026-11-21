//! Global allocator: large blocks come straight from mmap with transparent
//! huge pages requested (fewer page faults and a cheaper exit), and grow with
//! mremap (no copying). Small blocks use the system allocator. Linux only;
//! elsewhere the system allocator is used as is.

/// Hands the allocator memory that is no longer read (already faulted in):
/// later big non-zeroed allocations are carved from it before any fresh
/// mapping, so they need no new pages faulted and zeroed by the kernel.
/// SAFETY: nothing may access `[start, end)` afterwards.
pub unsafe fn donate(start: usize, end: usize) {
    #[cfg(target_os = "linux")]
    imp::donate(start, end);
    #[cfg(not(target_os = "linux"))]
    let _ = (start, end);
}

#[cfg(target_os = "linux")]
mod imp {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    /// donated ranges (2 MB aligned), guarded by a spin lock: big
    /// allocations are rare (hundreds per run)
    const NDON: usize = 64;
    static DON_LOCK: AtomicBool = AtomicBool::new(false);
    static DON_S: [AtomicUsize; NDON] = [const { AtomicUsize::new(0) }; NDON];
    static DON_E: [AtomicUsize; NDON] = [const { AtomicUsize::new(0) }; NDON];

    fn lock() {
        while DON_LOCK.compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed).is_err() {
            std::hint::spin_loop();
        }
    }
    fn unlock() {
        DON_LOCK.store(false, Ordering::Release);
    }

    pub fn donate(start: usize, end: usize) {
        let (s, e) = ((start + HUGE - 1) & !(HUGE - 1), end & !(HUGE - 1));
        if e <= s {
            return;
        }
        lock();
        for i in 0..NDON {
            if DON_S[i].load(Ordering::Relaxed) == DON_E[i].load(Ordering::Relaxed) {
                DON_S[i].store(s, Ordering::Relaxed);
                DON_E[i].store(e, Ordering::Relaxed);
                break;
            }
        }
        unlock();
    }

    /// `len` bytes (a multiple of 2 MB) from the donated memory, if any.
    fn take(len: usize) -> Option<usize> {
        lock();
        let mut r = None;
        for i in 0..NDON {
            let s = DON_S[i].load(Ordering::Relaxed);
            if DON_E[i].load(Ordering::Relaxed) - s >= len {
                DON_S[i].store(s + len, Ordering::Relaxed);
                r = Some(s);
                break;
            }
        }
        unlock();
        r
    }

    /// A big block for `alloc`: donated memory first, else a fresh mapping.
    unsafe fn get_big(size: usize) -> *mut u8 {
        match take(round(size)) {
            Some(p) => p as *mut u8,
            None => unsafe { map_big(size) },
        }
    }

    const BIG: usize = 1 << 20;
    const HUGE: usize = 1 << 21;

    pub struct Alloc;

    #[inline]
    fn round(n: usize) -> usize {
        (n + HUGE - 1) & !(HUGE - 1)
    }

    unsafe fn map_big(size: usize) -> *mut u8 {
        unsafe {
            let len = round(size);
            let p = libc::mmap(
                std::ptr::null_mut(),
                len + HUGE,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_NORESERVE,
                -1,
                0,
            );
            if p == libc::MAP_FAILED {
                return std::ptr::null_mut();
            }
            let a = p as usize;
            let aligned = (a + HUGE - 1) & !(HUGE - 1);
            if aligned > a {
                libc::munmap(p, aligned - a);
            }
            let tail = aligned + len;
            let end = a + len + HUGE;
            if end > tail {
                libc::munmap(tail as *mut libc::c_void, end - tail);
            }
            libc::madvise(aligned as *mut libc::c_void, len, libc::MADV_HUGEPAGE);
            aligned as *mut u8
        }
    }

    unsafe impl GlobalAlloc for Alloc {
        #[inline]
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            unsafe { if l.size() >= BIG && l.align() <= 4096 { get_big(l.size()) } else { System.alloc(l) } }
        }
        #[inline]
        unsafe fn alloc_zeroed(&self, l: Layout) -> *mut u8 {
            unsafe {
                if l.size() >= BIG && l.align() <= 4096 { map_big(l.size()) } else { System.alloc_zeroed(l) }
            }
        }
        #[inline]
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            unsafe {
                if l.size() >= BIG && l.align() <= 4096 {
                    libc::munmap(p as *mut libc::c_void, round(l.size()));
                } else {
                    System.dealloc(p, l)
                }
            }
        }
        #[inline]
        unsafe fn realloc(&self, p: *mut u8, l: Layout, new: usize) -> *mut u8 {
            unsafe {
                let old_big = l.size() >= BIG && l.align() <= 4096;
                let new_big = new >= BIG && l.align() <= 4096;
                if old_big && new_big {
                    let (o, n) = (round(l.size()), round(new));
                    if o == n {
                        return p;
                    }
                    if n < o {
                        libc::munmap(p.add(n) as *mut libc::c_void, o - n);
                        return p;
                    }
                    // move the pages (no copy) onto a fresh 2 MB-aligned reservation,
                    // so the grown buffer can still get huge pages
                    let dst = map_big(n);
                    if dst.is_null() {
                        return std::ptr::null_mut();
                    }
                    let q = libc::mremap(
                        p as *mut libc::c_void,
                        o,
                        n,
                        libc::MREMAP_MAYMOVE | libc::MREMAP_FIXED,
                        dst as *mut libc::c_void,
                    );
                    if q == libc::MAP_FAILED {
                        return std::ptr::null_mut();
                    }
                    libc::madvise(q, n, libc::MADV_HUGEPAGE);
                    return q as *mut u8;
                }
                if !old_big && !new_big {
                    return System.realloc(p, l, new);
                }
                let nl = Layout::from_size_align_unchecked(new, l.align());
                let q = self.alloc(nl);
                if !q.is_null() {
                    std::ptr::copy_nonoverlapping(p, q, l.size().min(new));
                    self.dealloc(p, l);
                }
                q
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[global_allocator]
static GLOBAL: imp::Alloc = imp::Alloc;
