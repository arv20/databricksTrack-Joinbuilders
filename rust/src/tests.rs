use super::*;

// Run in a child process: DATA_DIR/OUTPUT_DIR and Rayon are process-global.
#[test]
fn colliding_ids_never_silently_merge() {
    for case in ["events", "unlinked", "links", "lookup", "clicks"] {
        let result = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "tests::collision_child", "--nocapture"])
            .env("COLLISION_CASE", case)
            .output().unwrap();
        assert!(result.status.success(), "{case}: {}", String::from_utf8_lossy(&result.stdout));
    }
}

#[test]
fn collision_child() {
    let Ok(case) = std::env::var("COLLISION_CASE") else { return };
    let root = std::env::temp_dir().join(format!("sessions-collision-{}", std::process::id()));
    let input = root.join("input");
    let output = root.join("output");
    for dir in ["events", "clicks", "links"] { std::fs::create_dir_all(input.join(dir)).unwrap(); }
    std::fs::write(input.join("manifest.json"), r#"{"window_start":"2026-10-01T00:00:00Z","window_end":"2026-10-15T00:00:00Z"}"#).unwrap();
    std::fs::write(input.join("products.csv"), "product_id,category\np1,books\n").unwrap();
    std::fs::write(input.join("campaigns.csv"), "campaign_id,channel\nc1,search\n").unwrap();
    let (users, links, click, collision_ids): (&[&str], &str, &str, &[&[u8]]) = match case.as_str() {
        "events" => (&["u1", "u1"], "", "", &[b"e0", b"e1"]),
        "unlinked" => (&["u1", "u2"], "", "", &[b"u1", b"u2"]),
        "links" => (&["u1", "u2"], "u1,u2\n", "", &[b"u1", b"u2"]),
        "lookup" => (&["u1", "u2"], "u1,z\n", "", &[b"u1", b"u2"]),
        "clicks" => (&["u1"], "", "k1,u2,c1,2026-10-03T09:00:00Z\n", &[b"u1", b"u2"]),
        _ => unreachable!(),
    };
    let mut events = "event_id,user_id,ts,ingest_ts,event_type,product_id,price_cents\n".to_owned();
    for (i, u) in users.iter().enumerate() {
        events += &format!("e{i},{u},2026-10-03T10:00:00Z,2026-10-03T10:01:00Z,view,p1,\n");
    }
    std::fs::write(input.join("events/part.csv"), events).unwrap();
    std::fs::write(input.join("links/part.csv"), format!("user_id_a,user_id_b\n{links}")).unwrap();
    std::fs::write(input.join("clicks/part.csv"), format!("click_id,user_id,campaign_id,ts\n{click}")).unwrap();
    // No threads exist yet in this child; environment is set before Rayon starts.
    unsafe {
        std::env::set_var("DATA_DIR", &input);
        std::env::set_var("OUTPUT_DIR", &output);
        std::env::set_var("NCPU", "2");
    }
    let accepted = fast(123, |b, seed| {
        if collision_ids.contains(&b) { (12345, 6789) } else { hs(b, seed) }
    });
    let _ = std::fs::remove_dir_all(&root);
    assert!(!accepted, "different {case} IDs with identical hashes must trigger the exact fallback");
}

#[test]
fn exact_ids_preserve_lengths_zero_bytes_and_long_values() {
    let values: &[&[u8]] = &[
        b"", b"a", b"a\0", b"\0a", b"123456789012345", b"1234567890123456",
        b"1234567890123456x", b"1234567890123456y", "shopper-\u{00e9}-\u{65e5}\u{672c}".as_bytes(),
    ];
    for (i, a) in values.iter().enumerate() {
        let copied = a.to_vec(); // distinct addresses with identical bytes must compare equal
        assert!(Id::new(a) == Id::new(&copied));
        assert_eq!(Id::new(a).bytes(), *a);
        for (j, b) in values.iter().enumerate() {
            assert_eq!(Id::new(a) == Id::new(b), i == j);
        }
    }
}
