//! SQLite-side integration tests — skeleton (P1 PR 1).
//!
//! Behind `required-features = ["sqlite", "test-helpers"]` so the
//! default-feature build never compiles this file. PR 1 ships a
//! single `smoke_compiles` test to keep the integration target wired
//! into `cargo test` — PR 2-5 backfill mirror coverage for the
//! register-model / migration / introspection paths under the SQLite
//! arm per `docs/proposals/p1-sqlite-implementation-plan.md` §7.

/// Smoke test: the integration-test target compiles + a `#[test]` is
/// registered, so `cargo test -p zeroship-plugin-db --test
/// sqlite_integration --features "sqlite test-helpers"` exits 0 once
/// PR 1 lands. The body has no observable behaviour — every SQLite
/// capability method is a stub at PR 1.
#[test]
fn smoke_compiles() {
    // Intentionally empty — pinning the target wiring, not behaviour.
}
