//! Cross-app foreign-key parse-time check — stub.
//!
//! **P1 PR 1**: skeleton only — returns `Ok(())` so the orchestrator
//! call site (`orchestrator/register_model/bootstrap.rs::build_ctx`,
//! post-strictness-read, pre-lock-acquire) can be wired in PR 5
//! without re-plumbing imports.
//!
//! **P1 PR 5**: real validator walks `t.ref(target)` entries in the
//! supplied schema JSON and rejects any FK whose `target` carries an
//! `"<other_app>."` prefix. See §18 Q1 in the design doc and
//! `docs/proposals/p1-sqlite-implementation-plan.md` §6 for the
//! intended error shape:
//!
//! ```text
//! DbError::Configuration {
//!     code: "cross_app_fk_forbidden",
//!     message: "FK target \"other_app.users\" crosses app boundary; \
//!               only same-app FKs allowed",
//!     hint: Some("Drop the \"other_app.\" prefix from refTarget"),
//! }
//! ```

use crate::error::DbError;

/// Validate that no FK in `schema` references a different app's
/// collection.
///
/// **P1 PR 1 stub**: unconditional `Ok(())` — the real validator
/// lands in PR 5. Wiring this through now means PR 5 only edits the
/// body (and the orchestrator hook), not the call-site signature.
#[allow(dead_code)]
pub(crate) fn reject_cross_app_fk(
    _schema: &serde_json::Value,
    _app_id: &str,
) -> Result<(), DbError> {
    Ok(())
}
