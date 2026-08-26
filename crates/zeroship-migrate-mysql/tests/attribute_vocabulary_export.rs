//! Golden-file gate for `attribute-vocabulary.json` - this backend's declared knobs,
//! exported for its npm package's TypeScript generator.
//!
//! The rendering itself lives in
//! [`zeroship_migrate_backend::attribute::vocabulary_json`], which names no vendor and is
//! shared by all three backends: three copies would be three chances for the artifacts
//! to disagree in shape, at which point one package's generator reads a field another
//! vendor never wrote. What stays here is what is genuinely this crate's - WHICH
//! vocabulary, and the floors that make the check non-vacuous.
//!
//! # Why the artifact lives next to the crate
//!
//! One per vendor. A single combined export would have to live somewhere naming all
//! three backends, and would hand every vendor's npm package the other vendors' knobs.
//! This file names mysql because it IS mysql's - the one place naming a vendor is the
//! design, not a leak.
//!
//! # Regenerating
//!
//! `UPDATE_VOCABULARY=1 cargo test -p zeroship-migrate-mysql --test attribute_vocabulary_export`
//! rewrites the file; commit it, then re-run the package's generator. A default run
//! ASSERTS the on-disk file matches, so adding a knob without regenerating fails rather
//! than shipping a TypeScript surface that silently lacks it.

use std::path::PathBuf;

use zeroship_migrate_backend::attribute::vocabulary_json;
use zeroship_migrate_mysql::{DIALECT, VENDOR};

/// Below this, the export is near-empty and every assertion over it goes vacuous. An
/// artifact of zero attributes is valid JSON that generates an empty TypeScript
/// interface, which reads exactly like "this backend has no knobs" - wrong, and
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
             `UPDATE_VOCABULARY=1 cargo test -p zeroship-migrate-mysql --test \
             attribute_vocabulary_export` to generate it.",
            path.display()
        )
    });
    assert_eq!(
        on_disk, generated,
        "attribute-vocabulary.json is stale. Regenerate with `UPDATE_VOCABULARY=1 cargo \
         test -p zeroship-migrate-mysql --test attribute_vocabulary_export` and commit it, then \
         re-run the TypeScript generator."
    );
}

#[test]
fn the_export_carries_this_backends_declarations_and_not_an_empty_document() {
    let declared = VENDOR.attributes.len();
    assert!(
        declared >= DECLARED_FLOOR,
        "the mysql vocabulary exports {declared} attribute(s), expected at least \
         {DECLARED_FLOOR} — an empty export generates an empty TypeScript surface and \
         reads as 'this backend has no knobs'"
    );

    let doc = generated();
    // The dialect becomes the DSL namespace key, so a wrong or empty one silently
    // renames the whole authoring surface.
    assert!(
        doc.contains("\"dialect\": \"mysql\""),
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

/// A 64-bit bound must survive the export EXACTLY.
///
/// This is a regression test for a defect that shipped in the first generated typings:
/// `auto_increment` declares `i64::MAX`, the artifact carried it as a JSON number, and
/// `JSON.parse` - whose numbers are `f64` - turned 9223372036854775807 into
/// 9223372036854776000. The generated doc comment then stated a bound this backend has
/// never declared.
///
/// Nothing failed. The types still compiled, the drift test still passed, and the only
/// symptom was a wrong number in prose. That is exactly the shape of defect worth a
/// standing test: it is invisible unless something asserts the exact digits.
///
/// MySQL is where this lives because MySQL is the backend that declares a bound past
/// 2^53. A vendor whose knobs all fit in a `f64` could not detect the regression.
#[test]
fn a_64_bit_bound_survives_the_export_without_precision_loss() {
    let doc = generated();

    // The exact decimal, as a STRING. A JSON number here would be the defect.
    assert!(
        doc.contains(&format!("\"max\": \"{}\"", i64::MAX)),
        "i64::MAX must be exported as the exact decimal string {}, not a JSON number: {doc}",
        i64::MAX
    );

    // The control: the corrupted value must NOT appear. Without this the assertion above
    // could pass while a second, lossy copy of the bound travelled alongside it.
    assert!(
        !doc.contains("9223372036854776000"),
        "the f64-rounded form of i64::MAX must not appear anywhere in the export: {doc}"
    );

    // And the bound really is the one under test - if MySQL stopped declaring a knob past
    // 2^53, this whole test would pass while checking nothing.
    assert!(
        VENDOR.attributes.iter().any(|d| matches!(
            d.shape,
            zeroship_migrate_backend::attribute::AttrShape::Int { max, .. } if max > (1_i64 << 53)
        )),
        "no declared bound exceeds 2^53, so this test can no longer detect the precision \
         loss it exists for"
    );
}

/// Every storage engine the manual lists must be accepted.
///
/// The first declaration listed five engines. MySQL's own table lists ten plus documented
/// aliases, so `FEDERATED`, `MERGE`, `NDB` and others were REFUSED at plan time despite
/// being legal. An enum that is too narrow is a wrong answer, not a conservative one:
/// unlike a missing key, which is refused loudly and obviously, a missing VARIANT looks
/// like the user made a typo.
#[test]
fn every_documented_storage_engine_is_accepted() {
    use zeroship_migrate_ir::ir::IrScalar;

    let def = VENDOR
        .attributes
        .iter()
        .find(|d| d.key.name() == "engine")
        .expect("mysql declares engine");

    // The five that the first declaration wrongly refused, plus the two most common, so
    // the test covers both the regression and the ordinary case.
    for engine in [
        "InnoDB",
        "MyISAM",
        "FEDERATED",
        "MERGE",
        "MRG_MyISAM",
        "NDB",
        "NDBCLUSTER",
        "EXAMPLE",
        "HEAP",
    ] {
        assert_eq!(
            def.shape.check(&IrScalar::Str(engine.to_string())),
            Ok(()),
            "`{engine}` is in MySQL's documented engine table and must be accepted"
        );
    }

    // The control: the enum still refuses something, so this is not passing because the
    // variant list became a free-for-all.
    assert!(
        def.shape
            .check(&IrScalar::Str("NoSuchEngine".to_string()))
            .is_err(),
        "the enum must still reject an engine MySQL does not have"
    );
}
