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
#[inline]
pub fn parse_f1(f: &[u8]) -> Option<i64> {
    if f.len() != 20 || f[19] != b'Z' {
        return None;
    }
    civil(f)
}

/// Any of F1/F2/F3; `None` for a bad timestamp. An F3 value too large for
/// i64 is returned as `None` too (it is outside every window anyway).
#[inline]
pub fn parse(f: &[u8]) -> Option<i64> {
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
}
