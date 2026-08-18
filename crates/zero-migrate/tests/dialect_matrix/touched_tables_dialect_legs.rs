//! **The bare-name `dropIndex` supplement sees inside dialect legs, like its base.**
//!
//! `IrAuthor::resolved_touched_tables` builds the pending-contract interlock set in two
//! parts. The base, `MigrationIr::touched_tables`, descends `Op::Dialectal` and always
//! has. The supplement bolted on after it - which resolves a `dropIndex` that omits its
//! owning table, either to the live owner or to a fail-closed unknown sentinel -
//! matched `Op::DropIndex` at the TOP LEVEL only. So one function disagreed with itself
//! about whether a leg counts, and a bare-name drop authored inside `dialect({ ... })`
//! contributed nothing to the interlock set.
//!
//! ALL LEGS, and here that is forced rather than chosen: `resolved_touched_tables` is an
//! associated function taking `(ir, live)` with NO dialect, so selecting a leg is not
//! even expressible. The base it extends already descends every leg, and over-claiming a
//! touched table costs a conservative interlock while under-claiming loses one, so both
//! the structure and the failure direction point the same way.
//!
//! The oracle is a DIFFERENTIAL rather than the sentinel's spelling: the same bare-name
//! drop is authored once at the top level and once inside a leg, and the two touched
//! sets must match. That keeps the test off a `pub(crate)` constant and off any
//! assumption about how an unresolved owner is represented.
//!
//! Defense-in-depth rather than a live hazard: the doc on the function records that a
//! bare-name `dropIndex` is already rejected at validate on the production path, so this
//! arm serves callers that lower without the validator. Fixed because a function whose
//! two halves disagree is the shape that later gets copied, not because a user is
//! currently reaching it.

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::{IrAuthor, LiveSchema};

fn touched(ops_json: &str) -> Vec<String> {
    let raw = format!(r#"{{"ir_version":1,"name":"touched","ops":{ops_json}}}"#);
    let ir: MigrationIr = serde_json::from_str(&raw).expect("the touched-set test IR parses");
    let mut out = IrAuthor::resolved_touched_tables(&ir, &LiveSchema::default());
    out.sort();
    out
}

/// The control: a top-level bare-name drop contributes an entry, so the differential
/// below is comparing against something rather than against an empty set.
#[test]
fn a_top_level_bare_name_drop_index_contributes_a_touched_entry() {
    let top = touched(r#"[{"op":"dropIndex","name":"orphan_idx"}]"#);
    assert!(
        !top.is_empty(),
        "a bare-name dropIndex resolves to an owner or to the fail-closed sentinel: \
         {top:?}"
    );
}

/// The reported defect: the same op inside a leg contributed nothing, so one function
/// descended legs in its base and not in its supplement.
#[test]
fn a_bare_name_drop_index_inside_a_leg_contributes_the_same_entry() {
    let top = touched(r#"[{"op":"dropIndex","name":"orphan_idx"}]"#);
    let wrapped = touched(
        r#"[{"op":"dialectal","sqlite":[
              {"op":"dropIndex","name":"orphan_idx"}]}]"#,
    );
    assert_eq!(
        wrapped, top,
        "the same bare-name drop must reach the interlock set whether or not a \
         dialect() wrapper stands in front of it"
    );
}

/// A leg this run would not select still counts, because the function has no dialect to
/// select with and its base already claims every leg's tables.
#[test]
fn a_bare_name_drop_index_in_any_leg_contributes_the_same_entry() {
    let top = touched(r#"[{"op":"dropIndex","name":"orphan_idx"}]"#);
    let wrapped = touched(
        r#"[{"op":"dialectal","pg":[
              {"op":"dropIndex","name":"orphan_idx"}]}]"#,
    );
    assert_eq!(
        wrapped, top,
        "the interlock set is dialect-independent here, so every leg's bare-name drop \
         contributes"
    );
}
