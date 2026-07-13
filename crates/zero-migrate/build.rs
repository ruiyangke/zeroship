//! Emit the internal `pg_seam` cfg from the `host-pg` feature.
//!
//! The GENERIC Postgres apply path — the `driver::SqlSession` trait, the generic
//! `PostgresBackend<D>`, the `<D: SqlSession>` journal/drift/precondition/baseline
//! free functions, the `<D: SqlSession>` executor entries, and `ops::status` —
//! names NO concrete driver type (the seam is `driver::{Bind, Row, DbError}`).
//! It compiles whenever the host-callback plumbing (`host-pg`) is present.
//! `pg_seam` gates that shared path so a `--no-default-features` (PG-omitted)
//! build keeps a lean core while `host-pg` lights it up.

fn main() {
    // Teach rustc that `pg_seam` is an expected cfg (Rust 1.80+ checked-cfg), so a
    // `--no-default-features` build does not warn on the `cfg(pg_seam)` gates.
    println!("cargo::rustc-check-cfg=cfg(pg_seam)");

    // The `schema` module tree (dissolved in from a former standalone schema
    // crate) carries the live-catalog introspection-EXECUTION
    // helpers (`schema::diff::read_live_schema` / `estimate_row_count`, the
    // `SchemaError` driver wrapper) behind a never-declared `introspect` feature:
    // they name `compio_postgres` (a driver out of scope for this standalone — the
    // engine does its own introspection over the `driver::SqlSession` seam) and are
    // permanently-off dead code. Declaring the cfg here keeps the build free of
    // `unexpected_cfgs` warnings without resurrecting the feature or the PG driver.
    println!("cargo::rustc-check-cfg=cfg(feature, values(\"introspect\"))");

    let host_pg = std::env::var_os("CARGO_FEATURE_HOST_PG").is_some();
    if host_pg {
        println!("cargo::rustc-cfg=pg_seam");
    }

    // Re-run only when the manifest (feature set) changes.
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=Cargo.toml");
}
