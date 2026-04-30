//! Build script for `zeroship-sandbox-agent`.
//!
//! Captures the git commit at build time so the running binary can
//! report it via `/version`. Used by the controller to verify which
//! agent version is in a pod and gate features accordingly.

fn main() {
    let git = std::process::Command::new("git")
        .args(["rev-parse", "--short=12", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=AGENT_GIT_COMMIT={git}");
    // Re-run when HEAD moves.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
