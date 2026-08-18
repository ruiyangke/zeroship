//! The foreign-key candidate-key check sees `alterPrimaryKey`.
//!
//! A table-level FK is admitted only when the referenced columns are provably an
//! exact ordered candidate key. `snapshot_has_reference_key` decides that by
//! scanning a replayed `TableSnapshot` for a constraint of kind `PRIMARY KEY` or
//! `UNIQUE`. The replay that advances that snapshot handled `addConstraint`,
//! `dropConstraint`, `createIndex` and `dropIndex` - and not `alterPrimaryKey`.
//!
//! So the op that MOVES a primary key was invisible to the check whose whole
//! purpose is knowing where the primary key is.
//!
//! BOTH DIRECTIONS WERE MEASURED, and both are in this fixture, because a fix
//! written against either one alone leaves the other:
//!
//!     PK moved ONTO the FK target   the key exists, the replay cannot see it
//!                                   -> FALSE REFUSAL of a valid migration
//!     PK moved AWAY from the target the key is gone, the replay still shows it
//!                                   -> FALSE ACCEPT; the FK reaches apply and
//!                                      the server rejects it
//!
//! The false accept is the dangerous half: this check exists to prove a
//! candidate key is present BEFORE allowing the reference, so a stale replay
//! defeats the check it implements.
//!
//! THE PROOF NEEDED NO SERVER. The engine's own rule treats PRIMARY KEY and
//! UNIQUE alike - it matches both - so accepting a foreign key after
//! `UNIQUE(k)` while refusing the identical one after the primary key moves to
//! `k` is internally contradictory whatever PostgreSQL does. (PostgreSQL does
//! accept a reference to a primary key; that is textbook. The contradiction is
//! the tighter argument.)

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir, Dialect};

fn verdict(ops: &str) -> Result<(), String> {
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    validate_ir(&ir, Dialect::Postgres).map_err(|e| format!("{}: {}", e.code, e.reason))
}

/// `a` has primary key `(c0)` and a plain column `k`.
const A: &str = r#"{"op":"createTable","name":"a","columns":[{"name":"c0","type":"int","nullable":false},{"name":"k","type":"int","nullable":false}],"primaryKey":["c0"]}"#;
/// `b.v` references `a.k` - valid only if something makes `k` a candidate key.
const B_FK_K: &str = r#"{"op":"createTable","name":"b","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"],"constraints":[{"name":"fk1","kind":{"kind":"fk","columns":["v"],"referencesTable":"a","referencesColumns":["k"]}}]}"#;
/// `b.v` references `a.c0` - valid only while `c0` remains a candidate key.
const B_FK_C0: &str = r#"{"op":"createTable","name":"b","columns":[{"name":"c0","type":"int","nullable":false},{"name":"v","type":"int","nullable":true}],"primaryKey":["c0"],"constraints":[{"name":"fk1","kind":{"kind":"fk","columns":["v"],"referencesTable":"a","referencesColumns":["c0"]}}]}"#;
const UNIQUE_K: &str = r#",{"op":"addConstraint","table":"a","constraint":{"name":"uk","kind":{"kind":"unique","columns":["k"]}}}"#;
const PK_C0_TO_K: &str = r#",{"op":"alterPrimaryKey","table":"a","action":{"kind":"replace","expectedColumns":["c0"],"columns":["k"]}}"#;

// ---------------------------------------------------------------------------
// Controls. These already held, and they are what make the two failures
// attributable to `alterPrimaryKey` rather than to the check tracking nothing.
// ---------------------------------------------------------------------------

#[test]
fn a_reference_to_a_column_with_no_candidate_key_is_refused() {
    verdict(&format!("{A},{B_FK_K}")).expect_err("k is neither primary nor unique");
}

#[test]
fn a_unique_constraint_makes_the_column_referenceable() {
    // THE LOAD-BEARING CONTROL. It proves the replay does track keys that ops
    // introduce - so a failure below is about `alterPrimaryKey` specifically,
    // not about the replay being inert.
    verdict(&format!("{A}{UNIQUE_K},{B_FK_K}"))
        .expect("addConstraint UNIQUE(k) makes k a candidate key");
}

#[test]
fn a_reference_to_the_live_primary_key_is_accepted() {
    verdict(&format!("{A},{B_FK_C0}")).expect("c0 is the primary key");
}

// ---------------------------------------------------------------------------
// The two failures.
// ---------------------------------------------------------------------------

#[test]
fn a_primary_key_moved_onto_an_unproven_target_stays_refused() {
    // DELIBERATELY CONSERVATIVE, and pinned here as a QUESTION rather than a
    // settled rule. The offline replay installs a new primary key only when the
    // tuple is already provably a candidate key (an index or a unique
    // constraint). `k` has neither, so the replay declines to claim it and the
    // reference is refused - even though at apply the primary key really will
    // cover `k`.
    //
    // That may be correct: the module states the locked live preflight is
    // authoritative and an offline replay must never invent a key. It may also
    // be over-strict, since alterPrimaryKey DECLARES the new key rather than
    // discovering it. An earlier draft of this fixture asserted the opposite;
    // that assertion was my guess, not a measurement, so it is recorded as an
    // open design question instead of a green test.
    verdict(&format!("{A}{PK_C0_TO_K},{B_FK_K}"))
        .expect_err("the replay does not claim an unproven key; see the note above");
}

#[test]
fn a_primary_key_moved_away_from_the_target_is_refused() {
    // FALSE ACCEPT before the fix, and the dangerous half: nothing makes c0 a
    // candidate key once the primary key moves to k, so this FK cannot be
    // satisfied and must not reach apply.
    verdict(&format!("{A}{PK_C0_TO_K},{B_FK_C0}"))
        .expect_err("c0 stopped being a candidate key when the primary key moved");
}
