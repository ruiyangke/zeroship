//! Emit the internal `pg_seam` cfg = (`native-pg` OR `host-pg`).
//!
//! The GENERIC Postgres apply path — the `PgSession` trait, the generic
//! `PostgresBackend<D>`, the `<D: PgSession>` journal/drift/precondition/baseline
//! free functions, the `<D: PgSession>` executor entries, and `ops::status` —
//! names NO `compio_postgres` type since Phase A neutralised the seam
//! (`SeamBind`/`SeamRow`/`SeamError`). So it must compile whenever EITHER the
//! native compio driver (`native-pg`) OR the host-callback plumbing (`host-pg`)
//! is present. `pg_seam` is that union: gating the shared path on `pg_seam` keeps
//! it out of the `--no-default-features` (PG-omitted) lean core while lighting it
//! up for both `native-pg` and `host-pg`.
//!
//! The compio-CONCRETE pieces (the `impl PgSession for Client` adapters,
//! PgOnline/PgShadow, `conn::connect`, role provisioning, the `&Client` executor
//! entries) stay `#[cfg(feature = "native-pg")]` — they name compio types and
//! must NOT enter the `host-pg` (cross-platform, no-io_uring) build.

fn main() {
    // Teach rustc that `pg_seam` is an expected cfg (Rust 1.80+ checked-cfg), so a
    // `--no-default-features` build does not warn on the `cfg(pg_seam)` gates.
    println!("cargo::rustc-check-cfg=cfg(pg_seam)");

    // The native compio Postgres driver (`native-pg` feature) was a monorepo
    // private crate and is out of scope for this standalone (host-pg + SQLite
    // only). The `native-pg` feature is no longer declared in Cargo.toml, but the
    // compio-CONCRETE PG code (`#[cfg(feature = "native-pg")]`) stays as
    // permanently-off dead code. Declaring the cfg keeps the build free of
    // `unexpected_cfgs` warnings without resurrecting the feature or the driver.
    println!("cargo::rustc-check-cfg=cfg(feature, values(\"native-pg\"))");

    let native_pg = std::env::var_os("CARGO_FEATURE_NATIVE_PG").is_some();
    let host_pg = std::env::var_os("CARGO_FEATURE_HOST_PG").is_some();
    if native_pg || host_pg {
        println!("cargo::rustc-cfg=pg_seam");
    }

    // Re-run only when the manifest (feature set) changes.
    println!("cargo::rerun-if-changed=build.rs");
    println!("cargo::rerun-if-changed=Cargo.toml");
}
