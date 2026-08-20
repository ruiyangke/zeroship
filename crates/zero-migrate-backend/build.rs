//! Declare the never-enabled `introspect` cfg, exactly as the engine's `build.rs`
//! already did for the same code.
//!
//! `schema_error::SchemaError` rode into this crate with `mask_codec` (it is the
//! sibling of `MaskSentinelError` in the same file). It names `compio_postgres`, a
//! driver out of scope for this standalone, and sits behind a feature nothing
//! declares — permanently-off dead code that the engine chose to keep rather than
//! delete. Declaring the cfg here keeps the build free of `unexpected_cfgs` warnings
//! without resurrecting the feature or the driver, and it is a verbatim carry of the
//! decision `zero-migrate/build.rs` records; this commit is a move, not a place to
//! re-open it.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(feature, values(\"introspect\"))");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=Cargo.toml");
}
