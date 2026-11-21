use crate::keyrec;
use crate::k_ref;
use crate::sys::*;
use crate::*;
use std::os::fd::AsRawFd;
use std::path::Path;

fn field(raw: &[u8]) -> Option<&[u8]> {
    let q = raw.iter().filter(|&&b| b == b'"').count();
    if q == 0 {
        Some(raw)
    } else if q == 2 && raw.len() >= 2 && raw[0] == b'"' && raw[raw.len() - 1] == b'"' {
        Some(&raw[1..raw.len() - 1])
    } else {
        None
    }
}
pub fn split2(line: &[u8]) -> Option<(&[u8], &[u8])> {
    let c = line.iter().position(|&b| b == b',')?;
    let rest = &line[c + 1..];
    if rest.contains(&b',') {
        return None;
    }
    Some((field(&line[..c])?, field(rest)?))
}

fn manifest_ts(txt: &[u8], key: &[u8]) -> Option<i64> {
    let at = txt.windows(key.len()).position(|w| w == key)? + key.len();
    let rest = &txt[at..];
    let colon = rest.iter().position(|&b| b == b':')?;
    let rest = &rest[colon + 1..];
    let q1 = rest.iter().position(|&b| b == b'"')?;
    let rest = &rest[q1 + 1..];
    let q2 = rest.iter().position(|&b| b == b'"')?;
    let v = k_ref::parse_f1(&rest[..q2]);
    (v != crate::iface::TS_BAD).then_some(v)
}

#[inline]
fn find3(b: &[u8], s: usize) -> (usize, u8) {
    use std::arch::x86_64::*;
    let mut i = s;
    unsafe {
        let (nl, cm, qt) = (_mm256_set1_epi8(b'\n' as i8), _mm256_set1_epi8(b',' as i8), _mm256_set1_epi8(b'"' as i8));
        while i + 32 <= b.len() {
            let v = _mm256_loadu_si256(b.as_ptr().add(i) as *const __m256i);
            let m = _mm256_movemask_epi8(_mm256_or_si256(_mm256_or_si256(_mm256_cmpeq_epi8(v, nl), _mm256_cmpeq_epi8(v, cm)), _mm256_cmpeq_epi8(v, qt))) as u32;
            if m != 0 {
                let p = i + m.trailing_zeros() as usize;
                return (p, b[p]);
            }
            i += 32;
        }
    }
    while i < b.len() {
        let c = b[i];
        if c == b'\n' || c == b',' || c == b'"' {
            return (i, c);
        }
        i += 1;
    }
    (b.len(), b'\n')
}

fn data_lines<'a>(txt: &'a [u8], mut f: impl FnMut(&'a [u8])) {
    let Some(h) = txt.iter().position(|&b| b == b'\n') else { return };
    let body = &txt[h + 1..];
    k_ref::for_each_line(body, |s, e| f(&body[s..e]));
}

fn list(dir: &Path) -> Vec<(std::path::PathBuf, usize)> {
    let mut v = Vec::new();
    let Ok(rd) = std::fs::read_dir(dir) else { fallback("cannot list an input folder") };
    for e in rd.flatten() {
        let p = e.path();
        let name = e.file_name();
        if !name.as_encoded_bytes().ends_with(b".csv") {
            continue;
        }
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if md.is_file() {
            v.push((p, md.len() as usize));
        }
    }
    v
}

struct Lookups {
    prod: Vec<u8>,
    cats: Vec<Vec<u8>>,
    camp: Vec<u8>,
    channels: Vec<Vec<u8>>,
    nc: usize,
}

fn lookups(data: &Path) -> Lookups {
    let rd = |name: &str| std::fs::read(data.join(name)).unwrap_or_else(|_| fallback("cannot read a lookup file"));
    let ptxt = rd("products.csv");
    let mut prod = vec![255u8; PBOUND + 1];
    let mut ctab = [255u8; 64];
    let mut bad = false;
    let body: &[u8] = match ptxt.iter().position(|&b| b == b'\n') {
        Some(h) => &ptxt[h + 1..],
        None => &[],
    };
    let mut add = |n: usize, c: &[u8], bad: &mut bool, names: &mut Vec<Vec<u8>>| {
        if n >= PBOUND || prod[n] != 255 {
            *bad = true;
            return;
        }
        let mut h = 0xcbf2_9ce4_8422_2325u64;
        for &x in c {
            h = (h ^ x as u64).wrapping_mul(0x100_0000_01b3);
        }
        let mut slot = (h >> 58) as usize;
        let ci = loop {
            let k = ctab[slot];
            if k == 255 {
                if names.len() >= 31 {
                    *bad = true;
                    return;
                }
                names.push(c.to_vec());
                ctab[slot] = (names.len() - 1) as u8;
                break names.len() - 1;
            }
            if names[k as usize] == c {
                break k as usize;
            }
            slot = (slot + 1) & 63;
        };
        prod[n] = ci as u8;
    };
    let mut nm: Vec<Vec<u8>> = Vec::new();
    let mut s = 0usize;
    while s < body.len() {
        let (e, stop) = find3(body, s);
        let mut fast = false;
        if stop == b',' && body[s] == b'p' && e - s >= 2 && e - s <= 9 && !(body[s + 1] == b'0' && e - s > 2) && body[s + 1..e].iter().all(|c| c.is_ascii_digit()) {
            let (e2, stop2) = find3(body, e + 1);
            if stop2 == b'\n' && e2 > e + 1 {
                let mut n = 0usize;
                for &c in &body[s + 1..e] {
                    n = n * 10 + (c - b'0') as usize;
                }
                add(n, &body[e + 1..e2], &mut bad, &mut nm);
                s = e2 + 1;
                fast = true;
            }
        }
        if !fast {
            let le = body[s..].iter().position(|&b| b == b'\n').map_or(body.len(), |p| s + p);
            let l = &body[s..le];
            match split2(l) {
                Some((p, c)) if !c.is_empty() => add(k_ref::parse_product(p) as usize, c, &mut bad, &mut nm),
                _ => bad = true,
            }
            s = le + 1;
        }
        if bad {
            break;
        }
    }
    let names: Vec<&[u8]> = nm.iter().map(|v| v.as_slice()).collect();
    if bad {
        fallback("products.csv is not canonical, or has too many categories");
    }
    let mut cats: Vec<Vec<u8>> = names.iter().map(|c| c.to_vec()).collect();
    cats.push(b"unknown".to_vec());
    cats.sort_unstable();
    cats.dedup();
    if cats.len() > 31 {
        fallback("too many categories");
    }
    let unknown = cats.iter().position(|c| c == b"unknown").unwrap() as u8;
    let remap: Vec<u8> = names.iter().map(|c| cats.iter().position(|x| x == c).unwrap() as u8).collect();
    for p in prod.iter_mut() {
        *p = if *p == 255 { unknown } else { remap[*p as usize] };
    }
    let ctxt = rd("campaigns.csv");
    let mut camp = vec![255u8; CBOUND];
    let mut channels: Vec<Vec<u8>> = Vec::new();
    let mut nc = 0usize;
    let mut bad = false;
    data_lines(&ctxt, |l| match split2(l) {
        Some((c, ch)) if !ch.is_empty() => {
            let n = k_ref::parse_campaign(c) as usize;
            if n >= CBOUND || camp[n] != 255 {
                bad = true;
                return;
            }
            let ci = match channels.iter().position(|x| x == ch) {
                Some(i) => i,
                None => {
                    channels.push(ch.to_vec());
                    channels.len() - 1
                }
            };
            if ci >= 250 {
                bad = true;
                return;
            }
            camp[n] = ci as u8;
            nc = nc.max(n + 1);
        }
        _ => bad = true,
    });
    if bad {
        fallback("campaigns.csv is not canonical");
    }

    Lookups { prod, cats, camp, channels, nc }
}

pub fn setup(data: &str, nt: usize, check: bool) -> G {
    let data = Path::new(data);
    let rd = |name: &str| std::fs::read(data.join(name)).unwrap_or_else(|_| fallback("cannot read a lookup file"));
    let man = rd("manifest.json");
    let (Some(ws), Some(we)) = (manifest_ts(&man, b"\"window_start\""), manifest_ts(&man, b"\"window_end\"")) else { fallback("manifest") };
    if we <= ws || we - ws > 24 * DAY_MS || ws < 8 * DAY_MS {
        fallback("window");
    }
    crate::sys::WORKERS.store(true, std::sync::atomic::Ordering::SeqCst);
    let lk_handle = {
        let d = data.to_path_buf();
        std::thread::spawn(move || lookups(&d))
    };
    let mut all: Vec<(Kind, std::path::PathBuf, usize)> = Vec::new();
    for (kind, dir) in [(Kind::Ev, "events"), (Kind::Ck, "clicks"), (Kind::Lk, "links")] {
        for (p, len) in list(&data.join(dir)) {
            if len > 0 {
                all.push((kind, p, len));
            }
        }
    }
    all.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
    lap("s.list");
    let pg = 4096usize;
    let total: usize = all.iter().map(|f| ((f.2 + pg - 1) & !(pg - 1)) + pg).sum::<usize>() + pg;
    if total as u64 >= (1u64 << 40) {
        fallback("input too large");
    }
    let region = unsafe { mmap(std::ptr::null_mut(), total, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS | MAP_NORESERVE, -1, 0) };
    if region as isize == -1 {
        fallback("mmap region");
    }
    let mut files = Vec::with_capacity(all.len());
    let mut off = pg;
    let (mut ev_bytes, mut ck_bytes, mut lk_bytes) = (0usize, 0usize, 0usize);
    for (kind, p, len) in &all {
        let Ok(f) = std::fs::File::open(p) else { fallback("open input") };
        let q = unsafe { mmap(region.add(off), *len, PROT_READ, MAP_PRIVATE | MAP_FIXED, f.as_raw_fd(), 0) };
        if q as isize == -1 {
            fallback("mmap input");
        }
        match kind {
            Kind::Ev => ev_bytes += len,
            Kind::Ck => ck_bytes += len,
            Kind::Lk => lk_bytes += len,
        }
        files.push(FileInfo { kind: *kind, off, len: *len, hdr: usize::MAX });
        unsafe { crate::parse::keep_fd(f) };
        off += ((len + pg - 1) & !(pg - 1)) + pg;
    }
    lap("s.mmap");
    let total_bytes = ev_bytes + ck_bytes + lk_bytes;
    let ch = (total_bytes / (nt * 16)).clamp(64 << 10, 2 << 20);
    let mut queue = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let mut a = 1;
        while a < f.len {
            let b = (a + ch).min(f.len);
            queue.push(Chunk { file: i as u32, a, b });
            a = b;
        }
    }

    let mut sample = keyrec::Sample::default();
    for f in files.iter().filter(|f| f.kind == Kind::Ev) {
        if sample.events.len() >= 5000 {
            break;
        }
        let all = unsafe { std::slice::from_raw_parts(region.add(f.off), f.len) };
        let hdr = all.iter().position(|&b| b == b'\n').map_or(f.len, |i| i + 1);
        let bytes = &all[hdr..];
        let mut cut = bytes.len().min(600_000);
        if cut < bytes.len() {
            while cut > 0 && bytes[cut - 1] != b'\n' {
                cut -= 1;
            }
        }
        sample.add_event_lines(&bytes[..cut], 5000);
    }
    lap("s.sample");
    let mut st = keyrec::Stats::default();
    let envkey = |n: &str| if cfg!(feature = "testkeys") { std::env::var(n).ok().and_then(|v| u64::from_str_radix(&v, 16).ok()) } else { None };
    let (ukey, ekey) = match (envkey("XL_UKEY"), envkey("XL_EKEY")) {
        (Some(u), Some(e)) => (u, e),
        _ => {
            let Some(ukey) = keyrec::recover_user_key(&sample, &mut st) else { fallback("no user key") };
            let Some(ekey) = keyrec::recover_event_key(&sample, ukey, Some(ws), &mut st) else { fallback("no event key") };
            (ukey, ekey)
        }
    };
    if check {
        eprintln!("[v2b] keys: XL_UKEY={ukey:x} XL_EKEY={ekey:x}");
    }

    lap("maps+keys");
    let Lookups { prod, cats, camp, channels, nc } = lk_handle.join().unwrap_or_else(|_| fallback("lookup thread"));
    lap("lookups");
    let ecap = ((ev_bytes / 64 + 4096 + BSIZE - 1) & !(BSIZE - 1)) as u64;
    let ucap = (((ecap as usize / 16 + 4096 + UBSIZE - 1) & !(UBSIZE - 1)) as u64).min(1 << 26);
    let nb = (ecap as usize) >> BB;
    let nub = (ucap as usize) >> UB;
    let blk = 2usize << 20;
    let ev_arena = Arena::new(ev_bytes / 28 * 16 + nt * nb * 4096 + nt * blk + (16 << 20));
    let us_arena = Arena::new((ck_bytes / 10 + lk_bytes / 6) * 16 + nt * nub * 4096 + nt * blk + (16 << 20));
    let row_cap = ev_bytes / 40 / nt * 2 + 65536;
    let nd = ((we - ws + ws.rem_euclid(DAY_MS) + DAY_MS - 1) / DAY_MS) as usize;
    let mut t = Vec::with_capacity(nt);
    for _ in 0..nt {
        t.push(UnsafeCell::new(TState {
            ev: parse::Part::empty(),
            us: parse::Part::empty(),
            raws: Vec::new(),
            odd_users: Vec::new(),
            odd_events: Vec::new(),
            odd_clicks: Vec::new(),
            gen_edges: Vec::new(),
            cx_edges: Vec::new(),
            cx_users: Vec::new(),
            mixed_users: Vec::new(),
            slots: Vec::new(),
            edge: AVec::empty(),
            cx_raw: Vec::new(),
            cx_ev: (0..NSHARD).map(|_| Vec::new()).collect(),
            cx_ck: (0..NSHARD).map(|_| Vec::new()).collect(),
            ck: Vec::new(),
            runbuf: Vec::new(),
            pend: Vec::new(),
            cnt: Vec::new(),
            core: core::Core::new(0, 1, 16, 0, 0),
            n_ev_rows: 0,
            n_ck_rows: 0,
            n_lk_rows: 0,
            n_simple: 0,
            n_complex: 0,
            n_odd: 0,
            n_runs: 0,
            prof: [0; 8],
        }));
    }
    G {
        nt,
        check,
        ws,
        wlen: (we - ws) as u64,
        cbase: ws - 7 * DAY_MS,
        clen: (we - ws + 7 * DAY_MS) as u64,
        ing_base: ws - DAY_MS,
        ws_mod: ws.rem_euclid(DAY_MS) as u64,
        day0: ws.div_euclid(DAY_MS) as i32,
        nd,
        ukey,
        ekey,
        ecap,
        ucap,
        prod,
        camp,
        nc,
        cats,
        channels,
        region,
        files,
        queue,
        qnext: AtomicUsize::new(0),
        ev_bytes,
        row_cap,
        nb,
        nub,
        ev_arena,
        us_arena,
        t,
        utab: anon(ucap as usize * 2) as *mut u16,
        claim: anon(ucap as usize / 8 + 64) as *const AtomicU64,
        csr_off: anon((ucap as usize + nub + 8) * 4) as *mut u32,
        csr: std::ptr::null_mut(),
        csr_base: Vec::new(),
        ncsr: 0,
        uf: links::Uf::new(),
        edges: anon(nb * std::mem::size_of::<EdgeInfo>()) as *mut EdgeInfo,
        joined: Vec::new(),
        cxu: Vec::new(),
        extra: std::collections::HashMap::new(),
        odd_kept: Vec::new(),
        next: AtomicUsize::new(0),
        st2b_done: std::sync::atomic::AtomicBool::new(false),
    }
}
