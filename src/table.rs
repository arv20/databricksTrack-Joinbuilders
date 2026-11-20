//! Open-addressing hash map from `Key` (never 0) to a small Copy value.

use crate::ids::{Key, hash};

#[derive(Clone)]
pub struct KeyMap<V: Copy + Default> {
    keys: Vec<Key>,
    vals: Vec<V>,
    mask: usize,
    len: usize,
}

impl<V: Copy + Default> KeyMap<V> {
    pub fn with_capacity(n: usize) -> Self {
        let cap = (n.max(4) * 2).next_power_of_two();
        KeyMap { keys: vec![0; cap], vals: vec![V::default(); cap], mask: cap - 1, len: 0 }
    }

    #[inline(always)]
    pub fn len(&self) -> usize {
        self.len
    }

    #[inline(always)]
    pub fn slot_of(&self, k: Key) -> usize {
        (hash(k) as usize) & self.mask
    }

    #[inline(always)]
    pub fn prefetch(&self, k: Key) {
        let i = self.slot_of(k);
        prefetch_read(unsafe { self.keys.as_ptr().add(i) } as *const u8);
    }

    #[inline(always)]
    pub fn get(&self, k: Key) -> Option<V> {
        let mut i = self.slot_of(k);
        loop {
            let s = unsafe { *self.keys.get_unchecked(i) };
            if s == k {
                return Some(unsafe { *self.vals.get_unchecked(i) });
            }
            if s == 0 {
                return None;
            }
            i = (i + 1) & self.mask;
        }
    }

    /// Returns the value for `k`, inserting `make()` first if absent.
    #[inline(always)]
    pub fn get_or_insert(&mut self, k: Key, make: impl FnOnce() -> V) -> V {
        if (self.len + 1) * 4 > self.keys.len() * 3 {
            self.grow();
        }
        let mut i = self.slot_of(k);
        loop {
            let s = unsafe { *self.keys.get_unchecked(i) };
            if s == k {
                return unsafe { *self.vals.get_unchecked(i) };
            }
            if s == 0 {
                let v = make();
                self.keys[i] = k;
                self.vals[i] = v;
                self.len += 1;
                return v;
            }
            i = (i + 1) & self.mask;
        }
    }

    pub fn insert(&mut self, k: Key, v: V) {
        if (self.len + 1) * 4 > self.keys.len() * 3 {
            self.grow();
        }
        let mut i = self.slot_of(k);
        loop {
            let s = self.keys[i];
            if s == k {
                self.vals[i] = v;
                return;
            }
            if s == 0 {
                self.keys[i] = k;
                self.vals[i] = v;
                self.len += 1;
                return;
            }
            i = (i + 1) & self.mask;
        }
    }

    fn grow(&mut self) {
        let cap = self.keys.len() * 2;
        let old_k = std::mem::replace(&mut self.keys, vec![0; cap]);
        let old_v = std::mem::replace(&mut self.vals, vec![V::default(); cap]);
        self.mask = cap - 1;
        self.len = 0;
        for (k, v) in old_k.into_iter().zip(old_v) {
            if k != 0 {
                self.insert(k, v);
            }
        }
    }

    /// Empties the map, keeping the allocation if it is not too big for `n`.
    pub fn clear_for(&mut self, n: usize) {
        let cap = (n.max(4) * 2).next_power_of_two();
        if cap > self.keys.len() || self.keys.len() > cap * 8 {
            *self = Self::with_capacity(n);
        } else {
            self.keys.fill(0);
            self.len = 0;
        }
    }

    /// Rewrites every value.
    pub fn map_values(&mut self, mut f: impl FnMut(Key, V) -> V) {
        for i in 0..self.keys.len() {
            if self.keys[i] != 0 {
                self.vals[i] = f(self.keys[i], self.vals[i]);
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
