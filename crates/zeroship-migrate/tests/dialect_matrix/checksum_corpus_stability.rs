//! The WIRE-FORMAT guard for `Checksum::of_ir` across the whole op corpus.
//!
//! `Checksum::of_ir` folds each op's RFC 8785 (JCS) canonical bytes, and JCS
//! sorts and EMITS object key names. Field names are therefore inside the
//! identity checksum of every already-deployed migration: any serde rename, any
//! added-without-`skip_serializing_if` field, any change to how a dialect is
//! spelled on the wire silently invalidates deployed history and breaks
//! drift/tamper detection against journals that are already in production.
//!
//! `crates/zeroship-migrate/tests/ir_contract/ir_checksum.rs` pins ONE golden over three hand-picked ops. That is a
//! spot check. This file pins the checksum of EVERY `(op-kind, variant)` row of
//! the shared dialect corpus — the same corpus `dialect_table_faithfulness.rs`
//! and `dialect_conformance_live.rs` drive — so a rename anywhere in the op
//! surface is caught, not only a rename inside `createTable`/`insert`/`raw`.
//!
//! Two assertions, deliberately:
//!
//! 1. The corpus is non-empty and each row's checksum is recomputed
//!    deterministically (a corpus that silently emptied would "pass" vacuously).
//! 2. The AGGREGATE digest over `kind|variant|checksum` for every row, in corpus
//!    order, equals a frozen constant.
//!
//! `corpus_checksums_are_byte_stable` below prints the full per-row listing to
//! stdout, which is what a before/after comparison across a refactor diffs: capture
//! it with `--nocapture` and shell redirection —
//! `cargo test -p zeroship-migrate --test dialect_matrix \
//! corpus_checksums_are_byte_stable -- --nocapture > before.txt` — rather than an
//! operator-chosen path read from the process environment (this crate's tests take
//! no input from their own environment; `clippy.toml`'s `disallowed-methods` on
//! `std::env::var`). libtest also surfaces the same printed listing automatically
//! on a FAILING run, with no flag needed.

use crate::dialect_corpus;

use sha2::{Digest, Sha256};
use zeroship_migrate::model::ir::CanonicalOpList;
use zeroship_migrate::{Checksum, MigrationFlags};

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

    // Printed, not written to an operator-chosen path: see this file's header for why
    // and how to capture it (`--nocapture` plus shell redirection).
    println!("{listing}");

    let mut hasher = Sha256::new();
    hasher.update(listing.as_bytes());
    let aggregate = hex::encode(hasher.finalize());

    // Re-recorded when the pre-production Dialectal contract deliberately moved
    // from closed `default`/`pg`/`sqlite`/`mysql` fields to a canonical
    // `DialectId`-keyed `legs` map. There is no deployed-journal compatibility
    // requirement, but every later wire change remains an explicit review gate.
    // Re-recorded again when the gated raw-statement escape stopped spelling one
    // server's product in the neutral vocabulary: the variant is `Op::Raw` and the
    // wire tag is `"raw"`. The review this assertion asks for was done with
    // `ZM_CHECKSUM_CORPUS_DUMP` on both sides:
    // of the 92 rows, EXACTLY ONE moved --
    //   -pgRaw|base|0d8096e44299459958f5667cc82d61b69b86b13b953faf0b9455c286d214b529
    //   +raw|base|7d09edb22e3b80abdfbe36ace97cac8d49c39d488a9d16bf3115b0224af85e81
    // -- and the other 91 are byte-identical, which is what makes this a scoped
    // rename rather than a wire-format drift. Aggregate:
    // 0590adee7a3048a19689e2f2632c860d59797afc5e72c3d19705dbc85f360471 ->
    // 7b960d132e2487c27567e906cec97834bf12222a9ca429b375d006072835b7c3.
    //
    // Re-recorded again when `FuncLanguage`'s procedural-language variant stopped
    // spelling one server's product in the neutral vocabulary: the variant is
    // `Procedural` and the wire tag is `"procedural"`, with the `plpgsql` token it
    // renders to now living in `zeroship-migrate-postgres`. Same review, same tool, and
    // again EXACTLY ONE of the 92 rows moved --
    //   -createFunction|base|57a4260da7befada75c0e8c0e34ebc21d88f7835cc984c3a04be10b008fabdc9
    //   +createFunction|base|d37ce769303e359ab9bbce5dbf6fdfe0128d377e46448ed1d5e423f5f84251c5
    // -- which is the row whose op carries a `language` field, and the other 91 are
    // byte-identical. Aggregate:
    // 7b960d132e2487c27567e906cec97834bf12222a9ca429b375d006072835b7c3 ->
    // 7af1f998f5cc38e4db2e25d004b08ff253dd04573df8475be7f10ead264297c7.
    const EXPECTED_AGGREGATE: &str =
        "7af1f998f5cc38e4db2e25d004b08ff253dd04573df8475be7f10ead264297c7";
    assert_eq!(
        aggregate,
        EXPECTED_AGGREGATE,
        "the op-list wire format moved: {} corpus rows re-hashed. Review the \
         exact wire diff before re-recording this constant.",
        rows.len()
    );
}
