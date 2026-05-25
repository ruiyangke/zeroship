//! Build script for `zeroship-sandbox`.
//!
//! T5: captures the controller's git commit at build time so the wake
//! state machine can verify the restored agent's `/version` endpoint
//! reports the SAME `git_commit` (Option A — agent and controller
//! deployed together). Mirrors `crates/sandbox-agent/build.rs`; both
//! `cargo:rustc-env` keys read 12 short-hex characters.
//!
//! When the build isn't in a git checkout the fallback value is the
//! literal `"unknown"` — the wake-path check treats that as a
//! signal-suppressed sentinel (skip the comparison with a WARN, not
//! fail every wake) so vendor-tarball builds remain usable.

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
    println!("cargo:rustc-env=CONTROLLER_GIT_COMMIT={git}");
    // Re-run when HEAD moves.
    println!("cargo:rerun-if-changed=../../.git/HEAD");
}
