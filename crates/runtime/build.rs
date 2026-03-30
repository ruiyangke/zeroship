fn main() {
    // Snapshot generation requires the "snapshot" feature
    // When enabled, we need deno_core as a build dependency
    // For now, snapshot is opt-in and build.rs is a no-op without the feature
    println!("cargo:rerun-if-changed=src/embed/runtime.js");
}
