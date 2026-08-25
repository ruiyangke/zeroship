//! Golden-file gate for `attribute-vocabulary.json` — this backend's declared knobs,
//! exported for its npm package's TypeScript generator.
//!
//! The rendering itself lives in
//! [`zero_migrate_backend::attribute::vocabulary_json`], which names no vendor and is
//! shared by all three backends: three copies would be three chances for the artifacts
//! to disagree in shape, at which point one package's generator reads a field another
//! vendor never wrote. What stays here is what is genuinely this crate's — WHICH
//! vocabulary, and the floors that make the check non-vacuous.
//!
//! # Why the artifact lives next to the crate
//!
//! One per vendor. A single combined export would have to live somewhere naming all
//! three backends, and would hand every vendor's npm package the other vendors' knobs.
//! This file names postgres because it IS postgres's — the one place naming a vendor is the
//! design, not a leak.
//!
//! # Regenerating
//!
//! `UPDATE_VOCABULARY=1 cargo test -p zero-migrate-postgres --test attribute_vocabulary_export`
//! rewrites the file; commit it, then re-run the package's generator. A default run
//! ASSERTS the on-disk file matches, so adding a knob without regenerating fails rather
//! than shipping a TypeScript surface that silently lacks it.

use std::path::PathBuf;

use zero_migrate_backend::attribute::vocabulary_json;
use zero_migrate_postgres::{DIALECT, VENDOR};

/// Below this, the export is near-empty and every assertion over it goes vacuous. An
/// artifact of zero attributes is valid JSON that generates an empty TypeScript
/// interface, which reads exactly like "this backend has no knobs" — wrong, and
/// indistinguishable from a cleared vocabulary or a walk over the wrong static.
const DECLARED_FLOOR: usize = 3;

fn artifact_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("attribute-vocabulary.json")
}

fn generated() -> String {
    // Bound rather than inlined: `DIALECT` is a `const`, so `DIALECT.as_str()` would
    // borrow a temporary that dies at the end of the statement.
    let dialect = DIALECT;
    vocabulary_json(dialect.as_str(), VENDOR.attributes).expect("vocabulary serializes")
}

#[test]
fn emit_attribute_vocabulary() {
    let path = artifact_path();
    let generated = generated();
    if std::env::var("UPDATE_VOCABULARY").is_ok() {
        std::fs::write(&path, generated.as_bytes()).expect("write attribute-vocabulary.json");
        return;
    }
    let on_disk = std::fs::read_to_string(&path).unwrap_or_else(|e| {
        panic!(
            "attribute-vocabulary.json missing or unreadable at {}: {e}. Run \
             `UPDATE_VOCABULARY=1 cargo test -p zero-migrate-postgres --test \
             attribute_vocabulary_export` to generate it.",
            path.display()
        )
    });
    assert_eq!(
        on_disk, generated,
        "attribute-vocabulary.json is stale. Regenerate with `UPDATE_VOCABULARY=1 cargo \
         test -p zero-migrate-postgres --test attribute_vocabulary_export` and commit it, then \
         re-run the TypeScript generator."
    );
}

#[test]
fn the_export_carries_this_backends_declarations_and_not_an_empty_document() {
    let declared = VENDOR.attributes.len();
    assert!(
        declared >= DECLARED_FLOOR,
        "the postgres vocabulary exports {declared} attribute(s), expected at least \
         {DECLARED_FLOOR} — an empty export generates an empty TypeScript surface and \
         reads as 'this backend has no knobs'"
    );

    let doc = generated();
    // The dialect becomes the DSL namespace key, so a wrong or empty one silently
    // renames the whole authoring surface.
    assert!(
        doc.contains("\"dialect\": \"postgres\""),
        "the export must name the dialect the generator will use as the DSL key"
    );
    // A spot-check that the SHAPE survived serialization in the form the generator
    // reads. `kind` is the discriminant it switches on; without it every attribute
    // would fall through as unknown.
    assert!(
        doc.contains("\"kind\":"),
        "every shape must serialize with its `kind` discriminant: {doc}"
    );
}

/// Every exported key is in THIS backend's namespace.
///
/// The workspace-level census checks ownership across all vendors, but it lives in the
/// launcher crate and cannot run if only this crate is built. The same claim is asserted
/// here because the artifact is what the npm package is generated FROM: a foreign key
/// exported here would surface in this package's typings under someone else's namespace.
#[test]
fn every_exported_key_belongs_to_this_dialect() {
    let dialect = DIALECT;
    let mut checked = 0_usize;
    for def in VENDOR.attributes.iter() {
        assert_eq!(
            def.key.dialect(),
            dialect.as_str(),
            "`{}` is exported by the `{}` vocabulary but belongs to `{}`",
            def.key,
            dialect.as_str(),
            def.key.dialect()
        );
        checked += 1;
    }
    assert!(
        checked >= DECLARED_FLOOR,
        "walked {checked} attribute(s) — a walk over an empty set proves nothing"
    );
}
