//! The WIRE-FORMAT guard for `Checksum::of_ir` across the whole op corpus.
//!
//! `Checksum::of_ir` folds each op's RFC 8785 (JCS) canonical bytes, and JCS
//! sorts and EMITS object key names. Field names are therefore inside the
//! identity checksum of every already-deployed migration: any serde rename, any
//! added-without-`skip_serializing_if` field, any change to how a dialect is
//! spelled on the wire silently invalidates deployed history and breaks
//! drift/tamper detection against journals that are already in production.
//!
//! `tests/ir_checksum.rs` pins ONE golden over three hand-picked ops. That is a
//! spot check. This file pins the checksum of EVERY `(op-kind, variant)` row of
//! the shared dialect corpus — the same corpus `dialect_table_faithfulness.rs`
//! and `dialect_conformance_live.rs` drive — so a rename anywhere in the op
//! surface is caught, not only a rename inside `createTable`/`insert`/`pgRaw`.
//!
//! Two assertions, deliberately:
//!
//! 1. The corpus is non-empty and each row's checksum is recomputed
//!    deterministically (a corpus that silently emptied would "pass" vacuously).
//! 2. The AGGREGATE digest over `kind|variant|checksum` for every row, in corpus
//!    order, equals a frozen constant.
//!
//! Set `ZM_CHECKSUM_CORPUS_DUMP=<path>` to write the full per-row listing, which
//! is what a before/after comparison across a refactor diffs.

use crate::dialect_corpus;

use sha2::{Digest, Sha256};
use zero_migrate::model::ir::CanonicalOpList;
use zero_migrate::{Checksum, MigrationFlags};

/// Frozen, dialect-neutral flags. `of_ir` takes no dialect parameter; these are
/// the derived-then-overridden neutral flags its contract requires.
fn frozen_flags() -> MigrationFlags {
    MigrationFlags {
        transactional: true,
        destructive: false,
        online: false,
        requires_approval: false,
        timeout_ms: None,
        lock_timeout_ms: None,
        phase: None,
        repeatable: false,
        engine_goodie_ddl: false,
    }
}

/// `(kind, variant, checksum-hex)` for every corpus row, in corpus order.
fn corpus_checksums() -> Vec<(&'static str, &'static str, String)> {
    let flags = frozen_flags();
    dialect_corpus::corpus()
        .into_iter()
        .map(|(kind, variant, op)| {
            let ops = vec![op];
            let checksum = Checksum::of_ir(
                &CanonicalOpList(&ops),
                &flags,
                "app_checksum_corpus",
                &[],
                &[],
                &[],
            );
            (kind, variant, checksum.as_str().to_string())
        })
        .collect()
}

#[test]
fn every_corpus_op_has_a_deterministic_checksum() {
    let first = corpus_checksums();
    assert!(
        first.len() >= 92,
        "the dialect corpus must not silently shrink: {} rows",
        first.len()
    );
    let second = corpus_checksums();
    assert_eq!(
        first, second,
        "Checksum::of_ir must be deterministic over the same op"
    );
}

#[test]
fn corpus_checksums_are_byte_stable() {
    let rows = corpus_checksums();

    let mut listing = String::new();
    for (kind, variant, hex) in &rows {
        listing.push_str(kind);
        listing.push('|');
        listing.push_str(variant);
        listing.push('|');
        listing.push_str(hex);
        listing.push('\n');
    }

    if let Ok(path) = std::env::var("ZM_CHECKSUM_CORPUS_DUMP") {
        std::fs::write(&path, &listing).expect("dump path is writable");
    }

    let mut hasher = Sha256::new();
    hasher.update(listing.as_bytes());
    let aggregate = hex::encode(hasher.finalize());

    // Captured on `main` at 2e764660, BEFORE the `DialectId` /
    // `BackendDescriptor` work. Any change to this value means an op's wire
    // spelling moved and every deployed migration using that op now has a
    // different identity checksum. That is a break, not a golden to re-record.
    const EXPECTED_AGGREGATE: &str =
        "65504d0ea9159e5a912f1d61888441e6377faebef87ae4fbc1cc8034d9de61d2";
    assert_eq!(
        aggregate,
        EXPECTED_AGGREGATE,
        "the op-list wire format moved: {} corpus rows re-hashed. \
         Re-recording this constant invalidates every deployed journal.",
        rows.len()
    );
}
