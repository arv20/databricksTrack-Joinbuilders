//! Byte-level input handling: file mapping, chunking, the CSV dialect, timestamps.
use std::hash::BuildHasher;
use std::path::Path;

pub type B = &'static [u8];

pub const DAY_MS: i64 = 86_400_000;

/// Fixed-seed hash of a byte string (deterministic across runs).
#[inline]
pub fn hash(b: &[u8]) -> u64 {
    foldhash::fast::FixedState::with_seed(0x9E37_79B9_7F4A_7C15).hash_one(b)
}

/// Map a file read-only and leak the mapping, so slices live for the whole run.
pub fn map_file(path: &Path) -> B {
    let f = std::fs::File::open(path).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return &[];
    }
    let m = unsafe { memmap2::Mmap::map(&f) }.unwrap_or_else(|e| panic!("mmap {}: {e}", path.display()));
    let m: &'static memmap2::Mmap = Box::leak(Box::new(m));
    &m[..]
}

/// Every *.csv file in a directory (missing directory = no files).
pub fn map_dir(dir: &Path) -> Vec<B> {
    let mut paths: Vec<_> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_file() && p.file_name().map_or(false, |n| n.as_encoded_bytes().ends_with(b".csv")))
            .collect(),
        Err(_) => Vec::new(),
    };
    paths.sort();
    paths.iter().map(|p| map_file(p)).collect()
}

/// Split the files into chunks that end at line ends. The header line of each file is skipped.
pub fn chunks(files: &[B], target: usize) -> Vec<B> {
    let target = target.max(1);
    let mut out = Vec::new();
    for &f in files {
        let body = match memchr::memchr(b'\n', f) {
            Some(p) => &f[p + 1..],
            None => continue,
        };
        let mut s = 0;
        while s < body.len() {
            let mut e = (s + target).min(body.len());
            if e < body.len() {
                e = match memchr::memchr(b'\n', &body[e..]) {
                    Some(p) => e + p + 1,
                    None => body.len(),
                };
            }
            out.push(&body[s..e]);
            s = e;
        }
    }
    out
}

pub fn chunk_target(files: &[B], ncpu: usize) -> usize {
    let total: usize = files.iter().map(|f| f.len()).sum();
    (total / (ncpu * 16).max(1)).clamp(1 << 20, 16 << 20)
}

/// Call f on every non-empty line of a chunk. Empty lines are malformed for every file kind.
#[inline]
pub fn for_lines(chunk: B, mut f: impl FnMut(B)) {
    let mut s = 0;
    for p in memchr::memchr_iter(b'\n', chunk) {
        if p > s {
            f(&chunk[s..p]);
        }
        s = p + 1;
    }
    if s < chunk.len() {
        f(&chunk[s..]);
    }
}

#[inline]
fn unquote(f: B, has_quote: bool) -> Option<B> {
    if !has_quote {
        return Some(f);
    }
    let n = f.len();
    if n >= 2 && f[0] == b'"' && f[n - 1] == b'"' && memchr::memchr(b'"', &f[1..n - 1]).is_none() {
        Some(&f[1..n - 1])
    } else {
        None
    }
}

/// Split a line into exactly N fields, removing quotes around whole fields.
/// None = malformed (wrong field count or a stray quote).
#[inline]
pub fn split<const N: usize>(line: B) -> Option<[B; N]> {
    let mut out: [B; N] = [&[]; N];
    let mut k = 0usize;
    let mut start = 0usize;
    let mut q = false;
    for (i, &b) in line.iter().enumerate() {
        if b == b',' {
            if k + 1 >= N {
                return None;
            }
            out[k] = unquote(&line[start..i], q)?;
            k += 1;
            start = i + 1;
            q = false;
        } else if b == b'"' {
            q = true;
        }
    }
    if k + 1 != N {
        return None;
    }
    out[k] = unquote(&line[start..], q)?;
    Some(out)
}

#[inline]
fn d2(a: u8, b: u8) -> Option<i64> {
    if a.is_ascii_digit() && b.is_ascii_digit() { Some(((a - b'0') * 10 + (b - b'0')) as i64) } else { None }
}

pub fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn is_leap(y: i64) -> bool {
    y % 4 == 0 && (y % 100 != 0 || y % 400 == 0)
}

/// The "YYYY-MM-DDTHH:MM:SS" prefix as UTC ms, with full calendar validation.
#[inline]
fn civil(s: &[u8]) -> Option<i64> {
    if s[4] != b'-' || s[7] != b'-' || s[10] != b'T' || s[13] != b':' || s[16] != b':' {
        return None;
    }
    let y = d2(s[0], s[1])? * 100 + d2(s[2], s[3])?;
    let mo = d2(s[5], s[6])?;
    let d = d2(s[8], s[9])?;
    let h = d2(s[11], s[12])?;
    let mi = d2(s[14], s[15])?;
    let se = d2(s[17], s[18])?;
    if !(1..=12).contains(&mo) || h > 23 || mi > 59 || se > 59 || d < 1 {
        return None;
    }
    let dim = match mo {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ => if is_leap(y) { 29 } else { 28 },
    };
    if d > dim {
        return None;
    }
    Some((days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se) * 1000)
}

/// Parse a timestamp in form F1, F2 or F3 to epoch ms. None = bad.
/// F3 values with more than 18 significant digits return None: they are outside every window.
#[inline]
pub fn parse_ts(s: &[u8]) -> Option<i64> {
    if s.is_empty() {
        return None;
    }
    if s.iter().all(|b| b.is_ascii_digit()) {
        let mut i = 0;
        while i < s.len() && s[i] == b'0' {
            i += 1;
        }
        let r = &s[i..];
        if r.len() > 18 {
            return None;
        }
        let mut v: i64 = 0;
        for &b in r {
            v = v * 10 + (b - b'0') as i64;
        }
        return Some(v);
    }
    match s.len() {
        20 => {
            if s[19] != b'Z' {
                return None;
            }
            civil(s)
        }
        25 => {
            let sign = match s[19] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            if s[22] != b':' {
                return None;
            }
            let oh = d2(s[20], s[21])?;
            let om = d2(s[23], s[24])?;
            if oh > 23 || om > 59 {
                return None;
            }
            civil(s).map(|t| t - sign * (oh * 3600 + om * 60) * 1000)
        }
        _ => None,
    }
}

/// Purchase price: ASCII digits only, value 1..=10,000,000.
#[inline]
pub fn parse_price(s: &[u8]) -> Option<u32> {
    if s.is_empty() || !s.iter().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut i = 0;
    while i < s.len() && s[i] == b'0' {
        i += 1;
    }
    let r = &s[i..];
    if r.len() > 8 {
        return None;
    }
    let mut v: u32 = 0;
    for &b in r {
        v = v * 10 + (b - b'0') as u32;
    }
    if (1..=10_000_000).contains(&v) { Some(v) } else { None }
}

#[inline]
pub fn day_of(ts: i64) -> i32 {
    ts.div_euclid(DAY_MS) as i32
}

/// Read window_start and window_end from manifest.json.
pub fn manifest(data: &[u8]) -> (i64, i64) {
    let get = |key: &[u8]| -> i64 {
        let pos = memchr::memmem::find(data, key).unwrap_or_else(|| panic!("manifest: no {}", String::from_utf8_lossy(key)));
        let rest = &data[pos + key.len()..];
        let colon = memchr::memchr(b':', rest).expect("manifest: no colon");
        let rest = &rest[colon + 1..];
        let q1 = memchr::memchr(b'"', rest).expect("manifest: no value");
        let rest = &rest[q1 + 1..];
        let q2 = memchr::memchr(b'"', rest).expect("manifest: unterminated value");
        parse_ts(&rest[..q2]).unwrap_or_else(|| panic!("manifest: bad timestamp {}", String::from_utf8_lossy(&rest[..q2])))
    };
    (get(b"\"window_start\""), get(b"\"window_end\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ts() {
        assert_eq!(parse_ts(b"2026-10-03T23:58:10Z"), Some(1791071890000));
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-04:00"), Some(1791071890000));
        assert_eq!(parse_ts(b"1791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"0001791071890000"), Some(1791071890000));
        assert_eq!(parse_ts(b"2026-10-03T23:58:10z"), None);
        assert_eq!(parse_ts(b"2026-02-29T00:00:00Z"), None);
        assert_eq!(parse_ts(b"2028-02-29T00:00:00Z").is_some(), true);
        assert_eq!(parse_ts(b"2026-10-03T19:58:10+24:00"), None);
        assert_eq!(parse_ts(b"2026-10-03T19:58:10+0200"), None);
        assert_eq!(parse_ts(b"2026-10-03T19:58:10-00:00"), parse_ts(b"2026-10-03T19:58:10Z"));
        assert_eq!(parse_ts(b"2026-10-03T24:00:00Z"), None);
        assert_eq!(parse_ts(b"2026-10-03T23:59:60Z"), None);
        assert_eq!(parse_ts(b"1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_price(b"0250"), Some(250));
        assert_eq!(parse_price(b"10000000"), Some(10_000_000));
        assert_eq!(parse_price(b"10000001"), None);
        assert_eq!(parse_price(b"0"), None);
        assert_eq!(split::<2>(b"\"a\",\"\""), Some([&b"a"[..], &b""[..]]));
        assert_eq!(split::<2>(b"a,b\""), None);
        assert_eq!(split::<2>(b"a,b,c"), None);
        assert_eq!(split::<2>(b"a"), None);
    }
}
