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
//!
//! The portable runs repeat the CLI's tests on the creator platforms (see
//! `xtask::shards::PORTABLE`). Each must have run exactly what its build lists,
//! and at least one test, and its listing must match the shards' listing of the
//! same packages except for the confined tests, each of which is listed on
//! exactly the systems its entry names.

use crate::Result;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use xtask::shards::{Confined, Portable, Shard, Untimed, LINUX, PORTABLE, SHARDS};

/// A test, as `<binary id> <test name>`.
type TestId = String;

pub fn run(directory: &Path, metadata: &Value) -> Result<()> {
    let mut problems = Vec::new();
    let mut ran_in: BTreeMap<TestId, Vec<&str>> = BTreeMap::new();
    let mut binaries_in: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut linux_listed: BTreeSet<TestId> = BTreeSet::new();
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
                linux_listed.extend(listing.tests.iter().cloned());
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
    let mut platforms = BTreeMap::new();
    for platform in PORTABLE.platforms {
        let label = Portable::label(platform);
        let listing = read(directory, &format!("list-{label}.json"))
            .and_then(|text| parse_listing(&text).map_err(|error| format!("list-{label}.json: {error}")));
        let executed = read(directory, &format!("junit-{label}.xml")).map(|text| parse_junit(&text));
        match (listing, executed) {
            (Ok(listing), Ok(executed)) => {
                problems.extend(difference(platform.name, &label, "nextest", &listing.tests, &executed));
                if executed.is_empty() {
                    problems.push(format!("{label} ran no test"));
                }
                eprintln!("{label}: {} tests ran", executed.len());
                platforms.insert(platform.name, listing.tests);
            }
            (listing, executed) => {
                problems.extend(listing.err());
                problems.extend(executed.err());
            }
        }
    }
    problems.extend(placement_problems(
        &linux_listed,
        &platforms,
        PORTABLE.packages,
        PORTABLE.confined,
    ));
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

/// Whether the test `id` belongs to one of `packages`: its binary id is the
/// package's name, or the name followed by `::`.
fn in_packages(id: &str, packages: &[&str]) -> bool {
    let binary = id.split_once(' ').map_or(id, |(binary, _)| binary);
    packages.iter().any(|package| {
        binary == *package
            || binary
                .strip_prefix(package)
                .is_some_and(|rest| rest.starts_with("::"))
    })
}

/// Where each test of the portable `packages` is listed, held to `confined`.
///
/// `linux` is every test the shards list, and `platforms` each platform's
/// listing. A test no entry covers must be listed on Linux and on every
/// platform; a covered test must be listed on exactly the systems its entry
/// names. Every entry must cover a listed test, and no test may be covered by
/// two entries.
fn placement_problems(
    linux: &BTreeSet<TestId>,
    platforms: &BTreeMap<&str, BTreeSet<TestId>>,
    packages: &[&str],
    confined: &[Confined],
) -> Vec<String> {
    let mut problems = Vec::new();
    let linux: BTreeSet<&TestId> = linux.iter().filter(|id| in_packages(id, packages)).collect();
    if linux.is_empty() {
        problems.push(format!("the shards list no test of the portable packages {packages:?}"));
    }
    let mut every: BTreeSet<&TestId> = linux.clone();
    for (platform, listed) in platforms {
        for id in listed {
            if in_packages(id, packages) {
                every.insert(id);
            } else {
                problems.push(format!("{platform} lists {id}, which is not a test of {packages:?}"));
            }
        }
    }
    let mut used = vec![false; confined.len()];
    for id in every {
        let (binary, test) = id.split_once(' ').unwrap_or((id.as_str(), ""));
        let covering: Vec<usize> = (0..confined.len())
            .filter(|at| confined[*at].covers(binary, test))
            .collect();
        if covering.len() > 1 {
            problems.push(format!("{id} is covered by more than one confined entry"));
        }
        for at in &covering {
            used[*at] = true;
        }
        let entry = covering.first().map(|at| &confined[*at]);
        let systems = std::iter::once((LINUX, linux.contains(id)))
            .chain(platforms.iter().map(|(platform, listed)| (*platform, listed.contains(id))));
        for (system, listed) in systems {
            let exists = entry.is_none_or(|entry| entry.on.contains(&system));
            match (listed, exists, entry.is_some()) {
                (true, false, _) => problems.push(format!(
                    "{id} is listed on {system}, which its confined entry leaves out"
                )),
                (false, true, true) => problems.push(format!(
                    "{id} is not listed on {system}, which its confined entry names"
                )),
                (false, true, false) => problems.push(format!(
                    "{id} is not listed on {system}; a test that exists on some systems only \
                     needs a confined entry saying which and why"
                )),
                _ => {}
            }
        }
    }
    for (entry, used) in confined.iter().zip(used) {
        if !used {
            problems.push(format!(
                "the confined entry {} {} covers no listed test",
                entry.binary, entry.prefix
            ));
        }
    }
    problems
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
    use super::{in_packages, parse_junit, parse_listing, placement_problems, selects, Listing};
    use std::collections::{BTreeMap, BTreeSet};
    use xtask::shards::{Confined, Untimed};

    fn tests(ids: &[&str]) -> BTreeSet<String> {
        ids.iter().map(|id| (*id).to_owned()).collect()
    }

    const UNIX_ONLY: Confined = Confined {
        binary: "cli::main",
        prefix: "e2e::modes::",
        on: &["linux", "macos"],
        reason: "file modes",
    };

    const OTHERS_ONLY: Confined = Confined {
        binary: "cli::bin/cli",
        prefix: "guard::polls",
        on: &["macos", "windows"],
        reason: "the fallback",
    };

    /// The listings a correct confined table describes: one test everywhere,
    /// one on the Unix systems, one off Linux, and a test of another package
    /// on Linux only, which the check ignores.
    fn agreeing() -> (BTreeSet<String>, BTreeMap<&'static str, BTreeSet<String>>) {
        let linux = tests(&[
            "cli::bin/cli args::parse",
            "cli::main e2e::modes::private",
            "other::main e2e::anything",
        ]);
        let platforms = BTreeMap::from([
            (
                "macos",
                tests(&["cli::bin/cli args::parse", "cli::main e2e::modes::private", "cli::bin/cli guard::polls"]),
            ),
            ("windows", tests(&["cli::bin/cli args::parse", "cli::bin/cli guard::polls"])),
        ]);
        (linux, platforms)
    }

    #[test]
    fn listings_matching_the_confined_table_are_accepted() {
        let (linux, platforms) = agreeing();
        let problems = placement_problems(&linux, &platforms, &["cli"], &[UNIX_ONLY, OTHERS_ONLY]);
        assert!(problems.is_empty(), "{problems:#?}");
    }

    #[test]
    fn a_test_missing_from_a_platform_without_an_entry_is_reported() {
        let (linux, platforms) = agreeing();
        let problems = placement_problems(&linux, &platforms, &["cli"], &[OTHERS_ONLY]);
        assert_eq!(
            problems,
            ["cli::main e2e::modes::private is not listed on windows; a test that exists on some \
              systems only needs a confined entry saying which and why"]
        );
    }

    #[test]
    fn a_confined_test_listed_where_its_entry_leaves_it_out_is_reported() {
        let (linux, mut platforms) = agreeing();
        platforms.get_mut("windows").unwrap().insert("cli::main e2e::modes::private".to_owned());
        let problems = placement_problems(&linux, &platforms, &["cli"], &[UNIX_ONLY, OTHERS_ONLY]);
        assert_eq!(
            problems,
            ["cli::main e2e::modes::private is listed on windows, which its confined entry leaves out"]
        );
    }

    #[test]
    fn a_confined_test_absent_where_its_entry_names_it_is_reported() {
        let (linux, mut platforms) = agreeing();
        platforms.get_mut("macos").unwrap().remove("cli::bin/cli guard::polls");
        let problems = placement_problems(&linux, &platforms, &["cli"], &[UNIX_ONLY, OTHERS_ONLY]);
        assert_eq!(
            problems,
            ["cli::bin/cli guard::polls is not listed on macos, which its confined entry names"]
        );
    }

    #[test]
    fn a_stale_or_overlapping_entry_is_reported() {
        let (linux, platforms) = agreeing();
        let stale = Confined {
            binary: "cli::main",
            prefix: "e2e::gone::",
            on: &["linux"],
            reason: "removed",
        };
        let problems = placement_problems(&linux, &platforms, &["cli"], &[UNIX_ONLY, OTHERS_ONLY, stale]);
        assert_eq!(problems, ["the confined entry cli::main e2e::gone:: covers no listed test"]);

        let overlapping = Confined {
            binary: "cli::main",
            prefix: "e2e::",
            on: &["linux", "macos"],
            reason: "wider",
        };
        let problems =
            placement_problems(&linux, &platforms, &["cli"], &[UNIX_ONLY, OTHERS_ONLY, overlapping]);
        assert_eq!(problems, ["cli::main e2e::modes::private is covered by more than one confined entry"]);
    }

    #[test]
    fn no_linux_test_of_the_packages_and_a_foreign_platform_test_are_reported() {
        let (_, platforms) = agreeing();
        let problems = placement_problems(&tests(&["other::main e2e::anything"]), &platforms, &["cli"], &[]);
        assert!(
            problems.iter().any(|problem| problem.starts_with("the shards list no test")),
            "{problems:#?}"
        );
        let mut platforms = platforms;
        platforms.get_mut("macos").unwrap().insert("other::main e2e::anything".to_owned());
        let problems = placement_problems(&agreeing().0, &platforms, &["cli"], &[UNIX_ONLY, OTHERS_ONLY]);
        assert_eq!(
            problems,
            [r#"macos lists other::main e2e::anything, which is not a test of ["cli"]"#]
        );
    }

    #[test]
    fn a_whole_name_entry_covers_one_test_and_a_module_entry_every_test_under_it() {
        assert!(OTHERS_ONLY.covers("cli::bin/cli", "guard::polls"));
        assert!(
            !OTHERS_ONLY.covers("cli::bin/cli", "guard::polls_twice"),
            "a test whose name only starts the same way"
        );
        assert!(UNIX_ONLY.covers("cli::main", "e2e::modes::private"));
        assert!(UNIX_ONLY.covers("cli::main", "e2e::modes::public"));
        assert!(!UNIX_ONLY.covers("cli::bin/cli", "e2e::modes::private"), "another binary");
    }

    #[test]
    fn a_package_owns_its_name_and_its_named_binaries_only() {
        assert!(in_packages("cli t::a", &["cli"]));
        assert!(in_packages("cli::main t::a", &["cli"]));
        assert!(in_packages("cli::bin/cli t::a", &["cli"]));
        assert!(!in_packages("cli-core t::a", &["cli"]), "a package sharing a prefix");
        assert!(!in_packages("cli-core::main t::a", &["cli"]));
    }

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
