//! `zeroship-migrate` — zeroship's versioned DB migration engine for **creator
//! project databases** (design `docs/proposals/2026-06-16-db-migration-engine-design.md`).
//!
//! This crate (Plan 1) implements the **security core** + the **migration
//! unit**: the migration data types (§2.1) and the parse-time SQL security
//! guard (§1.4 deny-list, §1.5 cross-schema confinement). The executor on
//! Postgres (journal, advisory lock, apply flow), the least-privilege
//! `migrator` role, and the authoring pipeline are later plans built on these
//! types.
//!
//! # Security stance (§1)
//!
//! Migrations are **privileged arbitrary-SQL** authored by **untrusted**
//! creators *and* a **prompt-injectable AI**. The threat surface is
//! cross-tenant access, privilege escalation, Postgres host-escape / RCE,
//! filesystem + network reach, and data loss.
//!
//! Defense is in depth:
//!
//! - **Line 1 — this guard ([`guard::SqlGuard`]).** Every statement is parsed
//!   with the *real* Postgres parser (`pg_query`/`libpg_query` — chosen over a
//!   pure-Rust parser precisely so a deny-list cannot be bypassed by exotic
//!   syntax it would misparse) and checked against a hard deny-list. Dangerous
//!   constructs nested inside `DO $$…$$` blocks and function bodies are
//!   inspected too, not just top-level statements. Unparseable input is
//!   denied. The guard **denies** RCE / priv-esc / cross-tenant / file /
//!   network, and only **flags** data loss (`DROP`/`TRUNCATE`/lossy type
//!   change) — the apply gate (a later plan) decides on destructive ops.
//! - **Line 2 — the least-privilege `migrator` role** (a later plan). The DB
//!   itself rejects the same ops even if SQL somehow slips past parse.
//!
//! The guard runs **out-of-band at deploy time** (not on the request hot path),
//! so it is plain synchronous logic — no tokio/compio — and exhaustively
//! unit-testable without a database (`tests/guard_security.rs`).

pub mod classify;
pub mod guard;
pub mod migration;

// ---------------------------------------------------------------------------
// Public API surface — re-exports (later plans depend on these names).
// ---------------------------------------------------------------------------

pub use classify::{classify, DdlKind, ParseError, StatementClass};
pub use guard::{flags_for, GuardConfig, GuardError, GuardReport, SqlGuard};
pub use migration::{Checksum, IdError, Migration, MigrationFlags, MigrationId, MIGRATION_PREFIX};
