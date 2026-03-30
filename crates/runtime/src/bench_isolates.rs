use appbase_runtime::v8::create_v8_runtime;

fn rss_kb() -> usize {
    let status = std::fs::read_to_string("/proc/self/status").unwrap();
    for line in status.lines() {
        if line.starts_with("VmRSS:") {
            return line.split_whitespace().nth(1).unwrap().parse().unwrap();
        }
    }
    0
}

fn main() {
    let base = rss_kb();
    eprintln!("Base RSS: {} KB ({:.1} MB)", base, base as f64 / 1024.0);

    let mut runtimes = Vec::new();
    for i in 1..=10 {
        let db_path = format!("/tmp/appbase-iso-{}-{}.db", std::process::id(), i);
        let (runtime, _rpc) = create_v8_runtime(&db_path).unwrap();
        runtimes.push((runtime, db_path.clone()));

        let current = rss_kb();
        let total_added = current - base;
        let per_isolate = total_added / i;
        eprintln!(
            "Isolate {}: RSS {} KB ({:.1} MB) | +{} KB total | ~{} KB per isolate ({:.1} MB)",
            i, current, current as f64 / 1024.0,
            total_added, per_isolate, per_isolate as f64 / 1024.0
        );
    }

    // Cleanup
    for (_, path) in &runtimes {
        let _ = std::fs::remove_file(path);
    }
}
