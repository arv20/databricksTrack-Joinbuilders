use crate::pqw::*;
use crate::*;
use std::path::Path;

fn die(what: &str) -> ! {
    eprintln!("[v2b] write error: {what}");
    unsafe { sys::_exit(1) }
}

fn write_piece(g: &G, t: usize, a: usize, b: usize, path: &Path, cats_off: &[u32], cats_data: &[u8], kbuf: &mut Vec<u32>) {
    let c = &g.t(t).core;
    let keys = &c.keys.as_slice()[a..b];
    let (mut lo, mut hi) = (u32::MAX, 0u32);
    for &k in keys {
        lo = lo.min(k);
        hi = hi.max(k);
    }
    kbuf.clear();
    kbuf.extend(keys.iter().map(|&k| k - lo));
    let no: &[u32] = if a == b { &[] } else { &c.names_off.as_slice()[lo as usize..hi as usize + 2] };
    let cols = [
        Column { name: "user_id", data: Data::Dict { dict: StrCol { offsets: no, data: c.names.as_slice() }, keys: Keys::U32(kbuf), nullable: false, plain: false } },
        Column { name: "day", data: Data::Date(&c.day.as_slice()[a..b], IntEnc::Dense) },
        Column { name: "sessions", data: Data::I32(&c.ses.as_slice()[a..b], IntEnc::Dense) },
        Column { name: "events", data: Data::I32(&c.evs.as_slice()[a..b], IntEnc::Auto) },
        Column { name: "revenue_cents", data: Data::I32(&c.rev.as_slice()[a..b], IntEnc::Plain) },
        Column { name: "top_category", data: Data::Dict { dict: StrCol { offsets: cats_off, data: cats_data }, keys: Keys::U8(&c.cat.as_slice()[a..b]), nullable: true, plain: false } },
    ];
    if write_file(path, &cols, &Opts::default(), Method::Stream(1 << 20)).is_err() {
        die("user_daily");
    }
}

fn write_campaign(g: &G, dir: &Path) {
    let nd = g.nd;
    let n = (g.nc + 1) * nd;
    let mut agg = vec![[0u64; 4]; n];
    for tid in 0..g.nt {
        let c = &g.t(tid).core;
        for i in 0..n {
            agg[i][0] += c.agg[i][0] as u64;
            agg[i][3] += c.agg[i][1] as u64;
            agg[i][1] += c.aggp[i][0];
            agg[i][2] += c.aggp[i][1];
        }
    }
    let (mut id_off, mut id_data, mut ch_off, mut ch_data) = (vec![0u32], Vec::new(), vec![0u32], Vec::new());
    let mut day = Vec::new();
    let mut v: [Vec<i64>; 4] = Default::default();
    for cn in 0..=g.nc {
        for d in 0..nd {
            let a = agg[cn * nd + d];
            if a[0] == 0 && a[3] == 0 {
                continue;
            }
            if cn == g.nc {
                id_data.extend_from_slice(b"organic");
                ch_data.extend_from_slice(b"organic");
            } else {
                let mut d = [0u8; 24];
                let (mut i, mut x) = (d.len(), cn);
                loop {
                    i -= 1;
                    d[i] = b'0' + (x % 10) as u8;
                    x /= 10;
                    if x == 0 {
                        break;
                    }
                }
                i -= 2;
                d[i] = b'c';
                d[i + 1] = b'_';
                id_data.extend_from_slice(&d[i..]);
                ch_data.extend_from_slice(&g.channels[g.camp[cn] as usize]);
            }
            id_off.push(id_data.len() as u32);
            ch_off.push(ch_data.len() as u32);
            day.push(g.day0 + d as i32);
            for k in 0..4 {
                v[k].push(a[k] as i64);
            }
        }
    }
    let empty: &[u32] = &[];
    let rows = day.len();
    let cols = [
        Column { name: "campaign_id", data: Data::Str(StrCol { offsets: if rows == 0 { empty } else { &id_off }, data: &id_data }) },
        Column { name: "channel", data: Data::Str(StrCol { offsets: if rows == 0 { empty } else { &ch_off }, data: &ch_data }) },
        Column { name: "day", data: Data::Date(&day, IntEnc::Plain) },
        Column { name: "sessions", data: Data::I64(&v[0], IntEnc::Plain) },
        Column { name: "purchasing_sessions", data: Data::I64(&v[1], IntEnc::Plain) },
        Column { name: "revenue_cents", data: Data::I64(&v[2], IntEnc::Plain) },
        Column { name: "assisted_sessions", data: Data::I64(&v[3], IntEnc::Plain) },
    ];
    if write_file(&dir.join("part-0.parquet"), &cols, &Opts::default(), Method::Whole).is_err() {
        die("campaign_daily");
    }
    if g.check {
        let (mut us, mut ur) = (0i64, 0i64);
        for tid in 0..g.nt {
            let c = &g.t(tid).core;
            us += c.ses.as_slice().iter().map(|&x| x as i64).sum::<i64>();
            ur += c.rev.as_slice().iter().map(|&x| x as i64).sum::<i64>();
        }
        let cs: i64 = v[0].iter().sum();
        let cr: i64 = v[2].iter().sum();
        let ca: i64 = v[3].iter().sum();
        let organic: i64 = (0..nd).map(|d| agg[g.nc * nd + d][0] as i64).sum();
        eprintln!("[v2b] check: sessions user={us} campaign={cs} revenue user={ur} campaign={cr} assisted={ca} credited_non_organic={}", cs - organic);
        if us != cs || ur != cr || ca < cs - organic {
            eprintln!("[v2b] CHECK FAILED");
            unsafe { sys::_exit(5) }
        }
    }
}

pub fn stage5(g: &mut G, outdir: &str) {
    let ud = Path::new(outdir).join("user_daily");
    let cd = Path::new(outdir).join("campaign_daily");
    if std::fs::create_dir_all(&ud).is_err() || std::fs::create_dir_all(&cd).is_err() {
        die("cannot create the output folders");
    }
    let mut cats_off = vec![0u32];
    let mut cats_data = Vec::new();
    for c in &g.cats {
        cats_data.extend_from_slice(c);
        cats_off.push(cats_data.len() as u32);
    }
    let nt = g.nt;
    let mut base = vec![0usize; nt + 1];
    for t in 0..nt {
        base[t + 1] = base[t] + g.t(t).core.day.len();
    }
    let total = base[nt];
    let np = (2 * nt).min(total).max(1);
    let mut pieces: Vec<(usize, usize, usize)> = Vec::new();
    for p in 0..np {
        let (ga, gb) = (total * p / np, total * (p + 1) / np);
        for t in 0..nt {
            let (a, b) = (ga.max(base[t]), gb.min(base[t + 1]));
            if a < b {
                pieces.push((t, a - base[t], b - base[t]));
            }
        }
    }
    if pieces.is_empty() {
        pieces.push((0, 0, 0));
    }
    let gr: &G = g;
    let next = std::sync::atomic::AtomicUsize::new(0);
    let (pr, nx) = (&pieces, &next);
    sys::run_par(nt, |_| {
        let mut kbuf = Vec::new();
        loop {
            let i = nx.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if i == 0 {
                write_campaign(gr, &cd);
            } else if i <= pr.len() {
                let (t, a, b) = pr[i - 1];
                write_piece(gr, t, a, b, &ud.join(format!("part-{}.parquet", i - 1)), &cats_off, &cats_data, &mut kbuf);
            } else {
                break;
            }
        }
    });
}
