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
    let mut names: Vec<&[u8]> = Vec::new();
    let mut prod = vec![255u8; PBOUND + 1];
    let mut bad = false;
    data_lines(&ptxt, |l| {
        let mut ok = false;
        let (mut n, mut i) = (0usize, 1usize);
        if l.len() >= 4 && l[0] == b'p' {
            while i < l.len() && i < 9 && l[i].is_ascii_digit() {
                n = n * 10 + (l[i] - b'0') as usize;
                i += 1;
            }
            ok = i > 1 && i < l.len() && l[i] == b',' && !(l[1] == b'0' && i > 2) && i + 1 < l.len() && !l[i + 1..].iter().any(|&c| c == b',' || c == b'"');
        }
        let c: &[u8] = if ok {
            &l[i + 1..]
        } else {
            match split2(l) {
                Some((p, c)) if !c.is_empty() => {
                    n = k_ref::parse_product(p) as usize;
                    c
                }
                _ => {
                    bad = true;
                    return;
                }
            }
        };
        if n >= PBOUND || prod[n] != 255 {
            bad = true;
            return;
        }
        let ci = match names.iter().position(|x| *x == c) {
            Some(k) => k,
            None => {
                names.push(c);
                names.len() - 1
            }
        };
        if ci >= 31 {
            bad = true;
            return;
        }
        prod[n] = ci as u8;
    });
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
    let lk_handle = { let d = data.to_path_buf(); std::thread::spawn(move || lookups(&d)) };
    let mut all: Vec<(Kind, std::path::PathBuf, usize)> = Vec::new();
    for (kind, dir) in [(Kind::Ev, "events"), (Kind::Ck, "clicks"), (Kind::Lk, "links")] {
        for (p, len) in list(&data.join(dir)) {
            if len > 0 {
                all.push((kind, p, len));
            }
        }
    }
    all.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.1.cmp(&b.1)));
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
        let bytes = unsafe { std::slice::from_raw_parts(region.add(off), *len) };
        let probe = &bytes[..bytes.len().min(4096)];
        let hdr = match probe.iter().position(|&b| b == b'\n') {
            Some(i) => i + 1,
            None => bytes.iter().position(|&b| b == b'\n').map_or(*len, |i| i + 1),
        };
        match kind {
            Kind::Ev => ev_bytes += len,
            Kind::Ck => ck_bytes += len,
            Kind::Lk => lk_bytes += len,
        }
        files.push(FileInfo { kind: *kind, off, len: *len, hdr });
        off += ((len + pg - 1) & !(pg - 1)) + pg;
    }
    let total_bytes = ev_bytes + ck_bytes + lk_bytes;
    let ch = (total_bytes / (nt * 16)).clamp(64 << 10, 2 << 20);
    let mut queue = Vec::new();
    for (i, f) in files.iter().enumerate() {
        let mut a = f.hdr;
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
        let bytes = unsafe { std::slice::from_raw_parts(region.add(f.off + f.hdr), f.len - f.hdr) };
        let mut cut = bytes.len().min(600_000);
        if cut < bytes.len() {
            while cut > 0 && bytes[cut - 1] != b'\n' {
                cut -= 1;
            }
        }
        sample.add_event_lines(&bytes[..cut], 5000);
    }
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
