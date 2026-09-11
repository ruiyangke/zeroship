//! The platform corpus through the real Node host, recorder and `PostgreSQL`.

#[path = "platform_corpus/fixture.rs"]
mod fixture;

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeSet;
use std::fs;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Migration {
    file: String,
    name: String,
    op_count: usize,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LintReport {
    label: String,
    ok: bool,
    op_count: usize,
    dialects: Vec<DialectReport>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct DialectReport {
    dialect: String,
    ok: bool,
    op_count: Option<usize>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Outcome {
    applied: Vec<String>,
    skipped: Vec<String>,
    recovered: Vec<String>,
    pending_contracts: Vec<Value>,
}

#[derive(Deserialize)]
struct Event {
    version: String,
    kind: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Status {
    busy: bool,
    lock_holders: Vec<Value>,
    pending: Vec<Value>,
    aborted: Vec<Value>,
    rolled_back: Vec<Value>,
    pending_contracts: Vec<Value>,
    blocked: Vec<Value>,
    unexpected_journal: Vec<Value>,
    interrupted_unwinds: Vec<Value>,
    plans: Vec<Plan>,
    applied: Vec<String>,
    current_version: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Plan {
    name: String,
    version: String,
    state: String,
    missing_dependencies: Vec<Value>,
    steps: Vec<Step>,
}

#[derive(Deserialize)]
struct Step {
    version: String,
    state: String,
}

#[test]
fn platform_corpus_records_applies_and_reapplies_without_changes() {
    let ledger = corpus_ledger();
    let labels = ledger
        .iter()
        .map(|entry| entry.file.strip_suffix(".ts").unwrap())
        .collect::<Vec<_>>();
    let platform = fixture::Platform::start();

    let lint = platform.run("lint", &["--json", "--dialect", "postgres"]);
    let reports: Vec<LintReport> = serde_json::from_slice(&lint.stdout).expect("lint reports");
    assert_recorded(&reports, &ledger);
    // Generic lint can refuse platform-only capabilities. Require a complete
    // recording and a matching exit status; the actual apply must succeed.
    assert_eq!(
        lint.status.code(),
        Some(i32::from(reports.iter().any(|report| !report.ok)))
    );

    let first = platform.run("apply", &["--approve"]);
    let first = apply_records(&first.stdout, &labels).expect("first apply records");
    assert_fresh_apply(&first, &labels);
    let applied = unique(
        first
            .iter()
            .flat_map(|outcome| outcome.applied.iter().map(String::as_str)),
    );
    let history = platform.run("history", &["--json"]);
    let history: Value = serde_json::from_slice(&history.stdout).expect("durable history");
    let events: Vec<Event> =
        serde_json::from_value(history["events"].clone()).expect("history events");
    assert!(events.iter().all(|event| event.kind == "applied"));
    assert_eq!(
        unique(events.iter().map(|event| event.version.as_str())),
        applied
    );

    let status = platform.run("status", &["--json", "--strict"]);
    let status: Status = serde_json::from_slice(&status.stdout).expect("strict status");
    assert_clean_status(&status, &ledger, &first, &applied);

    let second = platform.run("apply", &["--approve"]);
    let second = apply_records(&second.stdout, &labels).expect("second apply records");
    assert_repeat_apply(&first, &second, &labels);
    let after = platform.run("history", &["--json"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&after.stdout).unwrap(),
        history,
        "repeat apply changed history"
    );
}

fn corpus_ledger() -> Vec<Migration> {
    let corpus = fixture::root().join("db/migrations-ts");
    let ledger: Vec<Migration> =
        serde_json::from_slice(&fs::read(corpus.join("op-counts.json")).unwrap()).unwrap();
    assert!(!ledger.is_empty(), "platform operation ledger is empty");
    unique(ledger.iter().map(|entry| entry.name.as_str()));
    let mut files = fs::read_dir(&corpus)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext == "ts")
                && !name.ends_with(".d.ts")
        })
        .collect::<Vec<_>>();
    files.sort();
    assert_eq!(
        files,
        ledger
            .iter()
            .map(|entry| entry.file.clone())
            .collect::<Vec<_>>()
    );
    assert!(ledger.iter().all(|entry| entry.op_count > 0));
    ledger
}

fn assert_recorded(reports: &[LintReport], ledger: &[Migration]) {
    assert_eq!(reports.len(), ledger.len(), "lint omitted a migration");
    for (report, expected) in reports.iter().zip(ledger) {
        assert_eq!(report.label, expected.name);
        assert_eq!(
            report.op_count, expected.op_count,
            "{}: recorder drift",
            expected.file
        );
        assert_eq!(report.dialects.len(), 1);
        let dialect = &report.dialects[0];
        assert_eq!(dialect.dialect, "postgres");
        assert_eq!(report.ok, dialect.ok);
        if let Some(count) = dialect.op_count {
            assert_eq!(
                count, expected.op_count,
                "{}: verifier drift",
                expected.file
            );
        }
    }
}

fn assert_fresh_apply(first: &[Outcome], labels: &[&str]) {
    for (outcome, label) in first.iter().zip(labels) {
        assert!(
            !outcome.applied.is_empty(),
            "{label}: recorder drained empty"
        );
        assert!(
            outcome.skipped.is_empty(),
            "{label}: database was not fresh"
        );
        assert!(outcome.recovered.is_empty(), "{label}: recovered work");
        assert!(
            outcome.pending_contracts.is_empty(),
            "{label}: pending contracts"
        );
    }
}

fn assert_clean_status(
    status: &Status,
    ledger: &[Migration],
    first: &[Outcome],
    applied: &BTreeSet<&str>,
) {
    assert!(!status.busy);
    for (label, rows) in [
        ("lock holders", &status.lock_holders),
        ("pending", &status.pending),
        ("aborted", &status.aborted),
        ("rolled back", &status.rolled_back),
        ("pending contracts", &status.pending_contracts),
        ("blocked", &status.blocked),
        ("unexpected journal", &status.unexpected_journal),
        ("interrupted unwinds", &status.interrupted_unwinds),
    ] {
        assert!(rows.is_empty(), "status {label}: {rows:?}");
    }
    assert_eq!(
        status
            .plans
            .iter()
            .map(|plan| &plan.name)
            .collect::<Vec<_>>(),
        ledger.iter().map(|entry| &entry.name).collect::<Vec<_>>()
    );
    assert_eq!(
        status.applied,
        status
            .plans
            .iter()
            .map(|plan| plan.version.clone())
            .collect::<Vec<_>>()
    );
    unique(status.applied.iter().map(String::as_str));
    assert_eq!(Some(&status.current_version), status.applied.last());
    for (plan, outcome) in status.plans.iter().zip(first) {
        assert_eq!(plan.state, "applied", "{}", plan.name);
        assert!(
            plan.missing_dependencies.is_empty(),
            "{}: missing dependencies",
            plan.name
        );
        assert!(
            !plan.steps.is_empty(),
            "{}: no journal-visible steps",
            plan.name
        );
        assert!(
            plan.steps.iter().all(|step| step.state == "applied"),
            "{}: incomplete steps",
            plan.name
        );
        assert_eq!(
            unique(plan.steps.iter().map(|step| step.version.as_str())),
            unique(outcome.applied.iter().map(String::as_str)),
            "{}: status assigned journal steps to a different migration",
            plan.name
        );
    }
    assert_eq!(
        unique(
            status
                .plans
                .iter()
                .flat_map(|plan| plan.steps.iter().map(|step| step.version.as_str()))
        ),
        *applied
    );
}

fn assert_repeat_apply(first: &[Outcome], second: &[Outcome], labels: &[&str]) {
    for ((before, after), label) in first.iter().zip(second).zip(labels) {
        assert!(after.applied.is_empty(), "{label}: applied again");
        assert!(after.recovered.is_empty(), "{label}: recovered work");
        assert!(
            after.pending_contracts.is_empty(),
            "{label}: pending contracts"
        );
        assert_eq!(
            unique(after.skipped.iter().map(String::as_str)),
            unique(before.applied.iter().map(String::as_str)),
            "{label}: skipped different steps"
        );
    }
}

fn unique<'a>(values: impl IntoIterator<Item = &'a str>) -> BTreeSet<&'a str> {
    let mut seen = BTreeSet::new();
    for value in values {
        assert!(!value.is_empty(), "empty identity");
        assert!(seen.insert(value), "duplicate identity: {value}");
    }
    assert!(!seen.is_empty(), "no identities examined");
    seen
}

// Apply has a per-file JSON outcome embedded in its progress line, rather than
// a --json mode. Consume every line and require the exact ordered file list so
// missing or malformed records cannot turn into an apparently empty success.
fn apply_records(stdout: &[u8], labels: &[&str]) -> Result<Vec<Outcome>, String> {
    if labels.is_empty() {
        return Err("empty corpus".into());
    }
    let stdout = std::str::from_utf8(stdout).map_err(|error| error.to_string())?;
    let lines = stdout.lines().collect::<Vec<_>>();
    if lines.len() != labels.len() {
        return Err(format!(
            "apply returned {} records for {} files",
            lines.len(),
            labels.len()
        ));
    }
    lines
        .into_iter()
        .zip(labels)
        .map(|(line, label)| {
            let json = line
                .strip_prefix(&format!("apply {label}: "))
                .ok_or_else(|| format!("expected apply result for {label}, got {line:?}"))?;
            serde_json::from_str(json)
                .map_err(|error| format!("{label}: invalid apply outcome: {error}"))
        })
        .collect()
}

#[test]
fn apply_protocol_rejects_incomplete_or_misidentified_results() {
    let line = "apply schema: {\"applied\":[\"step\"],\"skipped\":[],\"recovered\":[],\"pendingContracts\":[]}\n";
    assert!(apply_records(line.as_bytes(), &["schema"]).is_ok());
    for (output, labels) in [
        ("", vec![]),
        ("", vec!["schema"]),
        (line, vec!["schema", "roles"]),
        (line, vec!["other"]),
        ("apply schema: {}", vec!["schema"]),
        ("apply schema: broken", vec!["schema"]),
    ] {
        assert!(
            apply_records(output.as_bytes(), &labels).is_err(),
            "accepted {output:?}"
        );
    }
    let duplicate = line.repeat(2);
    assert!(apply_records(duplicate.as_bytes(), &["schema", "roles"]).is_err());
}
