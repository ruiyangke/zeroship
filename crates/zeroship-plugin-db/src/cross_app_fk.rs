//! Cross-app foreign-key parse-time check.
//!
//! Rejects any `t.ref("<other_app>.<collection>")` reference whose
//! `<other_app>` prefix differs from the calling app's id. The check
//! exists so creators cannot accidentally (or intentionally) build a
//! schema whose physical FK crosses a per-app SQLite file boundary —
//! see design `docs/archive/db-system-design.md` §18 Q1 for the
//! ATTACH-isolation reasoning. Although the motivation is SQLite-
//! specific (PG can express cross-schema FKs natively), the platform
//! policy is "every FK stays inside one app's namespace" on both
//! backends so the data-isolation invariant is identical regardless of
//! which storage engine an app is running on.
//!
//! **Why not under `backend/sqlite/`?** This module was lifted out
//! of the old SQLite-only subtree so that a PG-only build would still
//! compile it. The implementation is engine-agnostic (pure-Rust JSON
//! walk, no SQL); the design-lineage signal that the rule originated in
//! the SQLite ATTACH design lives in the rustdoc here.
//!
//! # THIS VALIDATOR HAS NO PRODUCTION CALL SITE
//!
//! Re-enumerated 2026-08-26. Exactly ONE line in the crate calls
//! [`reject_cross_app_fk`], and it is not reachable from a running worker:
//!
//! - `register_model::bootstrap::bootstrap` - NOT `build_ctx`, which this
//!   paragraph named until 2026-08-20 and which does not call it and runs on
//!   the far side of the advisory lock. The whole `bootstrap` module is
//!   `#[cfg(any(test, feature = "test-helpers"))]`, so it is absent from a
//!   default build. Its only caller is `run_pipeline`, gated the same way.
//!
//! A SECOND CALLER USED TO BE LISTED HERE and is gone, not moved: it was the
//! dev-tier arm that drove the migration engine, and it was deleted outright
//! when plugin-db stopped depending on the engine in any profile. An earlier
//! version of this note pointed at a `tests/support/` fixture it had briefly
//! become; that file does not exist either. Beyond the one call above, the
//! only things that reach this validator are the two integration tests that
//! call it directly to pin its refusal.
//!
//! What production `registerModel` does instead is in
//! `register_model::exec_register_model`: the PG arm returns `Ok(())`
//! with no DDL and no validation (the migration engine is the PG schema
//! authority, applying at deploy), and the SQLite dev arm only calls
//! `ensure_app_schema`. Neither passes through here.
//!
//! **So do not read this module as the thing that keeps foreign keys
//! inside an app.** That property does hold, but it is owned by the
//! migration engine, which applies schema at deploy: a dot-qualified
//! column ref is refused by `reject_cross_app_ref` in the vendored
//! `zero-migrate` (`render/declarative.rs`), the renderer qualifies
//! every `REFERENCES` with the schema it was called FOR, and
//! `crates/migrated` derives that schema from the app id server-side.
//! The foreign keys section of `docs/reference/db.md` lays out the four
//! layers with file:line. `crates/zeroship-schema` is NOT one of them -
//! its FK builders are reached only from the same cfg-gated pipeline
//! this module sits in, and the engine carries its own copy.
//!
//! This module is kept because `tests/integration.rs` and
//! `tests/sqlite_integration.rs` pin its rejection contract, and
//! because the four-phase pipeline it belongs to is still the reference
//! shape for an apply. Whether the pipeline (and this with it) should
//! be deleted outright under the repo's no-back-compat stance is an
//! open call, flagged the same way at `register_model/mod.rs:41`.

use crate::error::DbError;

/// Walk every field definition in `schema` and reject any FK whose
/// `refTarget` carries a dot-qualified `<other_app>.` prefix.
///
/// **Schema shape** (matches the consumers at
/// `crate::diff::compute_diff` and `crate::query::build_create_table`):
/// `schema` is a JSON object whose keys are field names and whose
/// values are field definitions. A field with `"type": "ref"` carries a
/// `"refTarget": "<target>"` string. The `<target>` is either:
///
/// - a bare collection name (e.g. `"users"`) — same-app reference,
///   accepted; or
/// - a dot-qualified `<app>.<collection>` (e.g. `"other_app.users"`) —
///   accepted iff `<app> == app_id`, rejected otherwise.
///
/// Each caller passes ONE collection's field set, so the walk is
/// O(fields-in-one-collection). See the module header for who those
/// callers are - in a default build, nobody.
///
/// **Error envelope** matches the contract in `docs/archive/p1-sqlite-implementation-plan.md`
/// §6: `DbError::Configuration { code: "cross_app_fk_forbidden", ... }`.
/// The static `.code` is the canonical SDK-visible classifier; the
/// `hint` carries operator-facing remediation text.
pub fn reject_cross_app_fk(
    schema: &serde_json::Value,
    app_id: &str,
) -> Result<(), DbError> {
    let Some(obj) = schema.as_object() else {
        // Non-object schema is malformed — but this hook isn't the
        // place to flag that; the downstream `compute_diff` /
        // `build_create_table` builders already reject it with a
        // typed `ValidationFailed`. Return Ok so we don't double-fail.
        return Ok(());
    };

    for (_field, def) in obj {
        if def.get("type").and_then(|t| t.as_str()) != Some("ref") {
            continue;
        }
        let Some(target) = def.get("refTarget").and_then(|v| v.as_str()) else {
            continue;
        };
        if target.is_empty() {
            continue;
        }

        // Locate the first dot. SQLite's ATTACH alias convention reuses
        // the same `<app>.<collection>` shape PG uses for cross-schema
        // references, so the rule is "no FK whose target carries a `.`
        // prefix differing from `app_id`".
        let Some(dot_idx) = target.find('.') else {
            // Bare collection name — same-app ref by definition.
            continue;
        };

        let prefix = &target[..dot_idx];
        if prefix == app_id {
            // Same-app dot-qualified ref (e.g. an SDK that always emits
            // `<app>.<collection>` for clarity). Accept.
            continue;
        }

        return Err(DbError::Configuration {
            code: "cross_app_fk_forbidden",
            message: format!(
                "FK target \"{target}\" crosses app boundary; only same-app FKs allowed"
            ),
            hint: Some(
                "Drop the \"<app>.\" prefix from refTarget so the FK stays inside the calling app"
                    .to_string(),
            ),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    //! Unit tests for the cross-app FK parse check. Pure-Rust — no
    //! database round-trip needed; the input is always a `Value` and
    //! the output is `Result<(), DbError>`.

    use super::*;
    use serde_json::json;

    #[test]
    fn empty_schema_is_ok() {
        let schema = json!({});
        assert!(reject_cross_app_fk(&schema, "app_demo").is_ok());
    }

    #[test]
    fn non_ref_field_is_ignored() {
        let schema = json!({
            "title": { "type": "text" },
            "views": { "type": "integer" }
        });
        assert!(reject_cross_app_fk(&schema, "app_demo").is_ok());
    }

    #[test]
    fn same_app_bare_ref_is_ok() {
        let schema = json!({
            "authorId": { "type": "ref", "refTarget": "users" }
        });
        assert!(reject_cross_app_fk(&schema, "app_demo").is_ok());
    }

    #[test]
    fn same_app_dot_qualified_ref_is_ok() {
        let schema = json!({
            "authorId": { "type": "ref", "refTarget": "app_demo.users" }
        });
        assert!(reject_cross_app_fk(&schema, "app_demo").is_ok());
    }

    #[test]
    fn cross_app_ref_is_rejected() {
        let schema = json!({
            "authorId": { "type": "ref", "refTarget": "other_app.users" }
        });
        let err = reject_cross_app_fk(&schema, "app_demo")
            .expect_err("cross-app ref must reject");
        match err {
            DbError::Configuration { code, message, hint } => {
                assert_eq!(code, "cross_app_fk_forbidden");
                assert!(
                    message.contains("other_app.users"),
                    "message must name the offending target: {message}"
                );
                assert!(
                    message.contains("crosses app boundary"),
                    "message must explain the rule: {message}"
                );
                assert!(
                    hint.as_deref()
                        .map(|h| h.contains("Drop the"))
                        .unwrap_or(false),
                    "hint must include remediation: {hint:?}"
                );
            }
            other => panic!("expected DbError::Configuration, got {other:?}"),
        }
    }

    #[test]
    fn empty_ref_target_is_ignored() {
        // A `"refTarget": ""` is downstream-validated by the schema
        // builder; this hook tolerates it (the empty string has no
        // dot, so the same-app branch accepts).
        let schema = json!({
            "authorId": { "type": "ref", "refTarget": "" }
        });
        assert!(reject_cross_app_fk(&schema, "app_demo").is_ok());
    }

    #[test]
    fn missing_ref_target_is_ignored() {
        let schema = json!({
            "authorId": { "type": "ref" }
        });
        assert!(reject_cross_app_fk(&schema, "app_demo").is_ok());
    }

    #[test]
    fn non_object_schema_is_ignored() {
        // A non-object schema is malformed; the downstream `compute_diff`
        // / `build_create_table` builders reject it with a typed
        // ValidationFailed. This hook does not double-fail.
        let schema = json!("not an object");
        assert!(reject_cross_app_fk(&schema, "app_demo").is_ok());
    }

    #[test]
    fn multiple_refs_all_validated() {
        // First two are fine; third is cross-app — the error must
        // surface and stop the walk at the offender.
        let schema = json!({
            "ownerId": { "type": "ref", "refTarget": "users" },
            "mirrorId": { "type": "ref", "refTarget": "app_demo.snapshots" },
            "audit": { "type": "ref", "refTarget": "other_app.entries" }
        });
        let err = reject_cross_app_fk(&schema, "app_demo")
            .expect_err("third field crosses; must reject");
        match err {
            DbError::Configuration { code, message, .. } => {
                assert_eq!(code, "cross_app_fk_forbidden");
                assert!(
                    message.contains("other_app.entries"),
                    "must name the third field's target: {message}"
                );
            }
            other => panic!("expected DbError::Configuration, got {other:?}"),
        }
    }
}
