//! The artifacts in `schema/` are compiler output, and this is what keeps them
//! that way: it re-runs the generator in check mode and fails on any drift.
//!
//! It lives beside the artifacts rather than in a consumer, so the crate that
//! owns them is the crate that proves they are current.

use std::path::Path;
use std::process::Command;

#[test]
fn generated_artifacts_match_the_authored_schema() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let generator = "crates/zeroship-workflow-schema/schema/generate.mjs";
    assert!(
        root.join(generator).is_file(),
        "the generator is not where this test looks for it: {generator}"
    );
    let output = Command::new("node")
        .arg(generator)
        .arg("--check")
        .current_dir(root)
        .output()
        .expect("run the workflow schema generator");
    assert!(
        output.status.success(),
        "schema compiler check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
