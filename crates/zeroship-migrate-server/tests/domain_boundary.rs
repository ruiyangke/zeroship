//! The migration service owns privileged DDL against a creator database. It does
//! not own anyone else's domain, and this is what keeps that true.
//!
//! # Why a source scan, when the house rule is to test behavior
//!
//! Because the property IS about the source. "This crate contains no workflow
//! knowledge" cannot be observed by running it: a service that grew a
//! workflow-shaped branch would still pass every behavioral test, exactly as it
//! did while `provision_workflow_app` lived here. The thing to detect is a
//! re-introduced dependency on another domain's vocabulary, and the only place
//! that is visible is the text.
//!
//! It is scoped to make that cheap and honest: the creator apply path's own
//! tests may name any domain they like, because a fixture describing a workflow
//! app is not the service knowing what a workflow is.

use std::fs;
use std::path::{Path, PathBuf};

/// The domains this service must not learn. One entry today; the list is the
/// point, because the second platform-owned schema is what would tempt someone
/// to add a second branch here.
const FOREIGN_DOMAIN_TERMS: &[&str] = &["workflow"];

#[test]
fn the_migration_service_carries_no_foreign_domain_vocabulary() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let files = rust_sources(&src);
    assert!(
        files.len() > 5,
        "the source walk found almost nothing, so this gate proves nothing: {files:?}"
    );

    let mut offenders = Vec::new();
    for file in &files {
        let text = fs::read_to_string(file).expect("read a service source file");
        for (number, line) in text.lines().enumerate() {
            for term in FOREIGN_DOMAIN_TERMS {
                if line.to_ascii_lowercase().contains(term) {
                    offenders.push(format!(
                        "{}:{}: {}",
                        file.strip_prefix(&src).unwrap().display(),
                        number + 1,
                        line.trim()
                    ));
                }
            }
        }
    }
    assert!(
        offenders.is_empty(),
        "the migration service names a foreign domain. Move the knowledge into that \
         domain's own crate and send it as a schema bundle:\n{}",
        offenders.join("\n")
    );
}

/// The control for the gate above: it has to be able to SEE a violation, or an
/// empty result means nothing. Same walk, same matcher, one term changed to one
/// this service legitimately uses everywhere.
#[test]
fn the_boundary_scan_would_notice_a_term_the_service_does_use() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let hits: usize = rust_sources(&src)
        .iter()
        .map(|file| {
            fs::read_to_string(file)
                .expect("read a service source file")
                .lines()
                .filter(|line| line.to_ascii_lowercase().contains("migration"))
                .count()
        })
        .sum();
    assert!(
        hits > 0,
        "the scanner matched nothing for a term this service certainly uses, so its empty \
         verdict on the foreign-domain terms is not evidence"
    );
}

fn rust_sources(root: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).expect("read a service source directory") {
            let path = entry.expect("read a directory entry").path();
            if path.is_dir() {
                pending.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}
