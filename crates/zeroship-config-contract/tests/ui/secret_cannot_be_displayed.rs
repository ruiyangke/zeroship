//! `Secret<T>` must have no value-revealing formatting surface at all.
//!
//! The redaction test in the linked-registry suite covers `Debug`. This covers
//! `Display`, which a redacting `Debug` alone would not stop.
//!
//! The constructor matters: written against a constructor that no longer
//! exists, this file would still FAIL to compile and the test would still pass,
//! while proving nothing about `Display` at all.

use zeroship_core::config::{Secret, SourceKind};

fn main() {
    println!(
        "{}",
        Secret::supplied(SourceKind::Env, Some(String::from("supersecretpw")))
    );
}
