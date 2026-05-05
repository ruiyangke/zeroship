//! Compile-fail: WebIdlDict field is a reference type.
//!
//! Pre-Wave-7 the derive accepted `&str` (or any `Type::Reference`)
//! fields silently — the codegen emitted a member extraction that
//! reads an OWNED `String` and tried to assign it back into the
//! `&str` field, surfacing as a confusing rustc "expected &str,
//! found String" error pointing at code the user did not write.
//!
//! Now the derive rejects reference-typed fields at parse time with
//! a clear message naming the supported owned-type alternatives
//! (closes H16).
#![allow(dead_code, unused_imports)]

use zeroship_runtime_macros::WebIdlDict;

#[derive(Default, Debug, WebIdlDict)]
struct BadDict<'a> {
    name: &'a str,
}

fn main() {}
