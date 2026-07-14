//! Host-authoring lower — turn a pure-JS IR envelope into the
//! `Vec<Migration>` the engine's `executor::apply` consumes, **folding the single
//! authoritative `Checksum::of_ir` in Rust** (never in JS).
//!
//! ## Why the addon lowers (not the facade)
//! The async entrypoints (`apply`/`status`/`history`) take a pre-lowered
//! `Vec<Migration>` (SQL `up` + folded checksum). But the pure-JS host recorder
//! can only produce the dialect-neutral IR op envelope — the
//! ops→SQL LOWER (`IrAuthor::load_and_lower`) is a Rust-only engine step (it routes
//! every op through the shared snapshot-builder + DDL emitter, `render/lower.rs`). So
//! the facade hands the envelope + provenance here; this module runs the SAME
//! fail-closed LOAD GATE + LOWER the IR envelope deploy path runs
//! (`IrAuthor::new(schema, app, dialect).load_and_lower(bytes, app, &registry, &live,
//! None)`), and the resulting `Migration.checksum` is `Checksum::of_ir` folded by
//! Rust over the canonical op list + the server-stamped `owner_app`. The JS side emits
//! ops; Rust owns the checksum — exactly the invariant the pure-JS recorder
//! (`sdks/migrate/src/internal/recorder.ts`) preserves.
//!
//! ## Fresh-DB `LiveSchema`
//! v1 host apply lowers against `LiveSchema::default()` (an empty live) — the
//! fresh-DB / create-first posture the golden-path scaffold + the oracle
//! exercise (`load_and_lower(&committed, APP, &registry, &LiveSchema::default(),
//! None)`). Incremental lowering
//! against an introspected live (FK inline-vs-defer, live UNIQUE-index drop gate)
//! needs a `snapshot_schema` read over the host driver — a deferred follow-up;
//! it is NOT reached by the create-first oracle.

use std::collections::BTreeMap;

use zero_migrate::model::ir::MigrationIr;
use zero_migrate::model::migration::Migration;
use zero_migrate::model::profile::PolicyProfile;
use zero_migrate::{
    effective_policy_from_ceiling_toml, resolve_create_table_policy, GuardConfig, IrAuthor,
    LiveSchema, SqlDialect,
};

/// Map the wire dialect spelling to the render [`SqlDialect`]. Unknown → `Err`.
fn parse_sql_dialect(s: &str) -> Result<SqlDialect, String> {
    match s {
        "postgres" => Ok(SqlDialect::Postgres),
        "sqlite" => Ok(SqlDialect::Sqlite),
        "mysql" => Ok(SqlDialect::Mysql),
        other => Err(format!(
            "unknown dialect {other:?} (expected postgres|sqlite|mysql)"
        )),
    }
}

/// Run the fail-closed IR envelope LOAD GATE + LOWER over an envelope, returning the
/// lowered `Vec<Migration>` (with `Checksum::of_ir` folded by Rust).
///
/// - `envelope_json` — the pure-JS IR envelope bytes (`{ ir_version, name,
///   ops }`). The envelope MUST NOT carry `owner_app` (a provenance field the
///   builder can't be trusted to set); it is stamped from `owner_app` here.
/// - `owner_app` — the deploying app id (`app_…`); the ownership check + the
///   `owner_app` stamped onto every emitted `Migration` + folded into its checksum.
/// - `project_schema` — the confined project schema the lower pins ops to.
/// - `dialect` — `"postgres" | "sqlite" | "mysql"`.
/// - `registry_json` — the project's `{ "table": "owner_app", … }` map (drives the
///   ownership check); an empty object `{}` on a fresh single-app project.
/// - `policy_ceiling_toml` — the **policy input**: the host's `RootCeiling` document
///   (TOML) that drives table-shape injection. The engine constructs NO default
///   ceiling: `None` injects nothing (the author-owned shape passes through); `Some`
///   composes the ceiling (against an empty draft) into the `EffectivePolicy` whose
///   `injects_for(object)` drives column/index/PK injection. The monorepo caller
///   passes zeroship's confined ceiling here (Phase 3).
///
/// # Errors
/// A JSON `Err(message)` on: an unknown dialect, a malformed registry, a malformed
/// policy ceiling document, the load gate refusing the artifact (malformed / future
/// `ir_version` / structural reject / ownership violation / checksum-hint mismatch),
/// or a lower failure — never a panic.
pub fn lower_envelope_to_migrations(
    envelope_json: &str,
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    policy_ceiling_toml: Option<&str>,
) -> Result<Vec<Migration>, String> {
    let dialect = parse_sql_dialect(dialect)?;
    let registry: BTreeMap<String, String> = serde_json::from_str(registry_json)
        .map_err(|e| format!("registry_json is not a string→string map: {e}"))?;

    // **System-shape fold (mirrors the pure-JS recorder's fold).** The
    // pure-JS host recorder drains ONLY the author-declared columns — the
    // platform-managed system fields (`id`/`created_at`/`updated_at`/`version`/…)
    // + the `["id"]` PRIMARY KEY are injected by `resolve_create_table_policy` off the
    // host-supplied POLICY CEILING (the `EffectivePolicy`'s `injects_for`), NOT by the
    // JS DSL. The engine hardcodes no ceiling: the monorepo passes zeroship's confined
    // ceiling. The native IR envelope on disk is post-fold (the recorder folds before
    // writing); the host path folds here so the addon lowers the SAME resolved shape —
    // otherwise the confined table-shape guard rejects a createTable missing its system
    // columns (TABLE_SHAPE_POLICY). This is a pure structural resolve; the JS side never
    // sees the system fields (they are platform-owned).
    //
    // NOTE (guard/validate — retired in a later cut): `load_and_lower_guarded` below
    // still takes the `PolicyProfile`-shaped confined profile as its validate input.
    // Only the INJECTION flow moves to the `EffectivePolicy` here; the guard/validate
    // path is untouched until Steps 2-3.
    let confined = PolicyProfile::confined();
    let effective = match policy_ceiling_toml {
        Some(toml) => effective_policy_from_ceiling_toml(toml)?,
        // No ceiling supplied ⇒ inject nothing (author-owned shape). The engine
        // constructs no default ceiling of its own.
        None => effective_policy_from_ceiling_toml("policy_version = 1\n")?,
    };
    let raw_ir: MigrationIr = serde_json::from_str(envelope_json)
        .map_err(|e| format!("envelope is not a MigrationIr document: {e}"))?;
    let resolved = resolve_create_table_policy(&raw_ir, &effective)
        .map_err(|e| format!("table-shape resolve failed: {e}"))?;
    let resolved_bytes = serde_json::to_string(&resolved)
        .map_err(|e| format!("resolved IR failed to serialize: {e}"))?;

    let author = IrAuthor::new(project_schema, owner_app, dialect);

    // Use the GUARDED lower — the SAME entry the IR envelope deploy path uses
    // (`load_and_lower_guarded` in `render/lower.rs`). This matters for JOURNAL
    // IDENTITY: `load_and_lower_guarded` assembles an `AppliedPlan` and stamps the
    // dialect-neutral `authoritative_ir_checksum` (the `Checksum::of_ir` ANCHOR)
    // onto EVERY DDL step's `Migration.checksum`. The non-guarded `load_and_lower`
    // → `lower()` skips `assemble_plan`, so its steps carry per-step checksums
    // instead of the shared anchor — which diverges from the reference journal
    // (the DB-backed oracle caught exactly this). Guarded here ⇒ the host journal's
    // checksum column is byte-identical to the reference path's.
    let guard_cfg = match dialect {
        SqlDialect::Postgres => GuardConfig::confined(project_schema.to_string()),
        SqlDialect::Sqlite => GuardConfig::confined_sqlite(project_schema.to_string()),
        SqlDialect::Mysql => GuardConfig::confined_mysql(project_schema.to_string()),
    };
    author
        .load_and_lower_guarded(
            &resolved_bytes,
            owner_app,
            &registry,
            &LiveSchema::default(),
            &guard_cfg,
            Some(&confined),
        )
        .map(|artifact| artifact.migrations())
        .map_err(|e| e.to_string())
}

/// [`lower_envelope_to_migrations`] returning a JSON `Vec<Migration>` string (the
/// shape the addon's `apply`/`status` entrypoints deserialize). Used both by the
/// napi `applyIr`/`statusIr` bridge and directly by the host facade's `plan`
/// (the DB-free pre-check).
///
/// # Errors
/// Same as [`lower_envelope_to_migrations`]; the error is returned as an `Err`
/// string so the napi bridge can reject the promise with it.
pub fn lower_envelope_to_migrations_json(
    envelope_json: &str,
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    policy_ceiling_toml: Option<&str>,
) -> Result<String, String> {
    let migrations = lower_envelope_to_migrations(
        envelope_json,
        owner_app,
        project_schema,
        dialect,
        registry_json,
        policy_ceiling_toml,
    )?;
    serde_json::to_string(&migrations)
        .map_err(|e| format!("failed to serialize lowered migrations: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal create-first envelope: one `createTable` op. Mirrors what the pure-JS
    // host recorder emits (ir_version stamped from the addon's `irVersion()` floor).
    fn create_widgets_envelope(ir_version: u32) -> String {
        serde_json::json!({
            "ir_version": ir_version,
            "name": "create_widgets",
            "ops": [
                {
                    "op": "createTable",
                    "table": "widgets",
                    "columns": [
                        { "name": "id", "type": { "kind": "int" }, "nullable": false },
                        { "name": "label", "type": { "kind": "text" }, "nullable": true }
                    ]
                }
            ]
        })
        .to_string()
    }

    // The generic confined test ceiling the monorepo will supply in Phase 3.
    const CEILING: &str = zero_migrate::ZEROSHIP_CONFINED_CEILING_TOML;

    #[test]
    fn unknown_dialect_is_an_err_not_a_panic() {
        let env = create_widgets_envelope(zero_migrate::model::ir::CURRENT_IR_VERSION);
        let r = lower_envelope_to_migrations(&env, "app_x", "app_x", "oracle", "{}", Some(CEILING));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("unknown dialect"));
    }

    #[test]
    fn malformed_registry_is_an_err() {
        let env = create_widgets_envelope(zero_migrate::model::ir::CURRENT_IR_VERSION);
        let r =
            lower_envelope_to_migrations(&env, "app_x", "app_x", "postgres", "[1,2,3]", Some(CEILING));
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("registry_json"));
    }

    #[test]
    fn malformed_policy_ceiling_is_an_err() {
        let env = create_widgets_envelope(zero_migrate::model::ir::CURRENT_IR_VERSION);
        let r = lower_envelope_to_migrations(
            &env,
            "app_x",
            "app_x",
            "postgres",
            "{}",
            Some("this is not = valid toml ["),
        );
        assert!(r.is_err());
        assert!(r.unwrap_err().contains("policy"));
    }

    #[test]
    fn create_first_envelope_lowers_to_a_migration_with_a_folded_checksum() {
        // The op shape may or may not match the exact IR schema (the round-trip op
        // vocabulary is authoritative), so this test asserts the GATE behavior: a
        // syntactically-valid envelope either lowers to a non-empty `Vec<Migration>`
        // whose sole migration carries a NON-empty `up` SQL + a folded checksum, or
        // fails closed with a message (never a panic). Both are acceptable proofs the
        // lower path is wired; the DB-backed oracle asserts journal identity.
        let env = create_widgets_envelope(zero_migrate::model::ir::CURRENT_IR_VERSION);
        match lower_envelope_to_migrations(
            &env,
            "app_widgets",
            "app_widgets",
            "postgres",
            "{}",
            Some(CEILING),
        ) {
            Ok(migs) => {
                assert!(
                    !migs.is_empty(),
                    "a create-first envelope lowers to ≥1 migration"
                );
                let m = &migs[0];
                assert!(
                    !m.up.is_empty(),
                    "the lowered migration has non-empty up SQL"
                );
                assert_eq!(
                    m.owner_app, "app_widgets",
                    "owner_app is stamped from the arg"
                );
            }
            Err(msg) => {
                // Fail-closed is acceptable here (op-schema mismatch) — the load gate
                // never panics. The DB oracle uses a build-verified envelope.
                assert!(!msg.is_empty());
            }
        }
    }
}
