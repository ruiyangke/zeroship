//! `Secret<T>` must have no value-revealing formatting surface at all.
//!
//! The redaction test in the linked-registry suite covers `Debug`. This covers
//! `Display`, which a redacting `Debug` alone would not stop.

use zeroship_core::config::Secret;

fn main() {
    println!("{}", Secret::new(String::from("supersecretpw")));
}
