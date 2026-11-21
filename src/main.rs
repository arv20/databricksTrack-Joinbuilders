mod attribution;
mod clicks;
mod events;
mod links;
mod output;
mod parse;
mod sessions;

use hashbrown::HashMap;
use output::Col;
use parse::{chunk_target, chunks, for_lines, map_dir, map_file, split, B};
use rayon::prelude::*;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

type Map = HashMap<B, u32, foldhash::fast::FixedState>;

const ES: usize = 256; // event dedup shards
const PS: usize = 256; // person shards

fn new_map(n: usize) -> Map {
    HashMap::with_capacity_and_hasher(n, foldhash::fast::FixedState::with_seed(7))
}

fn main() {
    let t0 = Instant::now();
    let mut last = t0;
    let mut lap = |what: &str| {
        let now = Instant::now();
        let rss_mb = std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1).and_then(|x| x.parse::<u64>().ok()))
            .map_or(0, |p| p * 4096 / (1 << 20));
        eprintln!(
            "[v1] {what:<14} {:>8.1} ms  (total {:>8.1} ms)  rss {rss_mb} MB",
            (now - last).as_secs_f64() * 1e3,
            (now - t0).as_secs_f64() * 1e3
        );
        last = now;
    };
    let args: Vec<String> = std::env::args().collect();
    let (data, out) = if args.len() >= 3 {
        (PathBuf::from(&args[args.len() - 2]), PathBuf::from(&args[args.len() - 1]))
    } else {
        (
            PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "/data".into())),
            PathBuf::from(std::env::var("OUTPUT_DIR").unwrap_or_else(|_| "/output".into())),
        )
    };
    let ncpu: usize = std::env::var("NCPU").ok().and_then(|s| s.trim().parse().ok()).filter(|&n| n > 0).unwrap_or(8);
    rayon::ThreadPoolBuilder::new().num_threads(ncpu).build_global().unwrap();

    // Small files.
    let (ws, we) = parse::manifest(map_file(&data.join("manifest.json")));
    let prod_file = map_file(&data.join("products.csv"));
    let mut prod_rows: Vec<(B, B)> = Vec::new();
    for c in chunks(&[prod_file], usize::MAX / 2) {
        for_lines(c, |l| {
            if let Some([p, cat]) = split::<2>(l) {
                prod_rows.push((p, cat));
            }
        });
    }
    let mut cat_names: Vec<B> = prod_rows.iter().map(|r| r.1).collect();
    cat_names.push(b"unknown");
    cat_names.sort_unstable();
    cat_names.dedup();
    let mut cat_rank = new_map(cat_names.len());
    for (i, &c) in cat_names.iter().enumerate() {
        cat_rank.insert(c, i as u32);
    }
    let mut products = new_map(prod_rows.len());
    for &(p, c) in &prod_rows {
        products.entry(p).or_insert(cat_rank[c]);
    }
    let unknown_cat = cat_rank[&b"unknown"[..]];
    let camp_file = map_file(&data.join("campaigns.csv"));
    let mut camp_ids: Vec<B> = Vec::new();
    let mut channels: Vec<B> = Vec::new();
    let mut campaigns = new_map(1024);
    for c in chunks(&[camp_file], usize::MAX / 2) {
        for_lines(c, |l| {
            if let Some([id, ch]) = split::<2>(l) {
                if !campaigns.contains_key(id) {
                    campaigns.insert(id, camp_ids.len() as u32);
                    camp_ids.push(id);
                    channels.push(ch);
                }
            }
        });
    }
    let organic = camp_ids.len() as u32;
    camp_ids.push(b"organic");
    channels.push(b"organic");

    let ev_files = map_dir(&data.join("events"));
    let ck_files = map_dir(&data.join("clicks"));
    let lk_files = map_dir(&data.join("links"));
    let ev_chunks = chunks(&ev_files, chunk_target(&ev_files, ncpu));
    let ck_chunks = chunks(&ck_files, chunk_target(&ck_files, ncpu));
    let lk_chunks = chunks(&lk_files, chunk_target(&lk_files, ncpu));
    lap("setup");

    let ectx = events::EvCtx { ws, we, products: &products, unknown_cat };
    let (links, (ev_parts, ck_parts)) = rayon::join(
        || links::build(&lk_chunks),
        || {
            rayon::join(
                || ev_chunks.par_iter().map(|&c| events::parse_chunk(c, &ectx, ES)).collect::<Vec<_>>(),
                || ck_chunks.par_iter().map(|&c| clicks::parse_chunk(c, ws, we, &campaigns)).collect::<Vec<_>>(),
            )
        },
    );
    lap("parse+links");
    let n_copies: usize = ev_parts.iter().map(|p| p.iter().map(|v| v.len()).sum::<usize>()).sum();

    let (recs, ckr) = rayon::join(
        || (0..ES).into_par_iter().map(|s| events::dedup_route(&ev_parts, s, &links, PS)).collect::<Vec<_>>(),
        || ck_parts.par_iter().map(|v| clicks::route(v, &links, PS)).collect::<Vec<_>>(),
    );
    let n_kept: usize = recs.iter().map(|r| r.1).sum();
    let n_clicks: usize = ck_parts.iter().map(|v| v.len()).sum();
    // Free the parsed copies before the person stage allocates (keeps peak memory down on 60M).
    ev_parts.into_par_iter().for_each(drop);
    drop(ck_parts);
    lap("dedup+route");

    let nthreads = rayon::current_num_threads();
    let accs: Vec<Mutex<sessions::Acc>> =
        (0..nthreads).map(|_| Mutex::new(sessions::Acc::with_hasher(foldhash::fast::FixedState::with_seed(3)))).collect();
    let ncat = cat_names.len();
    let rows: Vec<Vec<sessions::UserRow>> = (0..PS)
        .into_par_iter()
        .map(|p| {
            let mut acc = accs[rayon::current_thread_index().unwrap_or(0)].lock().unwrap();
            sessions::run_shard(p, &recs, &ckr, ncat, organic, &mut acc)
        })
        .collect();
    let mut acc = sessions::Acc::with_hasher(foldhash::fast::FixedState::with_seed(3));
    for a in accs {
        for (k, v) in a.into_inner().unwrap() {
            let e = acc.entry(k).or_insert([0; 4]);
            for i in 0..4 {
                e[i] += v[i];
            }
        }
    }
    std::mem::forget(recs);
    std::mem::forget(ckr);
    lap("sessions");

    let ud = out.join("user_daily");
    let cd = out.join("campaign_daily");
    std::fs::create_dir_all(&ud).expect("create user_daily");
    std::fs::create_dir_all(&cd).expect("create campaign_daily");
    let nparts = ncpu.min(PS).max(1);
    let per = PS.div_ceil(nparts);
    let n_rows: usize = rows.iter().map(|r| r.len()).sum();
    let res: Vec<Result<(), String>> = (0..nparts + 1)
        .into_par_iter()
        .map(|k| {
            if k == nparts {
                let mut cid = Vec::new();
                let mut ch = Vec::new();
                let mut day = Vec::new();
                let mut s = Vec::new();
                let mut ps = Vec::new();
                let mut rev = Vec::new();
                let mut asst = Vec::new();
                for (&key, v) in &acc {
                    let (c, d) = sessions::unkey(key);
                    cid.push(camp_ids[c as usize]);
                    ch.push(channels[c as usize]);
                    day.push(d);
                    s.push(v[0] as i64);
                    ps.push(v[1] as i64);
                    rev.push(v[2] as i64);
                    asst.push(v[3] as i64);
                }
                return output::write_table(
                    &cd.join("part-0.parquet"),
                    &[
                        ("campaign_id", Col::Str(cid)),
                        ("channel", Col::Str(ch)),
                        ("day", Col::Date(day)),
                        ("sessions", Col::I64(s)),
                        ("purchasing_sessions", Col::I64(ps)),
                        ("revenue_cents", Col::I64(rev)),
                        ("assisted_sessions", Col::I64(asst)),
                    ],
                );
            }
            let group = &rows[(k * per).min(PS)..((k + 1) * per).min(PS)];
            let n: usize = group.iter().map(|g| g.len()).sum();
            let mut uid = Vec::with_capacity(n);
            let mut day = Vec::with_capacity(n);
            let mut s = Vec::with_capacity(n);
            let mut ev = Vec::with_capacity(n);
            let mut rev = Vec::with_capacity(n);
            let mut top = Vec::with_capacity(n);
            for g in group {
                for r in g {
                    uid.push(r.name);
                    day.push(r.day);
                    s.push(r.sessions as i64);
                    ev.push(r.events as i64);
                    rev.push(r.revenue as i64);
                    top.push(r.top.map(|c| cat_names[c as usize]));
                }
            }
            output::write_table(
                &ud.join(format!("part-{k}.parquet")),
                &[
                    ("user_id", Col::Str(uid)),
                    ("day", Col::Date(day)),
                    ("sessions", Col::I64(s)),
                    ("events", Col::I64(ev)),
                    ("revenue_cents", Col::I64(rev)),
                    ("top_category", Col::OptStr(top)),
                ],
            )
        })
        .collect();
    for r in res {
        if let Err(e) = r {
            eprintln!("[v1] write error: {e}");
            std::process::exit(1);
        }
    }
    lap("write");
    eprintln!(
        "[v1] copies={n_copies} kept_events={n_kept} valid_clicks={n_clicks} link_ids={} user_rows={n_rows} campaign_rows={}",
        links.len(),
        acc.len()
    );
    std::process::exit(0);
}
