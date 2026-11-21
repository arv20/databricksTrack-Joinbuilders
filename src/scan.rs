//! SIMD structural scanner: walks a buffer in 64-byte blocks, keeping bitmasks
//! of `\n`, `,` and `"`, and hands out field boundaries with tzcnt instead of
//! a byte loop. The buffer must be followed by at least 64 readable bytes
//! (all inputs are padded). The end of the data acts as a newline.

#[inline(always)]
unsafe fn masks(p: *const u8) -> (u64, u64, u64) {
    #[cfg(all(target_arch = "x86_64", target_feature = "avx2"))]
    unsafe {
        use std::arch::x86_64::*;
        let a = _mm256_loadu_si256(p as *const __m256i);
        let b = _mm256_loadu_si256(p.add(32) as *const __m256i);
        let m = |c: i8| -> u64 {
            let v = _mm256_set1_epi8(c);
            let x = _mm256_movemask_epi8(_mm256_cmpeq_epi8(a, v)) as u32 as u64;
            let y = _mm256_movemask_epi8(_mm256_cmpeq_epi8(b, v)) as u32 as u64;
            x | (y << 32)
        };
        (m(b'\n' as i8), m(b',' as i8), m(b'"' as i8))
    }
    #[cfg(all(target_arch = "x86_64", not(target_feature = "avx2")))]
    unsafe {
        use std::arch::x86_64::*;
        let v: [__m128i; 4] = [
            _mm_loadu_si128(p as *const __m128i),
            _mm_loadu_si128(p.add(16) as *const __m128i),
            _mm_loadu_si128(p.add(32) as *const __m128i),
            _mm_loadu_si128(p.add(48) as *const __m128i),
        ];
        let m = |c: i8| -> u64 {
            let s = _mm_set1_epi8(c);
            let mut r = 0u64;
            for (i, x) in v.iter().enumerate() {
                r |= (_mm_movemask_epi8(_mm_cmpeq_epi8(*x, s)) as u32 as u64) << (16 * i);
            }
            r
        };
        (m(b'\n' as i8), m(b',' as i8), m(b'"' as i8))
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        use std::arch::aarch64::*;
        let v0 = vld1q_u8(p);
        let v1 = vld1q_u8(p.add(16));
        let v2 = vld1q_u8(p.add(32));
        let v3 = vld1q_u8(p.add(48));
        const BITS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
        let bits = vld1q_u8(BITS.as_ptr());
        let m = |c: u8| -> u64 {
            let s = vdupq_n_u8(c);
            let t0 = vandq_u8(vceqq_u8(v0, s), bits);
            let t1 = vandq_u8(vceqq_u8(v1, s), bits);
            let t2 = vandq_u8(vceqq_u8(v2, s), bits);
            let t3 = vandq_u8(vceqq_u8(v3, s), bits);
            let s0 = vpaddq_u8(t0, t1);
            let s1 = vpaddq_u8(t2, t3);
            let s0 = vpaddq_u8(s0, s1);
            let s0 = vpaddq_u8(s0, s0);
            vgetq_lane_u64(vreinterpretq_u64_u8(s0), 0)
        };
        (m(b'\n'), m(b','), m(b'"'))
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    unsafe {
        let mut r = (0u64, 0u64, 0u64);
        for i in 0..64 {
            let b = *p.add(i);
            r.0 |= ((b == b'\n') as u64) << i;
            r.1 |= ((b == b',') as u64) << i;
            r.2 |= ((b == b'"') as u64) << i;
        }
        r
    }
}

pub struct Scanner<'a> {
    pub data: &'a [u8],
    base: usize,
    nl: u64,
    cm: u64,
    qt: u64,
}

impl<'a> Scanner<'a> {
    /// A scanner positioned on the block starting at `pos` (`pos <= data.len()`).
    #[inline(always)]
    pub fn new(data: &'a [u8], pos: usize) -> Self {
        let mut s = Scanner { data, base: pos, nl: 0, cm: 0, qt: 0 };
        s.load();
        s
    }

    #[inline(always)]
    fn load(&mut self) {
        let (nl, cm, qt) = unsafe { masks(self.data.as_ptr().add(self.base)) };
        let left = self.data.len() - self.base;
        if left < 64 {
            let keep = (1u64 << left) - 1;
            self.nl = (nl & keep) | (1u64 << left);
            self.cm = cm & keep;
            self.qt = qt & keep;
        } else {
            self.nl = nl;
            self.cm = cm;
            self.qt = qt;
        }
    }

    /// The next `,` or `\n` at or after `pos` (the current block must not
    /// lie after `pos`), and the number of quotes in between.
    #[inline(always)]
    pub fn next_sep(&mut self, pos: usize) -> (usize, u32) {
        let mut quotes = 0u32;
        let mut rel = pos - self.base;
        loop {
            if rel < 64 {
                let sep = (self.cm | self.nl) >> rel;
                let q = self.qt >> rel;
                if sep != 0 {
                    let k = sep.trailing_zeros();
                    quotes += (q & ((1u64 << k) - 1)).count_ones();
                    return (pos_of(self.base, rel, k), quotes);
                }
                quotes += q.count_ones();
            } else if rel != 64 {
                // pos lies beyond the next block: jump to it
                self.base = pos;
                self.load();
                rel = 0;
                continue;
            }
            self.base += 64;
            self.load();
            rel = 0;
        }
    }

    /// Whether the separator found at `q` (in the current block) is a newline.
    #[inline(always)]
    pub fn is_nl(&self, q: usize) -> bool {
        (self.nl >> (q - self.base)) & 1 != 0
    }

    /// The next `\n` (or end) at or after `pos`.
    #[inline(always)]
    pub fn next_nl(&mut self, pos: usize) -> usize {
        let mut rel = pos - self.base;
        loop {
            if rel < 64 {
                let m = self.nl >> rel;
                if m != 0 {
                    return pos_of(self.base, rel, m.trailing_zeros());
                }
            } else if rel != 64 {
                self.base = pos;
                self.load();
                rel = 0;
                continue;
            }
            self.base += 64;
            self.load();
            rel = 0;
        }
    }

    /// Splits the line starting at `start` into exactly `N` fields. Returns
    /// whether the line is well formed, and the start of the next line.
    #[inline(always)]
    pub fn line<const N: usize>(&mut self, start: usize, out: &mut [&'a [u8]; N]) -> (bool, usize) {
        let data = self.data;
        let mut p = start;
        let mut ok = true;
        let mut k = 0;
        loop {
            let (q, quotes) = self.next_sep(p);
            // branchless: a field is either quote-free, or "..." with exactly two quotes
            let first = unsafe { *data.as_ptr().add(p) };
            let last = unsafe { *data.as_ptr().add(q.wrapping_sub(1).max(p)) };
            let quoted = (quotes == 2) & (first == b'"') & (last == b'"') & (q - p >= 2);
            ok &= (quotes == 0) | quoted;
            let off = quoted as usize;
            out[k] = unsafe { data.get_unchecked(p + off..q - off.min(q - p)) };
            if self.is_nl(q) {
                return (ok && k == N - 1, q + 1);
            }
            if k == N - 1 {
                let e = self.next_nl(q + 1);
                return (false, e + 1);
            }
            k += 1;
            p = q + 1;
        }
    }
}

#[inline(always)]
fn pos_of(base: usize, rel: usize, k: u32) -> usize {
    base + rel + k as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    fn lines<const N: usize>(text: &[u8]) -> Vec<Option<Vec<Vec<u8>>>> {
        let mut buf = text.to_vec();
        let len = buf.len();
        buf.resize(len + 128, 0);
        let data = &buf[..len];
        let mut sc = Scanner::new(data, 0);
        let mut p = 0;
        let mut out = Vec::new();
        while p < len {
            let mut f = [&b""[..]; N];
            let (ok, next) = sc.line::<N>(p, &mut f);
            out.push(if ok { Some(f.iter().map(|x| x.to_vec()).collect()) } else { None });
            p = next;
        }
        out
    }
    #[test]
    fn matches_simple_split() {
        let mut text = Vec::new();
        let samples: [&[u8]; 12] = [
            b"a,b", b"\"a\",\"\"", b",", b"a,b,c", b"a", b"", b"a,b\"", b"\"a,b\"", b"\"\",\"x\"",
            b"x\"y\",z", b"\"\"\",z", b"long-field-value-that-goes-on-and-on-and-on-past-64-bytes-xxxxxxxxxxxxxxxx,y",
        ];
        for s in samples.iter().cycle().take(500) {
            text.extend_from_slice(s);
            text.push(b'\n');
        }
        text.extend_from_slice(b"tail,end");
        let got = lines::<2>(&text);
        let mut want = Vec::new();
        for line in text.split(|&b| b == b'\n') {
            let mut f = [&b""[..]; 2];
            want.push(if crate::csv::split(line, &mut f) { Some(f.iter().map(|x| x.to_vec()).collect()) } else { None });
        }
        assert_eq!(got, want);
    }
}
