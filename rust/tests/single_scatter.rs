use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn event_source_count_over_256_declines_fast_engine() {
    let nonce = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    let root = std::env::temp_dir().join(format!("rproto-source-count-{}-{nonce}", std::process::id()));
    fs::create_dir(&root).unwrap();
    let root = TempDir(root);
    let data = root.0.join("data");
    let events = data.join("events");
    fs::create_dir_all(&events).unwrap();
    fs::write(
        data.join("manifest.json"),
        r#"{"window_start":"2026-01-01T00:00:00Z","window_end":"2026-01-15T00:00:00Z"}"#,
    ).unwrap();
    fs::write(data.join("products.csv"), "product_id,category\n").unwrap();
    fs::write(data.join("campaigns.csv"), "campaign_id,channel\n").unwrap();
    let header = "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n";
    for i in 0..=256 {
        fs::write(events.join(format!("{i:03}.csv")), header).unwrap();
    }
    fs::write(
        events.join("000.csv"),
        format!("{header}e,u,2026-01-01T00:00:00Z,2026-01-01T00:00:00Z,view,p,\n"),
    ).unwrap();

    let run = |out: &str| {
        let result = Command::new(env!("CARGO_BIN_EXE_rproto"))
            .env("DATA_DIR", &data)
            .env("OUTPUT_DIR", root.0.join(out))
            .env("NCPU", "2")
            .env("TIMING", "1")
            .env_remove("FORCE_SLOW")
            .output().unwrap();
        let stderr = String::from_utf8_lossy(&result.stderr);
        assert!(result.status.success(), "{stderr}");
        assert!(root.0.join(out).join("user_daily").is_dir());
        stderr.contains("fast engine declined")
    };
    assert!(run("slow"));
    fs::remove_file(events.join("256.csv")).unwrap();
    assert!(!run("fast"));
}
