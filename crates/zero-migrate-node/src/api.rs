//! Sync, DB-free API surface (design §C.5): load + verify an IR document.
//!
//! These functions run inline on the napi call thread — they touch NO database, so
//! there is no host bridge and no worker thread. They are pure Rust (no napi types),
//! so they compile and test without the Node ABI; `bridge.rs` wraps them as `#[napi]`
//! entrypoints returning a JSON string.
//!
//! The load-verify path is `zero_migrate::model::load::load_ir_document`, which
//! deserializes the closed IR AST, fails closed on an unknown `ir_version`, runs the
//! authoritative structural + schema-confinement validation, enforces ownership
//! against the project registry, and compares the advisory checksum hint (§2.4/§2.7).
//! It is the single DB-free gate an authoring host runs before deploy.

use std::collections::BTreeMap;

use serde::Serialize;
use zero_migrate::model::ir::CURRENT_IR_VERSION;
use zero_migrate::model::load::load_ir_document;
use zero_migrate::model::validate::Dialect;

/// The IR-format version this addon was built against (`ir_version` fail-closed
/// floor, §5.3). Surfaced so a host can pre-check an artifact's version.
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

/// The verdict of [`load_verify`].
#[derive(Debug, Clone, Serialize)]
pub struct LoadVerifyReport {
    /// `true` iff the IR document loaded + validated cleanly.
    pub ok: bool,
    /// The document's `ir_version` (when it deserialized far enough to read it).
    pub ir_version: Option<u32>,
    /// The number of `op.*` operations in the document (when loaded).
    pub op_count: Option<usize>,
    /// A human-readable error when `ok == false` (the first fail-closed rejection).
    pub error: Option<String>,
}

/// Load + verify an IR document (the sync, DB-free deploy gate, §C.5).
///
/// - `ir_json` — the `.ir.json` document bytes (UTF-8).
/// - `deploying_app` — the app id claiming the deploy (ownership is checked against
///   `registry`, not the artifact's self-claim).
/// - `dialect` — `"postgres" | "sqlite" | "mysql"`.
/// - `registry_json` — a JSON object `{ "owner_app": "project_id", ... }`: the
///   project registry ownership is enforced against.
///
/// Returns a [`LoadVerifyReport`]; a malformed document / failed validation / failed
/// ownership yields `ok: false` with the message, never a panic. The confinement
/// scope + policy profile default to **Confined** (the creator profile) — the same
/// fail-closed default `load_ir_document` uses when called with `None`.
#[must_use]
pub fn load_verify(
    ir_json: &str,
    deploying_app: &str,
    dialect: &str,
    registry_json: &str,
) -> LoadVerifyReport {
    let dialect = match parse_dialect(dialect) {
        Ok(d) => d,
        Err(e) => return err_report(e),
    };
    let registry: BTreeMap<String, String> = match serde_json::from_str(registry_json) {
        Ok(m) => m,
        Err(e) => return err_report(format!("registry_json is not a string→string map: {e}")),
    };

    match load_ir_document(ir_json, deploying_app, dialect, &registry, None, None) {
        Ok(ir) => LoadVerifyReport {
            ok: true,
            ir_version: Some(ir.ir_version),
            op_count: Some(ir.ops.len()),
            error: None,
        },
        Err(e) => err_report(e.to_string()),
    }
}

fn err_report(msg: impl Into<String>) -> LoadVerifyReport {
    LoadVerifyReport {
        ok: false,
        ir_version: None,
        op_count: None,
        error: Some(msg.into()),
    }
}

/// Serialize a [`LoadVerifyReport`] to JSON (the napi entrypoint returns this).
#[must_use]
pub fn load_verify_json(
    ir_json: &str,
    deploying_app: &str,
    dialect: &str,
    registry_json: &str,
) -> String {
    let report = load_verify(ir_json, deploying_app, dialect, registry_json);
    serde_json::to_string(&report).unwrap_or_else(|e| {
        format!("{{\"ok\":false,\"error\":\"failed to serialize report: {e}\"}}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_ir_version_is_the_core_floor() {
        assert_eq!(current_ir_version(), CURRENT_IR_VERSION);
    }

    #[test]
    fn unknown_dialect_is_rejected_not_panicked() {
        let r = load_verify("{}", "app_x", "oracle", "{}");
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("unknown dialect"));
    }

    #[test]
    fn malformed_ir_json_yields_ok_false_with_message() {
        let r = load_verify("{not json", "app_x", "postgres", "{}");
        assert!(!r.ok);
        assert!(r.error.is_some());
    }

    #[test]
    fn malformed_registry_is_reported() {
        let r = load_verify("{}", "app_x", "postgres", "[1,2,3]");
        assert!(!r.ok);
        assert!(r.error.unwrap().contains("registry_json"));
    }
}
