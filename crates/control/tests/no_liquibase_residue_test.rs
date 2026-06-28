//! Regression guard for T1: the control crate is migrated by `zeroship-migrate`
//! (`db/migrations`, platform profile), NOT Liquibase. The schema-authority
//! convergence moved the platform DB (control/auth/billing/oauth_hydra) off
//! Liquibase onto zeroship-migrate, but doc-comments and test/bench setup prose
//! still pointed operators at the dead `docker run liquibase ... -v db/changelog`
//! path. This test fails if that operational residue creeps back into the
//! control crate's `src/`, `tests/`, or `benches/`.
//!
//! It is deliberately precise: it flags only OPERATIONAL Liquibase references
//! (the literal `liquibase` token, case-insensitive, and the dead `db/changelog`
//! path). It does NOT flag the Stripe API "changelog" mentions in
//! `pricing.rs` / `stripe_client.rs` — those are about Stripe's versioning
//! changelog, not our migration tool, and the bare word `changelog` is allowed.

use std::fs;
use std::path::{Path, PathBuf};

/// Walk a directory tree collecting `.rs` files.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_rs(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            out.push(path);
        }
    }
}

#[test]
fn control_crate_has_no_operational_liquibase_residue() {
    // CARGO_MANIFEST_DIR is `crates/control` at test build time.
    let crate_root = Path::new(env!("CARGO_MANIFEST_DIR"));

    let mut files = Vec::new();
    for sub in ["src", "tests", "benches"] {
        collect_rs(&crate_root.join(sub), &mut files);
    }
    // Don't grep this guard test itself (it necessarily names the residue).
    let self_path = crate_root.join("tests/no_liquibase_residue_test.rs");

    let mut offenders: Vec<String> = Vec::new();
    for file in &files {
        if file == &self_path {
            continue;
        }
        let Ok(text) = fs::read_to_string(file) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            let lower = line.to_ascii_lowercase();
            // Operational Liquibase references: the tool name, or the dead
            // `db/changelog` migration path that no longer exists.
            if lower.contains("liquibase") || lower.contains("db/changelog") {
                offenders.push(format!(
                    "{}:{}: {}",
                    file.strip_prefix(crate_root).unwrap_or(file).display(),
                    i + 1,
                    line.trim()
                ));
            }
        }
    }

    assert!(
        offenders.is_empty(),
        "control crate still has operational Liquibase residue (the platform DB \
         is migrated by `zeroship-migrate migrate --dir db/migrations --profile \
         platform`, not Liquibase). Offending lines:\n{}",
        offenders.join("\n")
    );
}
