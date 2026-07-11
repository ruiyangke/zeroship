//! Teach rustc that `cfg(feature = "introspect")` is an EXPECTED (though never
//! enabled) cfg. The live-catalog introspection-EXECUTION helpers
//! (`diff::read_live_schema`, `diff::estimate_row_count`, the `SchemaError`
//! wrapper) required a native compio Postgres driver to run. That driver was a
//! monorepo private crate and is out of scope for this standalone (host-pg +
//! SQLite only), so the `introspect` feature is not declared in Cargo.toml and
//! the gated code is permanently-off dead code. Declaring the cfg here keeps the
//! build free of `unexpected_cfgs` warnings without resurrecting the feature or
//! the PG driver.

fn main() {
    println!("cargo::rustc-check-cfg=cfg(feature, values(\"introspect\"))");
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=Cargo.toml");
}
