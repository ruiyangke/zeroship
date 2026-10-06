//! Check what the shards ran against what they build: CI's `verify` job.
//!
//! Each shard's runs leave their reports in one directory (see `shard`). For
//! every shard and every run this requires the JUnit report and the listing,
//! and that the tests the run executed are exactly the tests its build lists,
//! less the untimed tests, which the untimed record must name instead. Across
//! shards it requires that no test ran in two primary runs, and that every test
//! target `cargo metadata` declares appears in exactly one shard's primary
//! listing. Together with the repository contract, which holds each shard's
//! packages and features to the workspace, that is every workspace test run
//! exactly once.

use crate::Result;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use xtask::shards::{Shard, Untimed, SHARDS};

/// A test, as `<binary id> <test name>`.
type TestId = String;

pub fn run(directory: &Path, metadata: &Value) -> Result<()> {
    let mut problems = Vec::new();
    let mut ran_in: BTreeMap<TestId, Vec<&str>> = BTreeMap::new();
    let mut binaries_in: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut executed_total = 0;
    for shard in SHARDS {
        let labels = std::iter::once(Shard::PRIMARY).chain(shard.repeats.iter().map(|repeat| repeat.label));
        for label in labels {
            let primary = label == Shard::PRIMARY;
            let listing = match read(directory, &format!("list-{}-{label}.json", shard.name)) {
                Ok(text) => parse_listing(&text)?,
                Err(error) => {
                    problems.push(error);
                    continue;
                }
            };
            let executed = match read(directory, &format!("junit-{}-{label}.xml", shard.name)) {
                Ok(text) => parse_junit(&text),
                Err(error) => {
                    problems.push(error);
                    continue;
                }
            };
            let (untimed, timed): (BTreeSet<TestId>, BTreeSet<TestId>) = listing
                .tests
                .iter()
                .cloned()
                .partition(|id| shard.untimed.iter().any(|untimed| selects(untimed, id)));
            problems.extend(difference(shard.name, label, "nextest", &timed, &executed));
            if primary {
                if !shard.untimed.is_empty() {
                    match read(directory, &format!("untimed-{}.txt", shard.name)) {
                        Ok(text) => {
                            let recorded: BTreeSet<TestId> =
                                text.lines().map(str::to_owned).collect();
                            problems.extend(difference(shard.name, label, "untimed", &untimed, &recorded));
                        }
                        Err(error) => problems.push(error),
                    }
                }
                for id in executed.iter().chain(untimed.iter()) {
                    ran_in.entry(id.clone()).or_default().push(shard.name);
                }
                for binary in &listing.binaries {
                    binaries_in.entry(binary.clone()).or_default().push(shard.name);
                }
                executed_total += executed.len() + untimed.len();
            }
        }
    }
    for (id, shards) in &ran_in {
        if shards.len() > 1 {
            problems.push(format!("{id} ran in shards {shards:?}"));
        }
    }
    let declared = declared_binaries(metadata);
    if declared.len() < 50 {
        problems.push(format!("cargo metadata declares too few test targets to check: {}", declared.len()));
    }
    for binary in &declared {
        match binaries_in.get(binary).map(Vec::len) {
            Some(1) => {}
            Some(_) => problems.push(format!("test target {binary} is in shards {:?}", binaries_in[binary])),
            None => problems.push(format!("test target {binary} is in no shard's listing")),
        }
    }
    if !problems.is_empty() {
        return Err(format!("shard reports disagree with what the shards build:\n  {}", problems.join("\n  ")).into());
    }
    eprintln!(
        "{executed_total} tests ran exactly once across {} shards, covering {} declared test targets",
        SHARDS.len(),
        declared.len()
    );
    Ok(())
}

fn read(directory: &Path, name: &str) -> std::result::Result<String, String> {
    std::fs::read_to_string(directory.join(name))
        .map_err(|error| format!("{name} did not report: {error}"))
}

/// The tests a run lists but did not execute, and the ones it executed but
/// does not list.
fn difference(
    shard: &str,
    label: &str,
    kind: &str,
    expected: &BTreeSet<TestId>,
    actual: &BTreeSet<TestId>,
) -> Vec<String> {
    let mut problems = Vec::new();
    let missing: Vec<&TestId> = expected.difference(actual).collect();
    let extra: Vec<&TestId> = actual.difference(expected).collect();
    if !missing.is_empty() {
        problems.push(format!(
            "{shard} {label}: {} listed tests did not run under {kind}, e.g. {:?}",
            missing.len(),
            &missing[..missing.len().min(5)]
        ));
    }
    if !extra.is_empty() {
        problems.push(format!(
            "{shard} {label}: {} tests ran under {kind} that the build does not list, e.g. {:?}",
            extra.len(),
            &extra[..extra.len().min(5)]
        ));
    }
    problems
}

/// Whether `untimed` selects the test `id`, as its `cargo test` filter does.
fn selects(untimed: &Untimed, id: &str) -> bool {
    id.split_once(' ').is_some_and(|(binary, test)| {
        binary == untimed.binary_id() && test.contains(untimed.filter)
    })
}

/// A nextest listing: every binary it names, and every test it would run.
#[derive(Debug, Default, PartialEq, Eq)]
struct Listing {
    binaries: BTreeSet<String>,
    tests: BTreeSet<TestId>,
}

fn parse_listing(text: &str) -> Result<Listing> {
    let listing: Value = serde_json::from_str(text)?;
    let suites = listing["rust-suites"]
        .as_object()
        .ok_or("a nextest listing has no rust-suites")?;
    let mut parsed = Listing::default();
    for (binary, suite) in suites {
        parsed.binaries.insert(binary.clone());
        for (test, case) in suite["testcases"].as_object().into_iter().flatten() {
            if case["filter-match"]["status"] == "matches" {
                parsed.tests.insert(format!("{binary} {test}"));
            }
        }
    }
    Ok(parsed)
}

/// The tests a nextest JUnit report records, from each `testcase` element's
/// `classname` (the binary id) and `name`.
fn parse_junit(text: &str) -> BTreeSet<TestId> {
    let mut tests = BTreeSet::new();
    for element in text.split("<testcase ").skip(1) {
        let tag = element.split('>').next().unwrap_or_default();
        if let (Some(name), Some(binary)) = (attribute(tag, "name"), attribute(tag, "classname")) {
            tests.insert(format!("{binary} {name}"));
        }
    }
    tests
}

fn attribute(tag: &str, name: &str) -> Option<String> {
    let start = tag.find(&format!(" {name}=\"")).map(|at| at + name.len() + 3).or_else(|| {
        tag.starts_with(&format!("{name}=\"")).then_some(name.len() + 2)
    })?;
    let value = &tag[start..];
    let end = value.find('"')?;
    Some(
        value[..end]
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&"),
    )
}

/// The nextest binary id of every test target the workspace declares.
fn declared_binaries(metadata: &Value) -> BTreeSet<String> {
    let members: BTreeSet<&str> = metadata["workspace_members"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut binaries = BTreeSet::new();
    for package in metadata["packages"].as_array().into_iter().flatten() {
        if !package["id"].as_str().is_some_and(|id| members.contains(id)) {
            continue;
        }
        let name = package["name"].as_str().unwrap_or_default();
        for target in package["targets"].as_array().into_iter().flatten() {
            if target["test"] != true {
                continue;
            }
            let kinds: Vec<&str> = target["kind"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
            let target_name = target["name"].as_str().unwrap_or_default();
            if kinds.iter().any(|kind| matches!(*kind, "lib" | "rlib" | "cdylib" | "dylib" | "staticlib" | "proc-macro")) {
                binaries.insert(name.to_owned());
            } else if kinds.contains(&"bin") {
                binaries.insert(format!("{name}::bin/{target_name}"));
            } else if kinds.contains(&"test") {
                binaries.insert(format!("{name}::{target_name}"));
            } else if kinds.contains(&"example") {
                binaries.insert(format!("{name}::example/{target_name}"));
            } else if kinds.contains(&"bench") {
                binaries.insert(format!("{name}::bench/{target_name}"));
            }
        }
    }
    binaries
}

#[cfg(test)]
mod tests {
    use super::{parse_junit, parse_listing, selects, Listing};
    use xtask::shards::Untimed;

    #[test]
    fn a_junit_report_yields_each_testcase_by_binary_and_name() {
        let report = r#"<testsuites><testsuite name="zeroship-kv::main" tests="2">
<testcase name="integration::a&amp;b" classname="zeroship-kv::main" time="1.0"/>
<testcase name="integration::c" classname="zeroship-kv::main" time="1.0"><failure/></testcase>
</testsuite></testsuites>"#;
        let tests: Vec<String> = parse_junit(report).into_iter().collect();
        assert_eq!(
            tests,
            ["zeroship-kv::main integration::a&b", "zeroship-kv::main integration::c"]
        );
        assert!(parse_junit("<testsuites/>").is_empty(), "no testcase, no test");
    }

    #[test]
    fn a_listing_keeps_matching_tests_and_every_binary() {
        let listing = r#"{"rust-suites": {
            "pkg": {"testcases": {"t::run": {"filter-match": {"status": "matches"}},
                                  "t::skip": {"filter-match": {"status": "mismatch", "reason": "ignored"}}}},
            "pkg::main": {"testcases": {}}}}"#;
        let parsed = parse_listing(listing).expect("parse the listing");
        assert_eq!(
            parsed,
            Listing {
                binaries: ["pkg", "pkg::main"].map(str::to_owned).into(),
                tests: ["pkg t::run"].map(str::to_owned).into(),
            }
        );
    }

    #[test]
    fn an_untimed_filter_selects_by_binary_and_substring() {
        let untimed = Untimed {
            package: "pkg",
            target: "main",
            filter: "integration::slow",
            reason: "",
        };
        assert!(selects(&untimed, "pkg::main integration::slow::one"));
        assert!(!selects(&untimed, "pkg integration::slow::one"), "another binary");
        assert!(!selects(&untimed, "pkg::main integration::fast"), "another test");
    }
}
