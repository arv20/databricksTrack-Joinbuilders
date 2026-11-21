use crate::pqw::*;
use crate::*;
use std::path::Path;

fn die(what: &str) -> ! {
    eprintln!("[v2b] write error: {what}");
    unsafe { sys::_exit(1) }
}

fn write_user(g: &G, tid: usize, dir: &Path, cats_off: &[u32], cats_data: &[u8]) {
    let c = &g.t(tid).core;
    if c.day.len() == 0 && tid != 0 {
        return;
    }
    let no: &[u32] = if c.day.len() == 0 { &[] } else { c.names_off.as_slice() };
    let cols = [
        Column { name: "user_id", data: Data::Dict { dict: StrCol { offsets: no, data: c.names.as_slice() }, keys: Keys::U32(c.keys.as_slice()), nullable: false, plain: false } },
        Column { name: "day", data: Data::Date(c.day.as_slice(), IntEnc::Dense) },
        Column { name: "sessions", data: Data::I32(c.ses.as_slice(), IntEnc::Dense) },
        Column { name: "events", data: Data::I32(c.evs.as_slice(), IntEnc::Auto) },
        Column { name: "revenue_cents", data: Data::I32(c.rev.as_slice(), IntEnc::Plain) },
        Column { name: "top_category", data: Data::Dict { dict: StrCol { offsets: cats_off, data: cats_data }, keys: Keys::U8(c.cat.as_slice()), nullable: true, plain: false } },
    ];
    if write_file(&dir.join(format!("part-{tid}.parquet")), &cols, &Opts::default(), Method::Stream(1 << 20)).is_err() {
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
                id_data.extend_from_slice(format!("c_{cn}").as_bytes());
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
    let gr: &G = g;
    sys::run_par(gr.nt, |tid| {
        if tid == 0 {
            write_campaign(gr, &cd);
        }
        write_user(gr, tid, &ud, &cats_off, &cats_data);
    });
}
