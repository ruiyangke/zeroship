//! A function signature may not be claimed twice, and `CREATE OR REPLACE` is not
//! an unconditional exemption.
//!
//! `createFunction` was the last name-claiming op with no name check at all. It
//! survived F717's sweep and F759's re-probe of the privileged ops for the same
//! reason both of those missed things: it is gated behind `CAP_FUNCTION`, so a
//! confined probe is answered by the capability gate and never reaches a name
//! check. Under a granting profile the engine accepted every collision below.
//!
//! THE KEY IS THE SIGNATURE, NOT THE NAME. Overloading is the point of the
//! object, so a name-keyed rule - the obvious one, and the shape every sibling
//! rule in this check uses - would refuse ordinary code. Measured, both
//! directions:
//!
//!     CREATE FUNCTION b(int); CREATE FUNCTION b(text)    ACCEPTED (an overload)
//!     CREATE FUNCTION a();    CREATE FUNCTION a()        ERROR: function "a"
//!                                      already exists with same argument types
//!     CREATE FUNCTION d(x int); CREATE FUNCTION d(y int) ERROR (the same error)
//!
//! That last line is what fixes the key: ARGUMENT NAMES DO NOT PARTICIPATE. A
//! signature built from the rendered argument list - name and type together,
//! which is what the SQL looks like - would pass the first two lines and miss it.
//!
//! `OUT` PARAMETERS DO NOT PARTICIPATE EITHER, and `INOUT` does. Measured rather
//! than taken from the manual, because the two directions have opposite costs:
//!
//!     f(x int); f(x int, OUT y int)     ERROR - same signature
//!     f(x int); f(x int, INOUT z int)   ACCEPTED - a different signature
//!
//! REPLACE IS A NARROW LICENCE. It permits a new body, and nothing else:
//!
//!     r1() RETURNS int;  CREATE OR REPLACE r1() RETURNS text
//!         ERROR: cannot change return type of existing function
//!     r2(x int);         CREATE OR REPLACE r2(y int)
//!         ERROR: cannot change name of input parameter "x"
//!
//! TYPE ALIASES COLLIDE. All eight folded pairs were measured, one CREATE per
//! pair, and each raised `already exists with same argument types`: int/integer/
//! int4, bigint/int8, smallint/int2, bool/boolean, varchar/character varying,
//! decimal/numeric, double precision/float8, timestamptz/timestamp with time
//! zone. Three near-neighbours were measured NOT to collide and are deliberately
//! kept apart - int/bigint, varchar/text, timestamptz/timestamp - because
//! folding those would refuse a real overload. An unrecognised spelling falls
//! through to itself: a missing alias under-refuses exactly as the engine did
//! before this rule, while a wrong one would refuse a migration the server runs.

mod support;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::validate::{validate_ir_authorized, Dialect, VendorAuthority};

fn verdict(ops: &str) -> Result<(), String> {
    let policy = support::operator_charter("public");
    let bytes = format!(r#"{{"ir_version":1,"name":"n","ops":[{ops}]}}"#);
    let ir: MigrationIr = serde_json::from_str(&bytes).expect("the envelope parses");
    let authority = VendorAuthority {
        effective: &policy,
        default_schema: "public",
    };
    validate_ir_authorized(&ir, Dialect::Postgres, None, Some(authority))
        .map_err(|e| format!("{}: {}", e.code, e.reason))
}

/// A capability denial satisfies `expect_err` while proving nothing - the exact
/// false green F718 recorded. Every refusal here must be about the signature.
fn expect_signature_refusal(ops: &str, what: &str, must_mention: &str) {
    let refusal = verdict(ops).expect_err(what);
    assert!(
        !refusal.contains("VENDOR_OP_DENIED"),
        "this must be refused for the SIGNATURE, not by the capability gate: {refusal}"
    );
    assert!(
        refusal.contains(must_mention),
        "the refusal must be the one this test is about, and say {must_mention:?}: {refusal}"
    );
}

/// `args` is a raw JSON array body; `returns` and `replace` complete the op.
fn func(name: &str, args: &str, returns: &str, replace: bool) -> String {
    format!(
        r#"{{"op":"createFunction","name":"{name}","args":[{args}],"returns":"{returns}","language":"sql","replace":{replace},"body":"SELECT 1"}}"#
    )
}

fn arg(name: &str, ty: &str) -> String {
    format!(r#"{{"name":"{name}","type":"{ty}"}}"#)
}

// ---------------------------------------------------------------------------
// Refusals.
// ---------------------------------------------------------------------------

#[test]
fn the_same_signature_twice_is_refused() {
    let f = func("fx", "", "int", false);
    expect_signature_refusal(
        &format!("{f},{f}"),
        "PostgreSQL rejects a repeated function signature",
        "already defined",
    );
}

#[test]
fn the_same_argument_types_twice_is_refused() {
    let f = func("fx", &arg("x", "int"), "int", false);
    expect_signature_refusal(
        &format!("{f},{f}"),
        "same name and same argument types collide",
        "already defined",
    );
}

#[test]
fn renaming_the_arguments_does_not_make_a_new_overload() {
    // MEASURED: `function "d" already exists with same argument types`. The line
    // that proves argument NAMES are not part of the key.
    expect_signature_refusal(
        &format!(
            "{},{}",
            func("fx", &arg("x", "int"), "int", false),
            func("fx", &arg("y", "int"), "int", false)
        ),
        "argument names do not distinguish two functions",
        "already defined",
    );
}

#[test]
fn a_type_alias_does_not_make_a_new_overload() {
    // `int` and `integer` are one type, so this is a collision and not an
    // overload - measured, along with the seven other folded pairs.
    expect_signature_refusal(
        &format!(
            "{},{}",
            func("fx", &arg("x", "int"), "int", false),
            func("fx", &arg("x", "integer"), "int", false)
        ),
        "int and integer are the same type",
        "already defined",
    );
}

#[test]
fn an_out_parameter_does_not_make_a_new_overload() {
    // MEASURED: f(x int) and f(x int, OUT y int) are the same signature.
    expect_signature_refusal(
        &format!(
            "{},{}",
            func("fx", &arg("x", "int"), "int", false),
            func(
                "fx",
                &format!(
                    r#"{},{{"name":"y","type":"int","mode":"out"}}"#,
                    arg("x", "int")
                ),
                "int",
                false
            )
        ),
        "an OUT parameter carries no signature weight",
        "already defined",
    );
}

#[test]
fn replace_may_not_change_the_return_type() {
    expect_signature_refusal(
        &format!(
            "{},{}",
            func("fx", "", "int", false),
            func("fx", "", "text", true)
        ),
        "CREATE OR REPLACE cannot change a return type",
        "return type",
    );
}

#[test]
fn replace_may_not_rename_an_input_parameter() {
    expect_signature_refusal(
        &format!(
            "{},{}",
            func("fx", &arg("x", "int"), "int", false),
            func("fx", &arg("y", "int"), "int", true)
        ),
        "CREATE OR REPLACE cannot rename an input parameter",
        "renames input parameter",
    );
}

// ---------------------------------------------------------------------------
// Controls. Every one of these is a shape the server accepts.
// ---------------------------------------------------------------------------

#[test]
fn a_genuine_overload_is_still_allowed() {
    verdict(&format!(
        "{},{}",
        func("fx", &arg("x", "int"), "int", false),
        func("fx", &arg("x", "text"), "int", false)
    ))
    .expect("differing argument types are an overload, which is the point of functions");
}

#[test]
fn replace_with_an_identical_signature_is_still_allowed() {
    // The ordinary shape: redefine the body of a function this migration just
    // created. The whole rule exists to permit this and refuse the rest.
    verdict(&format!(
        "{},{}",
        func("fx", &arg("x", "int"), "int", false),
        func("fx", &arg("x", "int"), "int", true)
    ))
    .expect("replacing a body is what CREATE OR REPLACE is for");
}

#[test]
fn near_neighbour_types_are_not_folded_together() {
    // Measured NOT to collide. Folding any of these would refuse a real overload,
    // which is why the alias table is measured rather than reasoned about.
    for (a, b) in [
        ("int", "bigint"),
        ("varchar", "text"),
        ("timestamptz", "timestamp"),
    ] {
        verdict(&format!(
            "{},{}",
            func("fx", &arg("x", a), "int", false),
            func("fx", &arg("x", b), "int", false)
        ))
        .unwrap_or_else(|e| panic!("{a} and {b} are distinct types and overload cleanly: {e}"));
    }
}

#[test]
fn an_inout_parameter_does_make_a_new_overload() {
    // The other half of the mode measurement: INOUT participates where OUT does
    // not, so this must stay allowed.
    verdict(&format!(
        "{},{}",
        func("fx", &arg("x", "int"), "int", false),
        func(
            "fx",
            &format!(
                r#"{},{{"name":"z","type":"int","mode":"inout"}}"#,
                arg("x", "int")
            ),
            "int",
            false
        )
    ))
    .expect("an INOUT parameter makes a distinct signature");
}

#[test]
fn dropping_the_function_frees_its_signature() {
    verdict(&format!(
        r#"{},{{"op":"dropFunction","name":"fx"}},{}"#,
        func("fx", "", "int", false),
        func("fx", "", "int", false)
    ))
    .expect("drop then recreate is a real pattern");
}

#[test]
fn dropping_one_overload_by_arg_types_frees_only_that_one() {
    // The precise drop releases the `int` overload...
    verdict(&format!(
        r#"{},{},{{"op":"dropFunction","name":"fx","argTypes":["int"]}},{}"#,
        func("fx", &arg("x", "int"), "int", false),
        func("fx", &arg("x", "text"), "int", false),
        func("fx", &arg("x", "int"), "int", false)
    ))
    .expect("the dropped overload's signature is free again");

    // ...and leaves the `text` one claimed. Without this second half, a
    // dropFunction that cleared every overload would pass the first half.
    expect_signature_refusal(
        &format!(
            r#"{},{},{{"op":"dropFunction","name":"fx","argTypes":["int"]}},{}"#,
            func("fx", &arg("x", "int"), "int", false),
            func("fx", &arg("x", "text"), "int", false),
            func("fx", &arg("x", "text"), "int", false)
        ),
        "the untouched overload is still claimed",
        "already defined",
    );
}

#[test]
fn a_function_may_share_a_name_with_a_table() {
    // MEASURED: `CREATE TABLE t; CREATE FUNCTION t()` is accepted - functions do
    // not live in the relation namespace. Folding them into `relations`, which is
    // where every other name in this check goes, would refuse it.
    let tbl = r#"{"op":"createTable","name":"fx","columns":[{"name":"c0","type":"int","nullable":false}],"primaryKey":["c0"]}"#;
    verdict(&format!("{tbl},{}", func("fx", "", "int", false)))
        .expect("a function and a table may share a name");
}

#[test]
fn distinct_function_names_are_still_allowed() {
    verdict(&format!(
        "{},{}",
        func("f1", &arg("x", "int"), "int", false),
        func("f2", &arg("x", "int"), "int", false)
    ))
    .expect("two differently named functions are ordinary");
}
