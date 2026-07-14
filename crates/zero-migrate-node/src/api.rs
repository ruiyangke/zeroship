//! Sync, DB-free API surface: load + verify an IR document.
//!
//! These functions run inline on the napi call thread — they touch NO database, so
//! there is no host bridge and no worker thread. They build the typed
//! [`LoadVerifyReply`] (the single source-of-truth DTO in [`crate::wire`]) directly;
//! `bridge.rs` returns it as the `loadVerify` `#[napi]` entrypoint (no JSON string).
//! The core of this module is pure Rust (no napi types) — the fold + the tests
//! compile without the Node ABI.
//!
//! The load-verify path is `zero_migrate::model::load::load_ir_document`, which
//! deserializes the closed IR AST, fails closed on an unknown `ir_version`, runs the
//! authoritative structural + schema-confinement validation, enforces ownership
//! against the project registry, and compares the advisory checksum hint.
//! It is the single DB-free gate an authoring host runs before deploy.

use std::collections::HashMap;

use zero_migrate::model::ir::{MigrationIr, Op, CURRENT_IR_VERSION};
use zero_migrate::model::load::load_ir_document;
use zero_migrate::model::profile::PolicyProfile;
use zero_migrate::model::validate::Dialect;
use zero_migrate::render::declarative::CollectionDescriptor;
use zero_migrate::{
    render_artifacts, render_artifacts_from_descriptors, resolve_create_table_policy,
    DEFAULT_PROJECT_SCHEMA,
};

use crate::wire::{GenArtifactsReply, LoadVerifyReply};

/// The IR-format version this addon was built against (`ir_version` fail-closed
/// floor). Surfaced so a host can pre-check an artifact's version.
#[must_use]
pub const fn current_ir_version() -> u32 {
    CURRENT_IR_VERSION
}

/// Map the wire dialect spelling to [`Dialect`]. Unknown → `Err`.
fn parse_dialect(s: &str) -> Result<Dialect, String> {
    match s {
        "postgres" => Ok(Dialect::Postgres),
        "sqlite" => Ok(Dialect::Sqlite),
        "mysql" => Ok(Dialect::Mysql),
        other => Err(format!(
            "unknown dialect {other:?} (expected postgres|sqlite|mysql)"
        )),
    }
}

/// Load + verify an IR document (the sync, DB-free deploy gate).
///
/// - `envelope_json` — the IR envelope document bytes (UTF-8).
/// - `deploying_app` — the app id claiming the deploy (ownership is checked against
///   `registry`, not the artifact's self-claim).
/// - `dialect` — `"postgres" | "sqlite" | "mysql"`.
/// - `registry` — the already-parsed `{ "owner_app": "project_id", ... }` map (it
///   crosses the napi boundary typed as a `Record<string,string>`, not a JSON
///   string): the project registry ownership is enforced against.
///
/// Returns a [`LoadVerifyReply`]; a malformed document / failed validation / failed
/// ownership yields `ok: false` with the message, never a panic. The confinement
/// scope + policy profile default to **Confined** (the creator profile) — the same
/// fail-closed default `load_ir_document` uses when called with `None`.
#[must_use]
pub fn load_verify(
    envelope_json: &str,
    deploying_app: &str,
    dialect: &str,
    registry: &HashMap<String, String>,
) -> LoadVerifyReply {
    let dialect = match parse_dialect(dialect) {
        Ok(d) => d,
        Err(e) => return err_report(e),
    };
    // `load_ir_document` takes a `BTreeMap` (deterministic ownership iteration).
    let registry: std::collections::BTreeMap<String, String> = registry
        .iter()
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();

    match load_ir_document(envelope_json, deploying_app, dialect, &registry, None, None) {
        Ok(ir) => LoadVerifyReply {
            ok: true,
            ir_version: Some(ir.ir_version),
            op_count: Some(ir.ops.len() as u32),
            error: None,
        },
        Err(e) => err_report(e.to_string()),
    }
}

/// Render the two schema artifacts from the GENERATED source: a set of IR
/// envelopes (`{ ir_version, name, ops }`). Each envelope's `ops` are concatenated
/// in order and folded through the shared renderer.
///
/// `project_schema` defaults to [`DEFAULT_PROJECT_SCHEMA`] when `None`.
///
/// # System-shape resolution (mirrors `lower.rs`)
/// The pure-JS recorder emits RAW, author-only `createTable` ops — it drains ONLY
/// the author-declared columns; the platform-managed system fields
/// (`id`/`created_at`/`updated_at`/`created_by`/`updated_by`/`version`/`deleted_at`)
/// + the `["id"]` PRIMARY KEY + the system indexes are injected by
/// [`resolve_create_table_policy`] under the **Confined** creator profile, NOT by the
/// JS DSL (exactly the fold `crate::lower::lower_envelope_to_migrations` runs before
/// it lowers). So each envelope is resolved here — per-envelope, as its own
/// [`MigrationIr`], with the SAME `PolicyProfile::confined()` the apply-side lower
/// uses — BEFORE the ops are concatenated and folded. Resolution stays OUT of the
/// shared [`render_artifacts`] tail: the manual descriptor path already resolves
/// inside `descriptors_to_create_ops`, so double-resolving there would be wrong. This
/// keeps the generated + manual paths byte-identical (both feed RESOLVED ops to the
/// one renderer).
///
/// Returns a [`GenArtifactsReply`]; a malformed envelope / a table-shape resolve
/// failure / an incoherent op stream yields `ok: false` with the message, never a
/// panic.
#[must_use]
pub fn gen_artifacts_from_envelopes(
    envelopes: &[serde_json::Value],
    project_schema: Option<&str>,
) -> GenArtifactsReply {
    let schema = project_schema.unwrap_or(DEFAULT_PROJECT_SCHEMA);
    let confined = PolicyProfile::confined();
    let mut ops: Vec<Op> = Vec::new();
    for (i, env) in envelopes.iter().enumerate() {
        let raw_ir: MigrationIr = match serde_json::from_value(env.clone()) {
            Ok(ir) => ir,
            Err(e) => {
                return gen_err(format!("envelope[{i}] is not a valid IR document: {e}"));
            }
        };
        // Resolve the confined system shape (system columns + [id] PK + system
        // indexes) BEFORE folding — the JS recorder emits author-only ops.
        let resolved = match resolve_create_table_policy(&raw_ir, &confined) {
            Ok(ir) => ir,
            Err(e) => {
                return gen_err(format!("envelope[{i}] table-shape resolve failed: {e}"));
            }
        };
        ops.extend(resolved.ops);
    }
    match render_artifacts(&ops, schema) {
        Ok(a) => gen_ok(a),
        Err(e) => gen_err(e.to_string()),
    }
}

/// Render the two schema artifacts from the MANUAL source: a declared
/// `CollectionDescriptor` set. The descriptors are turned into `createTable` ops via
/// the producer and folded through the SAME renderer tail — so the manual output is
/// byte-identical to the generated output for an equivalent schema.
///
/// `project_schema` defaults to [`DEFAULT_PROJECT_SCHEMA`] when `None`.
///
/// Returns a [`GenArtifactsReply`]; a descriptor set the producer/fold refuses
/// yields `ok: false` with the message, never a panic.
#[must_use]
pub fn gen_artifacts_from_descriptors(
    descriptors: &[CollectionDescriptor],
    project_schema: Option<&str>,
) -> GenArtifactsReply {
    let schema = project_schema.unwrap_or(DEFAULT_PROJECT_SCHEMA);
    match render_artifacts_from_descriptors(descriptors, schema) {
        Ok(a) => gen_ok(a),
        Err(e) => gen_err(e.to_string()),
    }
}

fn gen_ok(artifacts: zero_migrate::GeneratedArtifacts) -> GenArtifactsReply {
    GenArtifactsReply {
        ok: true,
        env_db_ts: Some(artifacts.env_db_ts),
        runtime_json: Some(artifacts.runtime_json),
        error: None,
    }
}

fn gen_err(msg: impl Into<String>) -> GenArtifactsReply {
    GenArtifactsReply {
        ok: false,
        env_db_ts: None,
        runtime_json: None,
        error: Some(msg.into()),
    }
}

fn err_report(msg: impl Into<String>) -> LoadVerifyReply {
    LoadVerifyReply {
        ok: false,
        ir_version: None,
        op_count: None,
        error: Some(msg.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_ir_version_is_the_core_floor() {
        assert_eq!(current_ir_version(), CURRENT_IR_VERSION);
    }

    fn empty_registry() -> HashMap<String, String> {
        HashMap::new()
    }

    #[test]
    fn unknown_dialect_is_rejected_not_panicked() {
        let r = load_verify("{}", "app_x", "oracle", &empty_registry());
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("unknown dialect"));
    }

    #[test]
    fn malformed_envelope_json_yields_ok_false_with_message() {
        let r = load_verify("{not json", "app_x", "postgres", &empty_registry());
        assert!(!r.ok);
        assert!(r.error.is_some());
    }

    #[test]
    fn gen_artifacts_from_envelopes_renders_both_files() {
        // A minimal generated source: one create-table envelope carrying ONLY the
        // author column — exactly the RAW shape the pure-JS recorder emits (no system
        // columns). `gen_artifacts_from_envelopes` resolves the confined system shape
        // before folding.
        let envelope = serde_json::json!({
            "ir_version": current_ir_version(),
            "name": "create_widgets",
            "ops": [{
                "op": "createTable",
                "name": "widgets",
                "columns": [{ "name": "label", "type": "string" }],
                "primaryKey": null
            }]
        });
        let reply = gen_artifacts_from_envelopes(&[envelope], None);
        assert!(reply.ok, "render ok: {:?}", reply.error);
        let runtime = reply.runtime_json.expect("runtime json");
        let ts = reply.env_db_ts.expect("env.db.ts");
        assert!(runtime.contains("\"version\": 1"), "v1 descriptor: {runtime}");
        assert!(runtime.contains("\"widgets\""), "carries the table: {runtime}");
        assert!(
            ts.contains("label: t.string(),"),
            "env.db.ts renders the builder chain: {ts}"
        );
    }

    #[test]
    fn gen_artifacts_from_raw_author_only_envelope_injects_system_fields_and_indexes() {
        // WALL 1 regression: the pure-JS recorder emits RAW author-only createTable ops
        // (NO system columns). `gen_artifacts_from_envelopes` MUST resolve the confined
        // system shape (7 system fields + [id] PK + 3 system indexes) before folding —
        // otherwise the generated descriptor is missing them entirely. Pre-fix this
        // path fed the raw ops straight to `render_artifacts`, so none of the 7 system
        // fields (nor the system indexes) appeared. Model the envelope EXACTLY as the
        // recorder writes it: a `createTable` with only the author column, no primary
        // key, no system cols.
        let envelope = serde_json::json!({
            "ir_version": current_ir_version(),
            "name": "create_widgets",
            "ops": [{
                "op": "createTable",
                "name": "widgets",
                "columns": [{ "name": "label", "type": "string" }],
                "primaryKey": null
            }]
        });
        let reply = gen_artifacts_from_envelopes(&[envelope], None);
        assert!(reply.ok, "render ok: {:?}", reply.error);
        let runtime = reply.runtime_json.expect("runtime json");
        let v: serde_json::Value = serde_json::from_str(&runtime).expect("runtime json parses");

        // All 7 platform system fields are present in the folded descriptor.
        let fields = &v["collections"]["widgets"]["fields"];
        for sys in [
            "id",
            "created_at",
            "updated_at",
            "created_by",
            "updated_by",
            "version",
            "deleted_at",
        ] {
            assert!(
                fields.get(sys).is_some(),
                "the RESOLVED generated descriptor must carry system field `{sys}`: {fields}"
            );
        }
        assert!(
            fields.get("label").is_some(),
            "the author column survives resolution: {fields}"
        );

        // The 3 confined system indexes (deleted_at / updated_at / created_by) are
        // injected too.
        let idx_fields: Vec<String> = v["collections"]["widgets"]["indexes"]
            .as_array()
            .expect("indexes array")
            .iter()
            .filter_map(|i| i["fields"][0].as_str().map(str::to_string))
            .collect();
        for sys_idx in ["deleted_at", "updated_at", "created_by"] {
            assert!(
                idx_fields.iter().any(|f| f == sys_idx),
                "the resolved descriptor carries the `{sys_idx}` system index: {idx_fields:?}"
            );
        }
    }

    #[test]
    fn gen_artifacts_from_a_malformed_envelope_fails_soft() {
        let bad = serde_json::json!({ "ir_version": current_ir_version(), "ops": "not-an-array" });
        let reply = gen_artifacts_from_envelopes(&[bad], None);
        assert!(!reply.ok);
        assert!(reply.error.is_some());
        assert!(reply.runtime_json.is_none());
    }

    #[test]
    fn gen_artifacts_from_descriptors_renders_both_files() {
        use zero_migrate::render::declarative::{CollectionDescriptor, FieldDescriptor};
        let descriptor = CollectionDescriptor {
            name: "widgets".to_string(),
            owner_app: "app_x".to_string(),
            fields: vec![FieldDescriptor {
                name: "label".to_string(),
                ty: "string".to_string(),
                ..Default::default()
            }],
            indexes: Vec::new(),
            runtime_options: Default::default(),
        };
        let reply = gen_artifacts_from_descriptors(&[descriptor], None);
        assert!(reply.ok, "render ok: {:?}", reply.error);
        assert!(reply.runtime_json.unwrap().contains("\"version\": 1"));
        assert!(reply.env_db_ts.unwrap().contains("label: t.string(),"));
    }

    #[test]
    fn a_populated_registry_is_accepted_typed_not_a_json_string() {
        // The registry crosses the boundary TYPED (a `Record<string,string>`), so a
        // populated map is a plain `HashMap` — no JSON string, no parse step. A
        // malformed IR envelope still fails closed with a message (never a panic).
        let mut reg = HashMap::new();
        reg.insert("widgets".to_string(), "app_x".to_string());
        let r = load_verify("{not json", "app_x", "postgres", &reg);
        assert!(!r.ok);
        assert!(r.error.is_some());
    }
}
