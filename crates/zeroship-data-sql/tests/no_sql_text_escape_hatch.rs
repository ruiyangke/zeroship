//! Source and manifest gates: no SQL text, no `Serialize`, no escape hatch.
//!
//! # Why these are gates and not review items
//!
//! SC-3 flags a shape it calls "cannot-fail": a negative check over types that
//! do not exist yet matches nothing and reports success, and keeps doing so if
//! the work is abandoned. Both gates below therefore **assert the subject
//! exists first** - a count of source files and a count of plan types actually
//! found - before ruling on the negative property. Each also carries a positive
//! control that runs the same predicate over a synthetic violation, so a green
//! run means the predicate can still say no.

use std::path::{Path, PathBuf};

/// Fewer source files than this means the walk collapsed and every negative
/// check below would pass over an empty set.
const SOURCE_FILE_FLOOR: usize = 8;

/// The plan and grammar types the negative checks must actually find. If a
/// rename empties this, the gates stop ruling on anything and say so.
///
/// The write family's types are listed for the reason SC-3 gives for the gate
/// existing at all: a negative check over types that do not exist matches
/// nothing and reports success. Adding a family without adding its types here
/// would leave the two gates below scanning the read family and calling it the
/// crate.
const PLAN_TYPES: &[&str] = &[
    "pub enum DbPlan",
    "pub struct Select",
    "pub enum Predicate",
    "pub struct Projection",
    "pub struct Ident",
    "pub enum Literal",
    "pub struct FieldPath",
    // The write family.
    "pub struct Insert",
    "pub struct Update",
    "pub struct Delete",
    "pub enum Assignment",
    "pub enum WriteValue",
    "pub struct Returning",
    "pub struct BindBudget",
    // The search family. Listed for the same reason the write family's types
    // are: without them the two gates below scan read and write and call it the
    // crate, and a `raw_sql` slot or a serde derive added to `search.rs` would
    // be ruled on by nothing.
    "pub struct Search",
    "pub enum SearchCriterion",
    "pub enum SearchScalarKind",
    "pub enum VectorMetric",
    "pub struct QueryVector",
    "pub struct GeoPoint",
    "pub struct RadiusMetres",
];

fn crate_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// Every `.rs` file under `src/`, as (relative path, contents).
fn sources() -> Vec<(String, String)> {
    let root = crate_root().join("src");
    let mut found = Vec::new();
    collect(&root, &root, &mut found);
    found.sort();
    found
}

fn collect(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
    let entries =
        std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()));
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect(root, &path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("rs") {
            let body = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            let name = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .display()
                .to_string();
            out.push((name, body));
        }
    }
}

/// The predicate the gate runs, extracted so it can be exercised on input whose
/// answer is known. A line "declares an escape hatch" if it introduces an item
/// whose name says it accepts SQL text.
fn declares_an_escape_hatch(line: &str) -> bool {
    let line = line.trim();
    if line.starts_with("//") {
        return false;
    }
    ESCAPE_HATCH_NAMES.iter().any(|name| line.contains(name))
}

/// Names an escape hatch would have to be spelled. Deliberately includes
/// `into_string`, which is not SQL text but is the usual way a validated
/// newtype is laundered back into the type it was refusing.
const ESCAPE_HATCH_NAMES: &[&str] = &[
    "raw_sql",
    "raw_query",
    "sql_fragment",
    "unchecked_sql",
    "from_sql_text",
    "push_sql",
    "sql_literal",
    "unsafe_ident",
    "unchecked_ident",
    "into_string",
    "from_str_unchecked",
];

fn derives_serde(line: &str) -> bool {
    let line = line.trim();
    if line.starts_with("//") {
        return false;
    }
    (line.contains("derive(") || line.starts_with("#["))
        && (line.contains("Serialize") || line.contains("Deserialize"))
}

/// The subject must exist before any negative check over it means anything.
#[test]
fn the_plan_types_the_gates_rule_on_actually_exist() {
    let sources = sources();
    assert!(
        sources.len() >= SOURCE_FILE_FLOOR,
        "the source walk found {} files, fewer than the {SOURCE_FILE_FLOOR} this crate \
         has; every gate below would pass over an empty set",
        sources.len()
    );
    let all: String = sources.iter().map(|(_, body)| body.as_str()).collect();
    let mut found = 0_usize;
    for declaration in PLAN_TYPES {
        assert!(
            all.contains(declaration),
            "`{declaration}` was not found in src/; the negative gates below would scan \
             for a property of types that do not exist"
        );
        found += 1;
    }
    assert_eq!(found, PLAN_TYPES.len());
    println!("ruled on {found} plan types across {} files", sources.len());
}

/// No public surface accepts SQL text, and none launders a validated newtype
/// back into an owned `String`.
#[test]
fn no_source_declares_an_sql_text_escape_hatch() {
    let sources = sources();
    assert!(sources.len() >= SOURCE_FILE_FLOOR);
    let mut ruled_on = 0_usize;
    let mut offenders: Vec<String> = Vec::new();
    for (name, body) in &sources {
        for (number, line) in body.lines().enumerate() {
            if declares_an_escape_hatch(line) {
                offenders.push(format!("{name}:{}: {}", number + 1, line.trim()));
            }
            ruled_on += 1;
        }
    }
    assert!(
        offenders.is_empty(),
        "{} escape hatch(es) reached the source:\n  {}\n\nA plan never carries a \
         fragment of SQL text. If a caller genuinely cannot express something, the \
         answer is a node, not a string.",
        offenders.len(),
        offenders.join("\n  ")
    );
    assert!(
        ruled_on >= 1500,
        "ruled on only {ruled_on} lines; the walk is not reaching the crate"
    );
    println!("ruled on {ruled_on} source lines");
}

/// SC-3's decision 3. The derive is one line and its cost is a versioned wire
/// format, so it is checked rather than remembered.
#[test]
fn no_plan_type_derives_serialize() {
    let sources = sources();
    let mut offenders: Vec<String> = Vec::new();
    let mut ruled_on = 0_usize;
    for (name, body) in &sources {
        for (number, line) in body.lines().enumerate() {
            if derives_serde(line) {
                offenders.push(format!("{name}:{}: {}", number + 1, line.trim()));
            }
            ruled_on += 1;
        }
    }
    assert!(
        offenders.is_empty(),
        "a serde derive reached a plan type:\n  {}",
        offenders.join("\n  ")
    );
    assert!(ruled_on >= 1500, "ruled on only {ruled_on} lines");
    println!("ruled on {ruled_on} source lines");
}

/// Query plans must remain unavailable to generic serialization APIs.
#[test]
fn plans_do_not_implement_serialize() {
    trybuild::TestCases::new().compile_fail("tests/ui/plan_is_not_serializable.rs");
}

#[test]
fn the_gate_predicates_can_actually_fail() {
    assert!(declares_an_escape_hatch(
        "pub fn raw_sql(s: String) -> Fragment {"
    ));
    assert!(declares_an_escape_hatch(
        "    pub fn into_string(self) -> String {"
    ));
    assert!(!declares_an_escape_hatch("pub fn as_str(&self) -> &str {"));
    assert!(
        !declares_an_escape_hatch("/// there is no raw_sql on this type"),
        "a comment mentioning the name must not be flagged, or the gate is unusable \
         in a codebase that documents why the hatch is absent"
    );

    assert!(derives_serde("#[derive(Debug, Serialize)]"));
    assert!(derives_serde("#[derive(Deserialize)]"));
    assert!(!derives_serde("#[derive(Debug, Clone, PartialEq, Eq)]"));
    assert!(!derives_serde("//! no Deserialize is derived here"));
    println!("ruled on 8 synthetic lines");
}
