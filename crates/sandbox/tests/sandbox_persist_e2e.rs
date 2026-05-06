//! Superseded: this file used to hold the sealed-record filesystem
//! restore tests. Restart restore is now pg-driven, and the
//! replacement tests live in `sandbox_pg_e2e.rs` (gated on
//! `PG_TEST_URL`).
//!
//! Specifically, the tests this file used to host (and where they live
//! now):
//! - `restart_with_unreachable_agent_keeps_sealed_records` →
//!   `sandbox_pg_e2e::restart_restore_unreachable_agent_marks_status`.
//! - `restart_with_mismatched_agent_deletes_sealed_record_and_records_outcome` →
//!   `sandbox_pg_e2e::restart_restore_mismatched_fingerprint_marks_recreating`.
//! - `restart_with_matching_agent_rehydrates_for_supported_backend` →
//!   `sandbox_pg_e2e::restart_restore_round_trip`.
//! - `persist_on_mint_*` paths → no longer apply (audit moves to pg;
//!   sealed record is secret-only). The pg-side equivalents are
//!   `sandbox_pg_e2e::insert_share_round_trip` and
//!   `sandbox_pg_e2e::rotate_share_secret_revokes_existing_rows`.
//!
//! This file is deliberately empty (apart from this header comment) —
//! it would otherwise have to drive the v2 sealed-record dual-write
//! shape that round-8 explicitly removes.
