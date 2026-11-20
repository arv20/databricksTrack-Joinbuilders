//! IDs as `u128` keys without heap strings.
//!
//! A value of 1..=15 bytes is packed big-endian, left-aligned, zero padded, with
//! its length in the lowest byte. Integer order on two such keys is exactly
//! byte-wise order of the strings (NUL bytes included: the length byte breaks
//! ties), and integer equality is string equality.
//!
//! Longer values are interned in a global table and get the key
//! `(index << 8) | 0xFF`; equality stays exact (one index per distinct string),
//! and `key_cmp` falls back to comparing the full bytes whenever a long key is
//! involved. The generator never writes IDs this long, so that path is cold.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::sync::Mutex;

pub type Key = u128;
pub const LONG_TAG: u8 = 0xFF;

struct Longs {
    map: HashMap<Vec<u8>, u32>,
    list: Vec<Vec<u8>>,
}

static LONGS: Mutex<Option<Longs>> = Mutex::new(None);

#[cold]
#[inline(never)]
fn intern_long(s: &[u8]) -> Key {
    let mut guard = LONGS.lock().unwrap();
    let longs = guard.get_or_insert_with(|| Longs { map: HashMap::new(), list: Vec::new() });
    let idx = match longs.map.get(s) {
        Some(&i) => i,
        None => {
            let i = longs.list.len() as u32;
            longs.list.push(s.to_vec());
            longs.map.insert(s.to_vec(), i);
            i
        }
    };
    ((idx as u128) << 8) | LONG_TAG as u128
}

/// Key of a non-empty value. SAFETY contract: at least 16 bytes must be
/// readable from `s.as_ptr()` (every input buffer carries such padding).
#[inline(always)]
pub fn key(s: &[u8]) -> Key {
    let n = s.len();
    debug_assert!(n > 0);
    if n <= 15 {
        let raw = unsafe { std::ptr::read_unaligned(s.as_ptr() as *const [u8; 16]) };
        let w = u128::from_be_bytes(raw);
        let mask = !0u128 << (8 * (16 - n));
        (w & mask) | n as u128
    } else {
        intern_long(s)
    }
}

/// Key of a value from an arbitrary (unpadded) slice.
pub fn key_copy(s: &[u8]) -> Key {
    let mut buf = [0u8; 32];
    if s.len() <= 15 {
        buf[..s.len()].copy_from_slice(s);
        key(&buf[..s.len()])
    } else {
        intern_long(s)
    }
}

#[inline(always)]
pub fn is_long(k: Key) -> bool {
    k as u8 == LONG_TAG
}

/// Appends the bytes of `k` to `out`.
pub fn write_bytes(k: Key, out: &mut Vec<u8>) {
    if is_long(k) {
        let guard = LONGS.lock().unwrap();
        out.extend_from_slice(&guard.as_ref().unwrap().list[(k >> 8) as usize]);
    } else {
        let n = (k as u8) as usize;
        let b = k.to_be_bytes();
        out.extend_from_slice(&b[..n]);
    }
}

pub fn bytes(k: Key) -> Vec<u8> {
    let mut v = Vec::new();
    write_bytes(k, &mut v);
    v
}

#[inline(always)]
pub fn key_cmp(a: Key, b: Key) -> Ordering {
    if is_long(a) | is_long(b) { slow_cmp(a, b) } else { a.cmp(&b) }
}

#[cold]
fn slow_cmp(a: Key, b: Key) -> Ordering {
    if a == b {
        return Ordering::Equal;
    }
    bytes(a).cmp(&bytes(b))
}

#[inline(always)]
pub fn hash(k: Key) -> u64 {
    let lo = k as u64;
    let hi = (k >> 64) as u64;
    let m = ((lo ^ 0x243F_6A88_85A3_08D3) as u128) * ((hi ^ 0x1319_8A2E_0370_7344) as u128);
    let x = (m as u64) ^ ((m >> 64) as u64);
    let m2 = ((x ^ 0xA409_3822_299F_31D0) as u128) * (0x9E37_79B9_7F4A_7C15u128);
    (m2 as u64) ^ ((m2 >> 64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn order_matches_bytes() {
        let vals: Vec<&[u8]> = vec![
            b"a", b"a\0", b"a\0\0", b"ab", b"b", b"\0", b"\0\0", b"A", b"u1", b"u10", b"u1 ", b" u1",
            b"abcdefghijklmno", b"abcdefghijklmnop", b"abcdefghijklmnoq", b"abcdefghijklmn",
            b"abcdefghijklmno\0", b"\xff", b"\xff\xff",
        ];
        for a in &vals {
            for b in &vals {
                let ka = key_copy(a);
                let kb = key_copy(b);
                assert_eq!(key_cmp(ka, kb), a.cmp(b), "{a:?} {b:?}");
                assert_eq!(ka == kb, a == b);
                assert_eq!(&bytes(ka)[..], *a);
            }
        }
    }
}
