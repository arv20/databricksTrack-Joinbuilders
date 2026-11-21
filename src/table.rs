//! Open-addressing (linear probing) hash map from `Key` (never 0) to a small
//! Copy value, key and value side by side so a hit costs one cache line.

use crate::ids::{Key, hash};

#[derive(Clone, Copy, Default)]
#[repr(C)]
struct Slot<V: Copy + Default> {
    k: Key,
    v: V,
}

#[derive(Clone)]
pub struct KeyMap<V: Copy + Default> {
    slots: Vec<Slot<V>>,
    mask: usize,
    len: usize,
}

impl<V: Copy + Default> KeyMap<V> {
    pub fn with_capacity(n: usize) -> Self {
        let cap = (n.max(4) * 2).next_power_of_two();
        KeyMap { slots: vec![Slot::default(); cap], mask: cap - 1, len: 0 }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    fn slot_of(&self, k: Key) -> usize {
        (hash(k) as usize) & self.mask
    }

    #[inline(always)]
    pub fn prefetch(&self, k: Key) {
        let i = self.slot_of(k);
        prefetch_read(unsafe { self.slots.as_ptr().add(i) } as *const u8);
    }

    #[inline(always)]
    pub fn get(&self, k: Key) -> Option<V> {
        let mut i = self.slot_of(k);
        loop {
            let s = unsafe { self.slots.get_unchecked(i) };
            if s.k == k {
                return Some(s.v);
            }
            if s.k == 0 {
                return None;
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Returns the value for `k`, inserting `make()` first if absent.
    #[inline(always)]
    pub fn get_or_insert(&mut self, k: Key, make: impl FnOnce() -> V) -> V {
        if (self.len + 1) * 4 > self.slots.len() * 3 {
            self.grow();
        }
        let mut i = self.slot_of(k);
        loop {
            let s = unsafe { self.slots.get_unchecked_mut(i) };
            if s.k == k {
                return s.v;
            }
            if s.k == 0 {
                let v = make();
                s.k = k;
                s.v = v;
                self.len += 1;
                return v;
            }
            i = (i + 1) & self.mask;
        }
    }

    pub fn insert(&mut self, k: Key, v: V) {
        if (self.len + 1) * 4 > self.slots.len() * 3 {
            self.grow();
        }
        let mut i = self.slot_of(k);
        loop {
            let s = &mut self.slots[i];
            if s.k == k {
                s.v = v;
                return;
            }
            if s.k == 0 {
                s.k = k;
                s.v = v;
                self.len += 1;
                return;
            }
            i = (i + 1) & self.mask;
        }
    }

    fn grow(&mut self) {
        let cap = self.slots.len() * 2;
        let old = std::mem::replace(&mut self.slots, vec![Slot::default(); cap]);
        self.mask = cap - 1;
        self.len = 0;
        for s in old {
            if s.k != 0 {
                self.insert(s.k, s.v);
            }
        }
    }

    /// Empties the map, keeping the allocation if it is not too big for `n`.
    pub fn clear_for(&mut self, n: usize) {
        let cap = (n.max(4) * 2).next_power_of_two();
        if cap > self.slots.len() || self.slots.len() > cap * 8 {
            *self = Self::with_capacity(n);
        } else {
            for s in self.slots.iter_mut() {
                s.k = 0;
            }
            self.len = 0;
        }
    }

    /// Rewrites every value.
    pub fn map_values(&mut self, mut f: impl FnMut(Key, V) -> V) {
        for s in self.slots.iter_mut() {
            if s.k != 0 {
                s.v = f(s.k, s.v);
            }
        }
    }
}

#[inline(always)]
pub fn prefetch_read(p: *const u8) {
    #[cfg(target_arch = "x86_64")]
    unsafe {
        std::arch::x86_64::_mm_prefetch(p as *const i8, std::arch::x86_64::_MM_HINT_T0);
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        std::arch::asm!("prfm pldl1keep, [{0}]", in(reg) p, options(nostack, readonly, preserves_flags));
    }
}
