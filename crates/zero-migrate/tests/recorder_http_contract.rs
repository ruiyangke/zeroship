#![cfg(feature = "zsv8")]
//! PR4a §8.9.2 HTTP-contract tests for `POST /v1/recorder/record` — the DTO shapes,
//! the §8.8 structured-error mapping, and the 200 body (ir_json + ts_provenance_blob
//! + checksum) driven through the real `RecorderService` + sandboxed child.

#![cfg(target_os = "linux")]

use zero_migrate::frontend::recorder_http::{handle_record, RecordHttpOutcome, RecordHttpRequest};
use zero_migrate::frontend::recorder_service::Authorizer;
use zero_migrate::frontend::RecorderService;

const MIGRATION: &str = r#"
import { table, t } from "@zeroship/migrate";
export function up() { table("http_tbl").create({ columns: { title: t.text().notNull() } }); }
"#;

struct OwnsOne(&'static str, &'static str);
impl Authorizer for OwnsOne {
    fn authorize(&self, token: &str, app_id: &str) -> Result<(), String> {
        if token == self.0 && app_id == self.1 {
            Ok(())
        } else {
            Err("not owned".into())
        }
    }
}

fn svc() -> RecorderService {
    RecorderService::new(Box::new(OwnsOne("pat_x", "app_x")))
}

#[test]
fn post_record_returns_ir_provenance_and_checksum() {
    let req = RecordHttpRequest {
        ts_source: MIGRATION.to_string(),
        app_id: "app_x".into(),
        schema_types_blob: None,
        name: Some("http_m".into()),
    };
    match handle_record(&svc(), Some("pat_x"), &req) {
        RecordHttpOutcome::Ok(resp) => {
            assert!(resp.ir_json.contains("http_tbl"));
            let env: serde_json::Value =
                serde_json::from_str(&resp.ir_json).expect("resolved envelope parses");
            let op = &env["ir"]["ops"][0];
            assert_eq!(op["primaryKey"], serde_json::json!(["id"]));
            let names = op["columns"]
                .as_array()
                .expect("columns")
                .iter()
                .map(|c| c["name"].as_str().expect("column name"))
                .collect::<Vec<_>>();
            assert_eq!(
                &names[..7],
                [
                    "id",
                    "created_at",
                    "updated_at",
                    "created_by",
                    "updated_by",
                    "version",
                    "deleted_at"
                ]
            );
            // ts_provenance_blob is the EXACT source (re-recorded by the §5.1 gate).
            assert_eq!(resp.ts_provenance_blob, MIGRATION);
            // checksum is a non-empty sha256 hex.
            assert_eq!(resp.checksum.len(), 64, "checksum: {}", resp.checksum);
            assert!(resp.checksum.chars().all(|c| c.is_ascii_hexdigit()));
        }
        RecordHttpOutcome::Err(e) => panic!("expected 200, got {e:?}"),
    }
}

#[test]
fn missing_bearer_is_401() {
    let req = RecordHttpRequest {
        ts_source: MIGRATION.to_string(),
        app_id: "app_x".into(),
        schema_types_blob: None,
        name: None,
    };
    match handle_record(&svc(), None, &req) {
        RecordHttpOutcome::Err(e) => {
            assert_eq!(e.http_status, 401);
            assert_eq!(e.code, "RECORDER_UNAUTHORIZED");
        }
        RecordHttpOutcome::Ok(_) => panic!("missing bearer must 401"),
    }
}

#[test]
fn ownership_mismatch_is_403() {
    let req = RecordHttpRequest {
        ts_source: MIGRATION.to_string(),
        app_id: "app_other".into(), // pat_x does not own app_other
        schema_types_blob: None,
        name: None,
    };
    match handle_record(&svc(), Some("pat_x"), &req) {
        RecordHttpOutcome::Err(e) => {
            assert_eq!(e.http_status, 403);
            assert_eq!(e.code, "RECORDER_UNAUTHORIZED");
            assert!(!e.retryable);
        }
        RecordHttpOutcome::Ok(_) => panic!("ownership mismatch must 403"),
    }
}

#[test]
fn authoring_reject_is_422_not_retryable() {
    // A migration that calls an op OUTSIDE up() (module top level) -> the recorder
    // surfaces an eval error -> 422, not retryable (an authoring bug, not a transient).
    let bad = r#"
        import { table, t } from "@zeroship/migrate";
        table("oops").create({ columns: { id: t.int().notNull() } }); // top-level call
        export function up() {}
    "#;
    let req = RecordHttpRequest {
        ts_source: bad.to_string(),
        app_id: "app_x".into(),
        schema_types_blob: None,
        name: None,
    };
    match handle_record(&svc(), Some("pat_x"), &req) {
        RecordHttpOutcome::Err(e) => {
            assert_eq!(e.http_status, 422, "got {e:?}");
            assert!(!e.retryable);
            // The REAL authoring error must surface — not the misleading
            // "recorded IR did not re-parse for checksum" the inner {ok:false}
            // accidentally produced (PR4a code-critic MED #3).
            assert_eq!(e.code, "RECORD_EVAL_ERROR", "got {e:?}");
            assert!(
                e.message.contains("recorder") || e.message.contains("OP_OUTSIDE")
                    || e.message.to_lowercase().contains("outside"),
                "the 422 must carry the real recorder error, got message: {}",
                e.message
            );
            assert!(
                !e.message.contains("did not re-parse"),
                "the 422 must NOT be the misleading checksum re-parse message: {}",
                e.message
            );
        }
        RecordHttpOutcome::Ok(_) => panic!("top-level op must be an authoring reject"),
    }
}

#[test]
fn up_throw_surfaces_real_error_not_success() {
    // up() that throws -> op_recorder.js catches it and writes an inner {ok:false,error}
    // envelope. The child must surface that as a FAILURE (eval error) at the service
    // boundary carrying the real throw text — NOT an outer ok:true wrapping an inner
    // failure (PR4a code-critic MED #3).
    let throws = r#"
        export function up() { throw new Error("CREATOR_BUG_xyz"); }
    "#;
    let req = RecordHttpRequest {
        ts_source: throws.to_string(),
        app_id: "app_x".into(),
        schema_types_blob: None,
        name: None,
    };
    match handle_record(&svc(), Some("pat_x"), &req) {
        RecordHttpOutcome::Err(e) => {
            assert_eq!(e.http_status, 422, "throw is an authoring reject; got {e:?}");
            assert_eq!(e.code, "RECORD_EVAL_ERROR", "got {e:?}");
            assert!(
                e.message.contains("CREATOR_BUG_xyz"),
                "the real throw text must surface, got: {}",
                e.message
            );
            assert!(
                !e.message.contains("did not re-parse"),
                "must NOT be the misleading checksum re-parse message: {}",
                e.message
            );
        }
        RecordHttpOutcome::Ok(_) => panic!("a throwing up() must surface as a failure, not success"),
    }
}

#[test]
fn oversized_app_id_is_rejected_before_spawn() {
    // app_id is client-controlled; bound it symmetrically with ts_source/name (PR4a
    // code-critic LOW). A hostile oversized app_id must be 413'd at the HTTP boundary
    // BEFORE svc.record() reaches the authorizer / spawns a child.
    let huge_id = "a".repeat(zero_migrate::frontend::recorder_http::MAX_APP_ID_BYTES + 1);
    let req = RecordHttpRequest {
        ts_source: MIGRATION.to_string(),
        app_id: huge_id,
        schema_types_blob: None,
        name: None,
    };
    match handle_record(&svc(), Some("pat_x"), &req) {
        RecordHttpOutcome::Err(e) => {
            assert_eq!(e.http_status, 413, "oversized app_id must be 413; got {e:?}");
            assert_eq!(e.code, "RECORDER_REQUEST_TOO_LARGE", "got {e:?}");
            assert!(!e.retryable);
            assert!(e.message.contains("app_id"), "message: {}", e.message);
        }
        RecordHttpOutcome::Ok(_) => panic!("oversized app_id must be rejected, not recorded"),
    }
}

#[test]
fn oversized_schema_types_blob_is_rejected_before_spawn() {
    // schema_types_blob is delivered in-memory to the child and reaches v8::String::new
    // (a panic vector above V8's max string length). Bound it at the HTTP boundary
    // symmetric with ts_source (PR4a code-critic LOW) -> 413 before a child is spawned.
    let huge_blob = "a".repeat(zero_migrate::frontend::recorder_http::MAX_SCHEMA_TYPES_BYTES + 1);
    let req = RecordHttpRequest {
        ts_source: MIGRATION.to_string(),
        app_id: "app_x".into(),
        schema_types_blob: Some(huge_blob),
        name: None,
    };
    match handle_record(&svc(), Some("pat_x"), &req) {
        RecordHttpOutcome::Err(e) => {
            assert_eq!(
                e.http_status, 413,
                "oversized schema_types_blob must be 413; got {e:?}"
            );
            assert_eq!(e.code, "RECORDER_REQUEST_TOO_LARGE", "got {e:?}");
            assert!(!e.retryable);
            assert!(
                e.message.contains("schema_types_blob"),
                "message: {}",
                e.message
            );
        }
        RecordHttpOutcome::Ok(_) => {
            panic!("oversized schema_types_blob must be rejected, not recorded")
        }
    }
}

#[test]
fn oversized_ts_source_is_rejected_before_spawn() {
    // A hostile >max-bytes ts_source must be rejected at the HTTP boundary (413) BEFORE
    // spawning a child / reaching v8::String::new (which panics above V8's max string
    // length). PR4a code-critic MED #5. We use a small synthetic cap via the public
    // limit to keep the test cheap.
    let huge = "a".repeat(zero_migrate::frontend::recorder_http::MAX_TS_SOURCE_BYTES + 1);
    let req = RecordHttpRequest {
        ts_source: huge,
        app_id: "app_x".into(),
        schema_types_blob: None,
        name: None,
    };
    match handle_record(&svc(), Some("pat_x"), &req) {
        RecordHttpOutcome::Err(e) => {
            assert_eq!(e.http_status, 413, "oversized source must be 413; got {e:?}");
            assert!(!e.retryable);
        }
        RecordHttpOutcome::Ok(_) => panic!("oversized ts_source must be rejected, not recorded"),
    }
}
