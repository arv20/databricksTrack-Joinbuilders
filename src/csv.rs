//! The CSV dialect: `\n` only, comma separated, a field is quoted only when
//! the whole field is `"..."` with no quote inside; any other quote, or a wrong
//! field count, makes the line malformed.

/// Splits `line` (no trailing `\n`) into exactly `N` unquoted fields.
#[inline(always)]
pub fn split<'a, const N: usize>(line: &'a [u8], out: &mut [&'a [u8]; N]) -> bool {
    let mut nf = 0usize;
    let mut start = 0usize;
    let mut quotes = 0u32;
    let len = line.len();
    let mut i = 0usize;
    while i < len {
        let b = line[i];
        if b == b',' {
            if nf == N - 1 {
                return false;
            }
            match finish(line, start, i, quotes) {
                Some(f) => out[nf] = f,
                None => return false,
            }
            nf += 1;
            start = i + 1;
            quotes = 0;
        } else if b == b'"' {
            quotes += 1;
        }
        i += 1;
    }
    if nf != N - 1 {
        return false;
    }
    match finish(line, start, len, quotes) {
        Some(f) => out[nf] = f,
        None => return false,
    }
    true
}

#[inline(always)]
fn finish(line: &[u8], s: usize, e: usize, quotes: u32) -> Option<&[u8]> {
    if quotes == 0 {
        Some(&line[s..e])
    } else if quotes == 2 && e - s >= 2 && line[s] == b'"' && line[e - 1] == b'"' {
        Some(&line[s + 1..e - 1])
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dialect() {
        let mut f = [&b""[..]; 2];
        assert!(split(b"a,b", &mut f) && f == [&b"a"[..], b"b"]);
        assert!(split(b"\"a\",\"\"", &mut f) && f == [&b"a"[..], b""]);
        assert!(split(b",", &mut f) && f == [&b""[..], b""]);
        assert!(!split(b"a,b,c", &mut f));
        assert!(!split(b"a", &mut f));
        assert!(!split(b"", &mut f));
        assert!(!split(b"a,b\"", &mut f));
        assert!(!split(b"\"a,b\"", &mut f));
        assert!(!split(b"\"a\"\",b", &mut f));
        assert!(!split(b"\",b", &mut f));
        assert!(!split(b"\"a\"b\",b", &mut f));
        assert!(split(b" \"a\" ,b", &mut f) == false);
    }
}
