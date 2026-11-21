//! Timestamps: F1 `YYYY-MM-DDTHH:MM:SSZ`, F2 `YYYY-MM-DDTHH:MM:SS+HH:MM`, F3 digits (epoch ms).

pub const SEC: i64 = 1_000;
pub const MIN: i64 = 60 * SEC;
pub const HOUR: i64 = 60 * MIN;
pub const DAY: i64 = 24 * HOUR;

#[inline(always)]
fn d(b: u8) -> u32 {
    b.wrapping_sub(b'0') as u32
}

#[inline(always)]
pub fn days_from_civil(y: i64, m: i64, dd: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + dd - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[inline(always)]
fn leap(y: u32) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// The civil part `YYYY-MM-DDTHH:MM:SS` (first 19 bytes, caller checks length) in ms.
#[inline(always)]
fn civil(f: &[u8]) -> Option<i64> {
    let ds = [
        d(f[0]), d(f[1]), d(f[2]), d(f[3]), d(f[5]), d(f[6]), d(f[8]), d(f[9]), d(f[11]), d(f[12]),
        d(f[14]), d(f[15]), d(f[17]), d(f[18]),
    ];
    let mut bad = 0u32;
    for &x in &ds {
        bad |= (x > 9) as u32;
    }
    if bad != 0 || f[4] != b'-' || f[7] != b'-' || f[10] != b'T' || f[13] != b':' || f[16] != b':' {
        return None;
    }
    let y = ds[0] * 1000 + ds[1] * 100 + ds[2] * 10 + ds[3];
    let mo = ds[4] * 10 + ds[5];
    let dd = ds[6] * 10 + ds[7];
    let h = ds[8] * 10 + ds[9];
    let mi = ds[10] * 10 + ds[11];
    let s = ds[12] * 10 + ds[13];
    if !(1..=12).contains(&mo) || h > 23 || mi > 59 || s > 59 || dd == 0 {
        return None;
    }
    const MDAYS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let dim = if mo == 2 && leap(y) { 29 } else { MDAYS[(mo - 1) as usize] };
    if dd > dim {
        return None;
    }
    let days = days_from_civil(y as i64, mo as i64, dd as i64);
    Some(((days * 24 + h as i64) * 60 + mi as i64) * MIN + s as i64 * SEC)
}

/// F1 only (ingest_ts).
#[allow(dead_code)]
pub fn parse_f1_slow(f: &[u8]) -> Option<i64> {
    if f.len() != 20 || f[19] != b'Z' {
        return None;
    }
    civil(f)
}

/// Any of F1/F2/F3; `None` for a bad timestamp. An F3 value too large for
/// i64 is returned as `None` too (it is outside every window anyway).
#[allow(dead_code)]
pub fn parse_slow(f: &[u8]) -> Option<i64> {
    let n = f.len();
    if n == 20 && f[4] == b'-' {
        if f[19] != b'Z' {
            return None;
        }
        return civil(f);
    }
    if n == 25 && f[4] == b'-' {
        let base = civil(f)?;
        let sign = f[19];
        let (a, b, c, e) = (d(f[20]), d(f[21]), d(f[23]), d(f[24]));
        if (sign != b'+' && sign != b'-') || f[22] != b':' || a > 9 || b > 9 || c > 9 || e > 9 {
            return None;
        }
        let oh = a * 10 + b;
        let om = c * 10 + e;
        if oh > 23 || om > 59 {
            return None;
        }
        let off = (oh as i64 * 60 + om as i64) * MIN;
        return Some(if sign == b'+' { base - off } else { base + off });
    }
    parse_digits(f)
}

#[inline]
fn parse_digits(f: &[u8]) -> Option<i64> {
    if f.is_empty() {
        return None;
    }
    let mut v: u64 = 0;
    for &b in f {
        let x = d(b);
        if x > 9 {
            return None;
        }
        v = v.checked_mul(10)?.checked_add(x as u64)?;
    }
    if v > i64::MAX as u64 { None } else { Some(v as i64) }
}

/// price_cents of a purchase: digits only, 1..=10,000,000.
#[inline]
pub fn parse_price(f: &[u8]) -> Option<u32> {
    if f.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    for &b in f {
        let x = d(b);
        if x > 9 {
            return None;
        }
        v = v * 10 + x;
        if v > 10_000_000 {
            return None;
        }
    }
    if v == 0 { None } else { Some(v) }
}


// ---------------------------------------------------------------- fast paths
// Callers guarantee at least 8 readable bytes past the end of every field
// (inputs are padded), so fixed-width loads may run past a short field.

#[inline(always)]
fn ld8(p: *const u8) -> u64 {
    u64::from_le_bytes(unsafe { std::ptr::read_unaligned(p as *const [u8; 8]) })
}

const ZEROS: u64 = 0x3030_3030_3030_3030;

/// Bytes of `x` (already xor'ed with '0's) selected by `m` are all ASCII digits.
#[inline(always)]
fn all_digits(x: u64, m: u64) -> bool {
    ((x | x.wrapping_add(0x7676_7676_7676_7676)) & 0x8080_8080_8080_8080 & m) == 0
}

#[inline(always)]
fn byte(x: u64, i: u32) -> u32 {
    ((x >> (8 * i)) & 0xff) as u32
}

/// `YYYY-MM-DDTHH:MM:SS` at `p` (19 bytes) -> ms, with every check.
#[inline(always)]
fn civil_fast(p: *const u8) -> Option<i64> {
    let a = ld8(p); // Y Y Y Y - M M -
    let b = ld8(unsafe { p.add(8) }); // D D T H H : M M
    let c = ld8(unsafe { p.add(16) }) & 0xff_ffff; // : S S
    // separators
    const SA_M: u64 = 0xff00_00ff_0000_0000;
    const SA_V: u64 = (b'-' as u64) << 56 | (b'-' as u64) << 32;
    const SB_M: u64 = 0x0000_ff00_00ff_0000;
    const SB_V: u64 = (b':' as u64) << 40 | (b'T' as u64) << 16;
    if (a & SA_M) != SA_V || (b & SB_M) != SB_V || (c & 0xff) != b':' as u64 {
        return None;
    }
    let xa = a ^ ZEROS;
    let xb = b ^ ZEROS;
    let xc = c ^ 0x30_3030;
    if !all_digits(xa, 0x0080_8000_8080_8080) || !all_digits(xb, 0x8080_0080_8000_8080) || !all_digits(xc, 0x80_8000) {
        return None;
    }
    let y = byte(xa, 0) * 1000 + byte(xa, 1) * 100 + byte(xa, 2) * 10 + byte(xa, 3);
    let mo = byte(xa, 5) * 10 + byte(xa, 6);
    let dd = byte(xb, 0) * 10 + byte(xb, 1);
    let h = byte(xb, 3) * 10 + byte(xb, 4);
    let mi = byte(xb, 6) * 10 + byte(xb, 7);
    let s = byte(xc, 1) * 10 + byte(xc, 2);
    if mo.wrapping_sub(1) > 11 || h > 23 || mi > 59 || s > 59 || dd == 0 {
        return None;
    }
    const MDAYS: [u32; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let dim = MDAYS[(mo - 1) as usize] + (mo == 2 && leap(y)) as u32;
    if dd > dim {
        return None;
    }
    let days = days_from_civil(y as i64, mo as i64, dd as i64);
    Some(((days * 24 + h as i64) * 60 + mi as i64) * MIN + s as i64 * SEC)
}

/// F1 only (ingest_ts).
#[inline(always)]
pub fn parse_f1(f: &[u8]) -> Option<i64> {
    if f.len() != 20 || f[19] != b'Z' {
        return None;
    }
    civil_fast(f.as_ptr())
}

/// 8 ASCII digits (xor'ed, little-endian load) -> value.
#[inline(always)]
fn swar8(x: u64) -> u64 {
    let x = (x * 10).wrapping_add(x >> 8);
    (((x & 0x0000_00ff_0000_00ff).wrapping_mul(100 + (1_000_000 << 32)))
        .wrapping_add(((x >> 16) & 0x0000_00ff_0000_00ff).wrapping_mul(1 + (10_000 << 32))))
        >> 32
}

#[inline(always)]
fn digits_fast(f: &[u8]) -> Option<i64> {
    let mut n = f.len();
    let mut p = f.as_ptr();
    if n == 0 {
        return None;
    }
    // validate every byte, 8 at a time (tail masked)
    let mut i = 0;
    while i < n {
        let x = ld8(unsafe { p.add(i) }) ^ ZEROS;
        let left = n - i;
        let m = if left >= 8 { 0x8080_8080_8080_8080 } else { 0x8080_8080_8080_8080u64 >> (8 * (8 - left)) };
        if !all_digits(x, m) {
            return None;
        }
        i += 8;
    }
    // strip leading zeros so at most 19 significant digits remain
    while n > 19 && unsafe { *p } == b'0' {
        p = unsafe { p.add(1) };
        n -= 1;
    }
    if n > 19 {
        return None; // too large for i64: outside every window
    }
    let mut v: u64 = 0;
    let head = n % 8;
    for j in 0..head {
        v = v * 10 + (unsafe { *p.add(j) } - b'0') as u64;
    }
    let mut j = head;
    while j < n {
        v = v * 100_000_000 + swar8(ld8(unsafe { p.add(j) }) ^ ZEROS);
        j += 8;
    }
    if v > i64::MAX as u64 { None } else { Some(v as i64) }
}

/// Any of F1/F2/F3; `None` for a bad timestamp (an F3 too large for i64 too).
#[inline(always)]
pub fn parse(f: &[u8]) -> Option<i64> {
    let n = f.len();
    if n == 20 || n == 25 {
        let p = f.as_ptr();
        if f[4] == b'-' {
            if n == 20 {
                if f[19] != b'Z' {
                    return None;
                }
                return civil_fast(p);
            }
            let base = civil_fast(p)?;
            let o = ld8(unsafe { p.add(19) }) & 0xffff_ffff_ffff; // +HH:MM
            let sign = (o & 0xff) as u8;
            if (sign != b'+' && sign != b'-') || ((o >> 24) & 0xff) != b':' as u64 {
                return None;
            }
            let x = o ^ 0x30_3000_3030_00;
            if !all_digits(x, 0x80_8000_8080_00) {
                return None;
            }
            let oh = byte(x, 1) * 10 + byte(x, 2);
            let om = byte(x, 4) * 10 + byte(x, 5);
            if oh > 23 || om > 59 {
                return None;
            }
            let off = (oh as i64 * 60 + om as i64) * MIN;
            return Some(if sign == b'+' { base - off } else { base + off });
        }
    }
    digits_fast(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn forms() {
        let f1 = parse(b"2026-10-03T23:58:10Z").unwrap();
        assert_eq!(f1, 1791071890000);
        assert_eq!(parse(b"2026-10-03T19:58:10-04:00"), Some(f1));
        assert_eq!(parse(b"1791071890000"), Some(f1));
        assert_eq!(parse(b"0001791071890000"), Some(f1));
        assert_eq!(parse(b"2026-10-04T01:30:00+02:00"), parse(b"2026-10-03T23:30:00Z"));
        assert_eq!(parse(b"2026-10-03T23:30:00-00:00"), parse(b"2026-10-03T23:30:00Z"));
        for bad in [
            &b""[..], b"2026-10-03T23:58:10z", b"2026-10-03t23:58:10Z", b"2026-10-03 23:58:10Z",
            b"2026-10-03T23:58:10", b"2026-13-03T23:58:10Z", b"2026-02-30T23:58:10Z",
            b"2027-02-29T23:58:10Z", b"2026-10-03T24:58:10Z", b"2026-10-03T23:58:60Z",
            b"2026-10-03T23:58:10+24:00", b"2026-10-03T23:58:10+05:60", b"2026-10-03T23:58:10+0200",
            b"1791071890000.0", b"1.79e12", b"-5", b"2026-10-03T23:58:10.123Z",
            b"999999999999999999999",
        ] {
            assert_eq!(parse(bad), None, "{:?}", std::str::from_utf8(bad));
        }
        assert!(parse(b"2028-02-29T00:00:00Z").is_some());
        assert_eq!(parse(b"86400000"), Some(86_400_000));
        assert_eq!(parse_price(b"0250"), Some(250));
        assert_eq!(parse_price(b"10000000"), Some(10_000_000));
        assert_eq!(parse_price(b"10000001"), None);
        assert_eq!(parse_price(b"00"), None);
    }

    fn padded(s: &[u8]) -> Vec<u8> {
        let mut v = s.to_vec();
        v.resize(s.len() + 16, 0);
        v
    }

    #[test]
    fn fast_matches_slow() {
        let mut seed = 0x1234_5678_9abc_def0u64;
        let mut rnd = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let alphabet = b"0123456789-:TZ+tz .9e";
        let bases: [&[u8]; 6] = [
            b"2026-10-03T23:58:10Z", b"2028-02-29T00:00:00Z", b"2026-10-03T19:58:10-04:00",
            b"1791071890000", b"0001791071890000", b"9223372036854775807",
        ];
        for _ in 0..2_000_000 {
            let base = bases[(rnd() % 6) as usize];
            let mut v = base.to_vec();
            for _ in 0..(rnd() % 3) {
                let i = (rnd() as usize) % v.len();
                v[i] = alphabet[(rnd() as usize) % alphabet.len()];
            }
            if rnd() % 10 == 0 {
                v.truncate((rnd() as usize) % (v.len() + 1));
            }
            if rnd() % 10 == 0 {
                v.extend_from_slice(b"0000000");
            }
            let p = padded(&v);
            let f = &p[..v.len()];
            assert_eq!(parse(f), parse_slow(f), "{:?}", String::from_utf8_lossy(f));
            assert_eq!(parse_f1(f), parse_f1_slow(f), "{:?}", String::from_utf8_lossy(f));
        }
        for t in [&b"18446744073709551615"[..], b"9223372036854775808", b"00000000000000000000009223372036854775807", b"0", b"00000000"] {
            let p = padded(t);
            assert_eq!(parse(&p[..t.len()]), parse_slow(t));
        }
    }
}
