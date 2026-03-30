use appbase_runtime::v8::create_snapshot;

fn main() {
    let output = std::env::args().nth(1).unwrap_or_else(|| "snapshot.bin".to_string());

    eprintln!("[snapshot] Creating V8 snapshot...");
    let snapshot = create_snapshot();
    eprintln!("[snapshot] Snapshot size: {} bytes", snapshot.len());

    std::fs::write(&output, &*snapshot).expect("Failed to write snapshot");
    eprintln!("[snapshot] Written to {output}");
}
