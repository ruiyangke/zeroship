//! Cross-app foreign-key parse-time check.
//!
//! Rejects any `t.ref("<other_app>.<collection>")` reference whose
//! `<other_app>` prefix differs from the calling app's id. The check
//! exists so creators cannot accidentally (or intentionally) build a
//! schema whose physical FK crosses a per-app SQLite file boundary —
//! see design `docs/proposals/db-system-design.md` §18 Q1 for the
//! ATTACH-isolation reasoning. Although the motivation is SQLite-
//! specific (PG can express cross-schema FKs natively), the platform
//! policy is "every FK stays inside one app's namespace" on both
//! backends so the data-isolation invariant is identical regardless of
//! which storage engine an app is running on.
//!
//! **Why not under `backend/sqlite/`?** P1 PR 5 lifted this module out
//! of the old SQLite-only subtree: the rule applies on
//! the PG build too (where the orchestrator's `register_model` pipeline
//! enforces it for every deploy), so gating the file behind the
//! optional `sqlite` Cargo feature would mean PG-only builds never run
//! the check. The design-lineage signal that the rule originated in the
//! SQLite ATTACH design lives in the rustdoc here; the implementation
//! is engine-agnostic (pure-Rust JSON walk, no SQL).
//!
//! Hook point: [`crate::orchestrator::register_model::bootstrap::build_ctx`]
//! invokes [`reject_cross_app_fk`] after the strictness read and BEFORE
//! the advisory-lock acquire. The order is load-bearing: rejecting a
//! malformed schema at parse time means we never take the per-app
//! `register_model` lock for a deploy that will fail validation, so
//! concurrent deploys for the same app stay un-blocked.

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
/// The `register_model` pipeline calls this hook once per collection
/// (one call per `bootstrap::build_ctx` invocation, with the per-
/// collection field set), so the walk is O(fields-in-one-collection).
///
/// **Error envelope** matches the contract in `docs/proposals/p1-sqlite-implementation-plan.md`
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
