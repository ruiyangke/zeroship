//! PR4 deliverable A3 / test #3 — the two record paths + the §8.9.2
//! recorder-unreachable FALLBACK:
//!
//! - A HOSTED thin client that returns a RETRYABLE `StructuredError`
//!   (recorder-unreachable / 503-class) makes the build FALL BACK to LOCAL recording
//!   — NOT a build failure — and still produces transient canonical IR with the
//!   correct typed-value checksum.
//! - A HOSTED thin client that returns a NON-retryable authoring reject (422) IS
//!   surfaced as a build error (no silent fallback).
//! - The LOCAL record path and the HOSTED happy path produce the SAME typed-value
//!   checksum (cross-path determinism, §8.9.2).
//!
//! Faithful: the LOCAL path drives the REAL sandboxed recorder child. The recorder
//! child must be built (`cargo build -p zeroship-migrate --bins`); the test
//! asserts the binary exists with a clear message.

use std::fs;
use std::path::Path;

use zeroship_migrate::frontend::recorder_protocol::MAX_TS_SOURCE_BYTES;
use zeroship_migrate::frontend::recorder_http::StructuredError;
use zeroship_migrate::frontend::recorder_service::recorder_child_path;
use zeroship_migrate::frontend::{
    build_migrations, BuildError, RecordPath, RecordVia, RecorderClient, ResourceBudget,
};

const OWNER: &str = "app_paths";

const MIG_TS: &str = r#"
import { table, t } from "@zeroship/migrate";
export function up() {
  table("widgets").create({
    columns: {
      id: t.uuid().notNull().primaryKey().default({ fn: "genRandomUuid" }),
      title: t.text().notNull(),
    },
  });
  table("widgets").column("qty").add({ type: t.integer() });
}
"#;

fn assert_child_built() {
    let bin = recorder_child_path();
    assert!(
        bin.exists(),
        "recorder child binary missing at {} — run `cargo build -p zeroship-migrate --bins` first",
        bin.display()
    );
}

fn write_mig(dir: &Path, stem: &str, ts: &str) {
    fs::write(dir.join(format!("{stem}.ts")), ts.as_bytes()).unwrap();
}

/// A hosted client that ALWAYS reports recorder-unreachable (retryable 503).
struct UnreachableClient;
impl RecorderClient for UnreachableClient {
    fn record(
        &self,
        _ts: &str,
        _app: &str,
        _name: &str,
        _blob: Option<&str>,
    ) -> Result<String, StructuredError> {
        Err(StructuredError {
            code: "RECORDER_UNREACHABLE".into(),
            message: "simulated recorder-unreachable".into(),
            http_status: 503,
            retryable: true,
        })
    }
}

/// A hosted client that reports a NON-retryable authoring reject (422).
struct RejectClient;
impl RecorderClient for RejectClient {
    fn record(
        &self,
        _ts: &str,
        _app: &str,
        _name: &str,
        _blob: Option<&str>,
    ) -> Result<String, StructuredError> {
        Err(StructuredError {
            code: "RECORD_EVAL_ERROR".into(),
            message: "simulated authoring reject (op outside recorder)".into(),
            http_status: 422,
            retryable: false,
        })
    }
}

#[test]
fn hosted_unreachable_falls_back_to_local_not_a_build_failure() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617120000_widgets";
    write_mig(dir.path(), stem, MIG_TS);

    let client = UnreachableClient;
    let via = RecordVia::Hosted {
        client: &client,
        local_fallback_budget: ResourceBudget::default(),
    };
    let outcome = build_migrations(dir.path(), OWNER, &via)
        .expect("recorder-unreachable must FALL BACK to local, not fail the build");
    assert_eq!(outcome.migrations.len(), 1);
    let m = &outcome.migrations[0];
    assert_eq!(
        m.record_path,
        RecordPath::HostedFellBackToLocal,
        "the build must record the fallback path"
    );
    // The IR stays in memory; no committed `.ir.json` is written.
    assert!(!dir.path().join(format!("{stem}.ir.json")).exists());
    assert!(!m.checksum.is_empty(), "a typed-value checksum was folded");
}

#[test]
fn hosted_non_retryable_reject_is_a_build_error_no_silent_fallback() {
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617120000_widgets";
    write_mig(dir.path(), stem, MIG_TS);

    let client = RejectClient;
    let via = RecordVia::Hosted {
        client: &client,
        local_fallback_budget: ResourceBudget::default(),
    };
    let err = build_migrations(dir.path(), OWNER, &via)
        .expect_err("a non-retryable authoring reject must surface as a build error");
    let msg = format!("{err}");
    assert!(
        msg.contains("RECORD_EVAL_ERROR"),
        "the reject's code must surface; got: {msg}"
    );
    // No committed artifact was written for a rejected migration.
    assert!(
        !dir.path().join(format!("{stem}.ir.json")).exists(),
        "a rejected migration must not write a committed artifact"
    );
}

#[test]
fn local_build_rejects_oversized_source_before_recording() {
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617120000_big";
    let mut huge = " ".repeat(MAX_TS_SOURCE_BYTES + 1);
    huge.push_str(MIG_TS);
    write_mig(dir.path(), stem, &huge);

    let started = std::time::Instant::now();
    let err = build_migrations(dir.path(), OWNER, &RecordVia::local())
        .expect_err("oversized local source must be rejected before recording");
    let elapsed = started.elapsed();

    match err {
        BuildError::SourceTooLarge { limit, actual, .. } => {
            assert_eq!(limit, MAX_TS_SOURCE_BYTES);
            assert!(
                actual > MAX_TS_SOURCE_BYTES as u64,
                "actual size must exceed the cap; got {actual}"
            );
        }
        other => panic!("expected SourceTooLarge, got {other:?}"),
    }
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "oversized source should fail at metadata/capped-read time, took {elapsed:?}"
    );
    assert!(
        !dir.path().join(format!("{stem}.ir.json")).exists(),
        "oversized source must not write a committed artifact"
    );
}

#[test]
fn build_allows_date_now_call_with_soft_warning() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617120000_date_now_call";
    let src = r#"
        import { table } from "@zeroship/migrate";
        export function up() {
          table("events").insert({ rows: [ { created_at: Date.now() } ] });
        }
    "#;
    write_mig(dir.path(), stem, src);

    let outcome = build_migrations(dir.path(), OWNER, &RecordVia::local())
        .expect("Date.now() call evaluates and records");
    assert_eq!(outcome.migrations.len(), 1);
    let m = &outcome.migrations[0];
    assert!(
        m.warnings.iter().any(|f| f.accessor.contains("Date.now")),
        "Date.now() call should surface only a soft advisory warning: {:?}",
        m.warnings
    );
    assert!(
        !dir.path().join(format!("{stem}.ir.json")).exists(),
        "Date.now() call must not write a committed artifact"
    );
}

#[test]
fn date_now_inside_comment_or_string_is_not_a_false_reject() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617120000_comment_string";
    let clean = r#"
        import { table } from "@zeroship/migrate";
        // Audit note: Date.now() and Math.random() are mentioned here only as text.
        export function up() {
          table("events").insert({
            rows: [ { note: "ran Date.now() and Math.random() at build" } ],
          });
        }
    "#;
    write_mig(dir.path(), stem, clean);

    let outcome = build_migrations(dir.path(), OWNER, &RecordVia::local())
        .expect("comment/string mentions of nondeterministic APIs record normally");
    assert_eq!(outcome.migrations.len(), 1);
    assert!(
        !dir.path().join(format!("{stem}.ir.json")).exists(),
        "deterministic source with inert Date.now/Math.random text must not write IR"
    );
}

#[test]
fn math_random_calls_evaluate_and_do_not_hard_fail() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617120000_random_calls";
    let src = r#"
        import { table } from "@zeroship/migrate";
        export function up() {
          const random = Math["random"];
          const collapsed = Math.random() - Math.random();
          table("events").insert({ rows: [ { sample: random(), collapsed } ] });
        }
    "#;
    write_mig(dir.path(), stem, src);

    let outcome = build_migrations(dir.path(), OWNER, &RecordVia::local())
        .expect("Math.random() calls evaluate and record");
    assert_eq!(outcome.migrations.len(), 1);
    assert!(
        outcome.migrations[0]
            .warnings
            .iter()
            .any(|f| f.accessor.contains("Math.random")),
        "literal Math.random() calls should surface only a soft advisory warning: {:?}",
        outcome.migrations[0].warnings
    );
    assert!(
        !dir.path().join(format!("{stem}.ir.json")).exists(),
        "Math.random() calls must not write a committed artifact"
    );
}

#[test]
fn date_now_arithmetic_call_evaluates_and_does_not_hard_fail() {
    assert_child_built();
    let dir = tempfile::tempdir().unwrap();
    let stem = "20240617120000_folded_date_now";
    let src = r#"
        import { table } from "@zeroship/migrate";
        export function up() {
          table("events").insert({ rows: [ { ms: Date.now() % 1000000 } ] });
        }
    "#;
    write_mig(dir.path(), stem, src);

    let outcome = build_migrations(dir.path(), OWNER, &RecordVia::local())
        .expect("Date.now() arithmetic call evaluates and records");
    assert_eq!(outcome.migrations.len(), 1);
    assert!(!dir.path().join(format!("{stem}.ir.json")).exists());
}

#[test]
fn local_and_hosted_paths_yield_same_typed_value_checksum() {
    assert_child_built();

    // LOCAL record path.
    let local_dir = tempfile::tempdir().unwrap();
    let stem = "20240617120000_widgets";
    write_mig(local_dir.path(), stem, MIG_TS);
    let local = build_migrations(
        local_dir.path(),
        OWNER,
        &RecordVia::Local {
            budget: ResourceBudget::default(),
        },
    )
    .expect("local record path");
    assert_eq!(local.migrations[0].record_path, RecordPath::Local);
    let local_checksum = local.migrations[0].checksum.clone();

    // HOSTED happy path: a client that drives the REAL sandboxed child (the same
    // child the local path uses), so it is a faithful hosted round-trip.
    struct LocalChildHostedClient;
    impl RecorderClient for LocalChildHostedClient {
        fn record(
            &self,
            ts: &str,
            app: &str,
            name: &str,
            _blob: Option<&str>,
        ) -> Result<String, StructuredError> {
            use zeroship_migrate::frontend::recorder_service::{
                spawn_sandboxed_record, RecordRequest,
            };
            use zeroship_migrate::frontend::{ResourceBudget, SandboxPosture};
            let req = RecordRequest {
                ts_source: ts.to_string(),
                owner_app: app.to_string(),
                name: name.to_string(),
                posture: SandboxPosture::Hosted,
                budget: ResourceBudget::default(),
                allow_read_paths: vec![],
                schema_types_blob: None,
            };
            spawn_sandboxed_record(&req)
                .map(|r| r.ir_json)
                .map_err(|e| (&e).into())
        }
    }

    let hosted_dir = tempfile::tempdir().unwrap();
    write_mig(hosted_dir.path(), stem, MIG_TS);
    let client = LocalChildHostedClient;
    let hosted = build_migrations(
        hosted_dir.path(),
        OWNER,
        &RecordVia::Hosted {
            client: &client,
            local_fallback_budget: ResourceBudget::default(),
        },
    )
    .expect("hosted record path");
    assert_eq!(hosted.migrations[0].record_path, RecordPath::Hosted);
    let hosted_checksum = hosted.migrations[0].checksum.clone();

    assert_eq!(
        local_checksum, hosted_checksum,
        "the LOCAL and HOSTED record paths must yield the SAME typed-value checksum (§8.9.2)"
    );

    // The transient BYTES are also byte-identical across the two paths (canonical
    // serialization + same owner stamp).
    assert_eq!(
        local.migrations[0].committed_bytes, hosted.migrations[0].committed_bytes,
        "the transient .ir.json bytes are byte-stable across record paths"
    );
}
