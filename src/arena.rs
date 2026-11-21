//! Radix-scatter buffers: each bucket is a list of fixed-size blocks carved
//! from a per-thread bump arena of huge-page-backed chunks. Pushing is a
//! store plus a counter bump; nothing is reallocated or copied.

use std::sync::atomic::{AtomicUsize, Ordering};

pub const BLOCK: usize = 128;
const CHUNK: usize = 64 << 20;

pub struct Arena {
    cur: usize,
    end: usize,
    /// recycled memory (start, end) that is no longer read, used before new slabs
    free: Vec<(usize, usize)>,
}

unsafe impl Send for Arena {}

pub fn map_chunk(bytes: usize) -> *mut u8 {
    const HUGE: usize = 1 << 21;
    let len = bytes.div_ceil(HUGE) * HUGE;
    unsafe {
        let p = libc::mmap(
            std::ptr::null_mut(),
            len + HUGE,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_PRIVATE | libc::MAP_ANON | libc::MAP_NORESERVE,
            -1,
            0,
        );
        assert!(p != libc::MAP_FAILED, "mmap failed");
        let a = p as usize;
        let aligned = (a + HUGE - 1) & !(HUGE - 1);
        #[cfg(target_os = "linux")]
        libc::madvise(aligned as *mut libc::c_void, len, libc::MADV_HUGEPAGE);
        aligned as *mut u8
    }
}

/// One big virtual reservation shared by all arenas, cut into slabs with an
/// atomic bump, so the hot phases make no mmap calls (which would contend on
/// the kernel's mmap lock with page faults).
static RES_BASE: AtomicUsize = AtomicUsize::new(0);
static RES_END: AtomicUsize = AtomicUsize::new(0);
static RES_NEXT: AtomicUsize = AtomicUsize::new(0);
const SLAB: usize = 2 << 20;

/// Reserves `bytes` of address space (no memory is committed until touched).
pub fn reserve(bytes: usize) {
    let p = map_chunk(bytes) as usize;
    RES_BASE.store(p, Ordering::Relaxed);
    RES_NEXT.store(p, Ordering::Relaxed);
    RES_END.store(p + bytes, Ordering::Relaxed);
}

/// The part of the reservation handed out so far, `(start, end)`.
pub fn used() -> (usize, usize) {
    let base = RES_BASE.load(Ordering::Relaxed);
    (base, RES_NEXT.load(Ordering::Relaxed).min(RES_END.load(Ordering::Relaxed)).max(base))
}

fn slab(bytes: usize) -> (usize, usize) {
    let size = bytes.div_ceil(SLAB) * SLAB;
    if RES_BASE.load(Ordering::Relaxed) != 0 {
        let p = RES_NEXT.fetch_add(size, Ordering::Relaxed);
        if p + size <= RES_END.load(Ordering::Relaxed) {
            return (p, p + size);
        }
    }
    let size = CHUNK.max(size);
    let p = map_chunk(size) as usize;
    (p, p + size)
}

impl Arena {
    pub fn new() -> Self {
        Arena { cur: 0, end: 0, free: Vec::new() }
    }
    #[inline]
    pub fn alloc(&mut self, bytes: usize, align: usize) -> *mut u8 {
        let mut p = (self.cur + align - 1) & !(align - 1);
        if p + bytes > self.end {
            while let Some((s, e)) = self.free.pop() {
                let q = (s + align - 1) & !(align - 1);
                if q + bytes <= e {
                    self.cur = q + bytes;
                    self.end = e;
                    return q as *mut u8;
                }
            }
            let (s, e) = slab(bytes + align);
            self.cur = s;
            self.end = e;
            p = (self.cur + align - 1) & !(align - 1);
        }
        self.cur = p + bytes;
        p as *mut u8
    }
}

pub struct Scatter<T: Copy> {
    arena: Arena,
    /// per bucket: the block being filled and its fill
    cur: Vec<(*mut T, u32)>,
    /// per bucket: every block, the one being filled last
    lists: Vec<Vec<*mut T>>,
}

unsafe impl<T: Copy + Send> Send for Scatter<T> {}
unsafe impl<T: Copy + Sync> Sync for Scatter<T> {}

impl<T: Copy> Scatter<T> {
    /// Hands bucket `b`'s blocks to `to` for reuse. SAFETY: nothing may read
    /// this bucket afterwards.
    pub unsafe fn recycle_bucket<U: Copy>(&self, b: usize, to: &mut Scatter<U>) {
        for &p in &self.lists[b] {
            let s = p as usize;
            to.arena.free.push((s, s + BLOCK * std::mem::size_of::<T>()));
        }
    }

    /// Moves every recycled chunk of `self` to `to`.
    pub fn give_free<U: Copy>(&mut self, to: &mut Scatter<U>) {
        to.arena.free.append(&mut self.arena.free);
    }

    pub fn new(nb: usize) -> Self {
        Scatter { arena: Arena::new(), cur: vec![(std::ptr::null_mut(), BLOCK as u32); nb], lists: vec![Vec::new(); nb] }
    }

    #[inline(always)]
    pub fn push(&mut self, b: usize, x: T) {
        let c = unsafe { self.cur.get_unchecked_mut(b) };
        if c.1 as usize == BLOCK {
            self.new_block(b);
        }
        let c = unsafe { self.cur.get_unchecked_mut(b) };
        unsafe { c.0.add(c.1 as usize).write(x) };
        c.1 += 1;
    }

    #[cold]
    #[inline(never)]
    fn new_block(&mut self, b: usize) {
        let p = self.arena.alloc(BLOCK * std::mem::size_of::<T>(), 64) as *mut T;
        self.lists[b].push(p);
        self.cur[b] = (p, 0);
    }

    /// Number of items in bucket `b`.
    pub fn len(&self, b: usize) -> usize {
        let n = self.lists[b].len();
        if n == 0 { 0 } else { (n - 1) * BLOCK + self.cur[b].1 as usize }
    }

    /// The blocks of bucket `b` as slices.
    #[inline]
    pub fn blocks(&self, b: usize) -> impl Iterator<Item = &[T]> + '_ {
        let n = self.lists[b].len();
        let last = self.cur[b].1 as usize;
        self.lists[b].iter().enumerate().map(move |(i, &p)| {
            let len = if i + 1 == n { last } else { BLOCK };
            unsafe { std::slice::from_raw_parts(p as *const T, len) }
        })
    }
}
