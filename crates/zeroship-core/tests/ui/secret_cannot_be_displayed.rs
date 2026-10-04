//! `Secret<T>` must have no value-revealing formatting surface at all.
//!
//! `the_resolved_struct_redacts_its_secret_without_a_hand_written_debug` in
//! crates/zeroship-config-contract/tests/integration/linked_registry.rs covers
//! `Debug`. This covers `Display`, which a redacting `Debug` alone would not
//! stop.
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
