//! Read-only file mappings with at least one zero page of padding after the
//! data, so 16-byte over-reads past any value are always safe.

use std::fs::File;
use std::os::unix::io::AsRawFd;
use std::path::Path;

pub const PAD: usize = 64;

pub fn page_size() -> usize {
    unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize }
}

/// Maps `path` and returns a `'static` slice of its bytes (the mapping is never
/// unmapped). At least `PAD` readable zero bytes follow the slice.
pub fn map(path: &Path) -> std::io::Result<&'static [u8]> {
    let f = File::open(path)?;
    let len = f.metadata()?.len() as usize;
    let page = page_size();
    let total = len.div_ceil(page) * page + page;
    unsafe {
        let base = libc::mmap(
            std::ptr::null_mut(),
            total,
            libc::PROT_READ,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        );
        if base == libc::MAP_FAILED {
            return Err(std::io::Error::last_os_error());
        }
        if len > 0 {
            let flags = libc::MAP_PRIVATE | libc::MAP_FIXED | populate_flag();
            let p = libc::mmap(base, len, libc::PROT_READ, flags, f.as_raw_fd(), 0);
            if p == libc::MAP_FAILED {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(std::slice::from_raw_parts(base as *const u8, len))
    }
}

/// Reads a small file into a leaked, padded buffer.
pub fn read_small(path: &Path) -> std::io::Result<&'static [u8]> {
    let mut v = std::fs::read(path)?;
    let len = v.len();
    v.resize(len + PAD, 0);
    let s: &'static [u8] = Vec::leak(v);
    Ok(&s[..len])
}

#[cfg(target_os = "linux")]
fn populate_flag() -> i32 {
    if std::env::var_os("SS_POPULATE").is_some() { libc::MAP_POPULATE } else { 0 }
}
#[cfg(not(target_os = "linux"))]
fn populate_flag() -> i32 {
    0
}

/// Pre-faults the page-cache pages of `[s, e)` of a mapping in one call
/// (MADV_POPULATE_READ, Linux 5.14+); silently does nothing elsewhere.
#[inline]
pub fn populate(data: &[u8], s: usize, e: usize) {
    #[cfg(target_os = "linux")]
    unsafe {
        let page = 4096usize;
        let a = (data.as_ptr() as usize + s) & !(page - 1);
        let b = data.as_ptr() as usize + e.min(data.len());
        if b > a {
            libc::madvise(a as *mut libc::c_void, b - a, 22 /* MADV_POPULATE_READ */);
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = (data, s, e);
}
