//! PR4a §8.9.2 HTTP-contract tests for `POST /v1/recorder/record` — the DTO shapes,
//! the §8.8 structured-error mapping, and the 200 body (ir_json + ts_provenance_blob
//! + checksum) driven through the real `RecorderService` + sandboxed child.

#![cfg(target_os = "linux")]

use zeroship_migrate_js::recorder_http::{handle_record, RecordHttpOutcome, RecordHttpRequest};
use zeroship_migrate_js::recorder_service::Authorizer;
use zeroship_migrate_js::RecorderService;

const MIGRATION: &str = r#"
import { createTable } from "@zeroship/migrate";
export function up() { createTable("http_tbl", [{ name: "id", type: "int", nullable: false }]); }
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
        import { createTable } from "@zeroship/migrate";
        createTable("oops", [{ name: "id", type: "int", nullable: false }]); // top-level call
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
        }
        RecordHttpOutcome::Ok(_) => panic!("top-level op must be an authoring reject"),
    }
}
