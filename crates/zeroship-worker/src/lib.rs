pub mod cache;
pub mod config;
pub mod executable;
// `cache`/`metrics` are compiled into BOTH targets: main.rs also declares a
// private `mod cache;`/`mod metrics;` so `handler.rs`/`sync.rs` (private
// modules of the BINARY, not this lib) can reach `cache`'s `pub(crate)` test
// seam (`test_db_service`) as same-crate code. `db_posture` has no such
// consumer - its only caller anywhere in this crate is the one
// `validate_database_url` call in `main()`, already `pub`, so it lives here
// ONLY, reached from main.rs as `zeroship_worker::db_posture::...`, the same
// way main.rs already reaches `config`. Two consequences: `cargo test -p
// zeroship-worker --lib` (the per-crate command AGENTS.md documents) is now
// what reaches its boot-gate assertions - previously only `--bin
// zeroship-worker` did, and nobody runs that by habit. And its one
// `#[compio::test]` opens a real Postgres connection and creates/drops
// scratch roles; dual-declaring it like `cache` would run that live round
// trip twice on every full `cargo test -p zeroship-worker`, a cost `cache`'s
// 21 tests (all synchronous, no I/O) never carried.
pub mod db_posture;
pub mod metrics;
// Dual-declared for the same reason as `cache` and `metrics`: `handler.rs` is
// a private module of the BINARY and reaches this one as same-crate code,
// while `cargo test -p zeroship-worker --lib` - the per-crate command
// AGENTS.md documents - only sees what this file declares. Its tests are
// synchronous and open nothing, so running them in both targets costs the
// same as `cache`'s do and buys a fence that neither command can miss.
pub mod policy;
pub mod workflow_runtime;
