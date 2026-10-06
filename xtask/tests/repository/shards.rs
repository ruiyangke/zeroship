//! Every workspace test runs in exactly one CI test shard.
//!
//! `xtask::shards::SHARDS` is the one place shard membership is written. These
//! checks hold it to what it has to agree with: `cargo metadata` and
//! `cargo tree`, which say which packages carry tests, which features each test
//! target needs and which features each build resolves, and the workflow, which
//! says what CI runs. A package added to the workspace with tests and no shard
//! fails here, as does a package in two shards, a test target its shard's run
//! never builds, a package its shard builds with fewer features than the
//! workspace-wide build, a feature of a shard package no run enables and no
//! entry explains, a test job shaped so it could skip a shard or forgive its
//! failure, and a second CI job that runs a workspace package's tests outside
//! the shards. CI's `verify` job then checks what the shards actually ran.

use crate::architecture::repo;
use serde_json::Value;
use serde_yaml::Value as Yaml;
use std::collections::{BTreeMap, BTreeSet};
use xtask::shards::{feature_package, selection, Shard, SHARDS};

/// The workspace packages with a target nextest or `cargo test --doc` runs.
fn tested_packages(packages: &[&Value]) -> BTreeSet<String> {
    packages
        .iter()
        .filter(|package| {
            package["targets"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|target| target["test"] == true || target["doctest"] == true)
        })
        .map(|package| package["name"].as_str().expect("package name").to_owned())
        .collect()
}

/// For each package, the shards that list it.
fn owners() -> BTreeMap<&'static str, Vec<&'static str>> {
    let mut owners: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for shard in SHARDS {
        for package in shard.packages {
            owners.entry(package).or_default().push(shard.name);
        }
    }
    owners
}

/// Membership problems: a tested package in no shard or in several, and a shard
/// entry that names no workspace package.
fn membership_problems(tested: &BTreeSet<String>, members: &BTreeSet<String>) -> Vec<String> {
    let owners = owners();
    let mut problems = Vec::new();
    for package in tested {
        match owners.get(package.as_str()).map(Vec::as_slice) {
            None | Some([]) => problems.push(format!("{package} has tests and is in no shard")),
            Some([_]) => {}
            Some(shards) => problems.push(format!("{package} is in shards {shards:?}")),
        }
    }
    for package in owners.keys() {
        if !members.contains(*package) {
            problems.push(format!("shard package {package} is not a workspace member"));
        }
    }
    problems
}

fn member_names() -> BTreeSet<String> {
    repo::workspace()
        .iter()
        .map(|package| package["name"].as_str().expect("package name").to_owned())
        .collect()
}

#[test]
fn every_tested_workspace_package_is_in_exactly_one_shard() {
    let packages = repo::workspace();
    let members = member_names();
    let tested = tested_packages(&packages);
    assert!(
        tested.len() >= 10,
        "the workspace scan found too few tested packages to check: {tested:?}"
    );
    let problems = membership_problems(&tested, &members);
    assert!(problems.is_empty(), "shard membership: {problems:#?}");

    // Rejection controls: the same check over a package set it must refuse.
    let mut unowned = tested.clone();
    unowned.insert("zeroship-unsharded-fixture".to_owned());
    let mut with_member = members.clone();
    with_member.insert("zeroship-unsharded-fixture".to_owned());
    assert_eq!(
        membership_problems(&unowned, &with_member),
        ["zeroship-unsharded-fixture has tests and is in no shard"],
        "a tested package outside every shard must be reported"
    );
    let first = SHARDS[0].packages[0];
    let mut without_first = members.clone();
    without_first.remove(first);
    assert!(
        membership_problems(&tested, &without_first)
            .iter()
            .any(|problem| problem.contains(first) && problem.contains("not a workspace member")),
        "a shard package missing from the workspace must be reported"
    );
}

/// Every feature entry a shard names: its run's, its repeats' and its
/// unexercised entries.
fn named_features(shard: &Shard) -> impl Iterator<Item = &'static str> + '_ {
    shard
        .features
        .iter()
        .chain(shard.repeats.iter().flat_map(|repeat| repeat.features.iter()))
        .copied()
        .chain(shard.unexercised.iter().map(|entry| entry.feature))
}

#[test]
fn every_test_target_is_built_by_its_shard_and_every_entry_names_something_real() {
    let packages: BTreeMap<String, &Value> = repo::workspace()
        .into_iter()
        .map(|package| (package["name"].as_str().expect("name").to_owned(), package))
        .collect();
    let mut examined = 0;
    let mut problems = Vec::new();
    let mut names = BTreeSet::new();
    for shard in SHARDS {
        assert!(names.insert(shard.name), "shard {} is declared twice", shard.name);
        assert!(!shard.packages.is_empty(), "shard {} owns no package", shard.name);
        let mut labels = BTreeSet::from([Shard::PRIMARY]);
        for repeat in shard.repeats {
            if !labels.insert(repeat.label) || repeat.reason.trim().is_empty() {
                problems.push(format!("{}: repeat {} is unnamed, reused or unexplained", shard.name, repeat.label));
            }
        }
        for entry in named_features(shard) {
            let package = feature_package(entry);
            let feature = entry.split_once('/').map(|(_, feature)| feature).unwrap_or("");
            if !shard.packages.contains(&package) {
                problems.push(format!("{}: {entry} names a package outside the shard", shard.name));
            } else if packages[package]["features"].get(feature).is_none() {
                problems.push(format!("{}: {package} declares no feature {feature}", shard.name));
            }
        }
        for entry in shard.unexercised {
            if entry.reason.trim().is_empty() {
                problems.push(format!("{}: {} is unexercised without a reason", shard.name, entry.feature));
            }
        }
        for untimed in shard.untimed {
            let declares = shard.packages.contains(&untimed.package)
                && packages[untimed.package]["targets"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .any(|target| target["name"] == untimed.target && target["test"] == true);
            if !declares || untimed.reason.trim().is_empty() || untimed.filter.is_empty() {
                problems.push(format!(
                    "{}: untimed {} names no test target of the shard, or has no filter or reason",
                    shard.name,
                    untimed.expression()
                ));
            }
        }
        for package in shard.packages {
            let Some(manifest) = packages.get(*package) else {
                continue;
            };
            for target in manifest["targets"].as_array().into_iter().flatten() {
                if target["test"] != true {
                    continue;
                }
                examined += 1;
                for required in target["required-features"].as_array().into_iter().flatten() {
                    let required = format!("{package}/{}", required.as_str().expect("feature"));
                    if !shard.features.contains(&required.as_str()) {
                        problems.push(format!(
                            "{}: test target {} needs {required}, which the shard's run does \
                             not enable, so nothing builds or runs it",
                            shard.name, target["name"]
                        ));
                    }
                }
            }
        }
    }
    assert!(examined >= 50, "the target scan examined too few test targets: {examined}");
    assert!(problems.is_empty(), "shard runs: {problems:#?}");
}

/// The ORM compile-fail warm the testkit declares is what the data shard runs
/// untimed, so `pnpm build:test-artifacts` and the shard run the same thing and
/// the nextest exclusion matches what the warm ran.
#[test]
fn the_untimed_run_is_the_warm_the_testkit_declares() {
    let untimed: Vec<Vec<&str>> = SHARDS
        .iter()
        .flat_map(|shard| shard.untimed.iter().map(|untimed| untimed.args()))
        .collect();
    assert!(
        untimed.contains(&zeroship_testkit::prebuilt::WARM_ARGS.to_vec()),
        "no shard runs the testkit's trybuild warm {:?} untimed; untimed runs: {untimed:?}",
        zeroship_testkit::prebuilt::WARM_ARGS
    );
}

/// For each workspace member the resolution resolves, the features it enables.
///
/// `args` selects the resolution as a nextest run would: `--workspace`, or a
/// shard's `-p` and `--features` arguments. Dev edges are included because a
/// test build activates the selected packages' dev-dependencies.
fn resolved_features(args: &[String], members: &BTreeSet<String>) -> BTreeMap<String, BTreeSet<String>> {
    let output = std::process::Command::new(env!("CARGO"))
        .current_dir(repo::root())
        .args(["tree", "--locked", "-e", "normal,build,dev", "--prefix", "none", "-f", "{p}|{f}"])
        .args(args)
        .output()
        .expect("run cargo tree");
    assert!(
        output.status.success(),
        "cargo tree {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let mut features: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    for line in String::from_utf8(output.stdout).expect("cargo tree prints UTF-8").lines() {
        let Some((package, enabled)) = line.split_once('|') else {
            continue;
        };
        let name = package.split_whitespace().next().unwrap_or_default();
        if !members.contains(name) {
            continue;
        }
        let enabled = enabled.trim().trim_end_matches("(*)").trim();
        features.entry(name.to_owned()).or_default().extend(
            enabled
                .split(',')
                .filter(|feature| !feature.is_empty())
                .map(str::to_owned),
        );
    }
    features
}

/// The features `package` gets in the workspace-wide build and not in its
/// shard's, as `package/feature`.
fn missing_features(
    package: &str,
    workspace: &BTreeMap<String, BTreeSet<String>>,
    shard: &BTreeMap<String, BTreeSet<String>>,
) -> Vec<String> {
    let empty = BTreeSet::new();
    workspace
        .get(package)
        .unwrap_or(&empty)
        .difference(shard.get(package).unwrap_or(&empty))
        .map(|feature| format!("{package}/{feature}"))
        .collect()
}

/// A test behind `#[cfg(feature = ...)]` compiles in the workspace-wide build
/// whenever any member enables that feature, and a shard builds only its own
/// packages, so a feature only another member turns on would silently drop
/// those tests from CI. Every shard package must be built with at least the
/// features the workspace-wide build gives it.
#[test]
fn every_shard_package_is_built_with_the_features_the_workspace_gives_it() {
    let members = member_names();
    let workspace = resolved_features(&["--workspace".to_owned()], &members);
    assert!(
        workspace.len() >= 10,
        "the workspace resolution named too few members: {workspace:?}"
    );
    let mut checked = 0;
    let mut problems = Vec::new();
    for shard in SHARDS {
        let resolved = resolved_features(&selection(shard.packages, shard.features), &members);
        for package in shard.packages {
            checked += 1;
            let missing = missing_features(package, &workspace, &resolved);
            if !missing.is_empty() {
                problems.push(format!("{}: {missing:?}", shard.name));
            }
        }
    }
    assert!(checked >= 10, "too few shard packages were compared: {checked}");
    assert!(
        problems.is_empty(),
        "shard runs build these packages with fewer features than the workspace-wide \
         build, so tests behind them run nowhere: {problems:#?}"
    );

    // Rejection control: a package whose shard resolution lacks a feature the
    // workspace enables is reported, and one with every feature is not.
    let set = |features: &[&str]| -> BTreeSet<String> { features.iter().map(|f| (*f).to_owned()).collect() };
    let workspace = BTreeMap::from([("fixture".to_owned(), set(&["default", "codec"]))]);
    let narrow = BTreeMap::from([("fixture".to_owned(), set(&["default"]))]);
    let wide = BTreeMap::from([("fixture".to_owned(), set(&["default", "codec", "tls"]))]);
    assert_eq!(missing_features("fixture", &workspace, &narrow), ["fixture/codec"]);
    assert!(missing_features("fixture", &workspace, &wide).is_empty());
}

/// Coverage problems for one package: features it declares that no run
/// enables and no unexercised entry explains, and unexercised entries naming
/// a feature some run does enable.
fn uncovered_features(
    package: &str,
    declared: &BTreeSet<String>,
    runs: &[BTreeMap<String, BTreeSet<String>>],
    unexercised: &BTreeSet<String>,
) -> Vec<String> {
    let enabled: BTreeSet<&String> = runs
        .iter()
        .filter_map(|run| run.get(package))
        .flatten()
        .collect();
    let mut problems = Vec::new();
    for feature in declared {
        let entry = format!("{package}/{feature}");
        let explained = unexercised.contains(&entry);
        match (enabled.contains(feature), explained) {
            (false, false) => problems.push(format!("{entry} is enabled by no run and explained by no entry")),
            (true, true) => problems.push(format!("{entry} is listed unexercised, but a run enables it")),
            _ => {}
        }
    }
    problems
}

/// A test behind a feature of a shard package that no run enables runs in no
/// CI job. Every feature a shard package declares must be enabled by the
/// shard's run or one of its repeats, or be listed as unexercised with why.
#[test]
fn every_feature_of_a_shard_package_is_exercised_or_explained() {
    let members = member_names();
    let packages: BTreeMap<String, &Value> = repo::workspace()
        .into_iter()
        .map(|package| (package["name"].as_str().expect("name").to_owned(), package))
        .collect();
    let mut declared_total = 0;
    let mut problems = Vec::new();
    for shard in SHARDS {
        let runs: Vec<BTreeMap<String, BTreeSet<String>>> = std::iter::once(shard.features)
            .chain(shard.repeats.iter().map(|repeat| repeat.features))
            .map(|features| resolved_features(&selection(shard.packages, features), &members))
            .collect();
        let unexercised: BTreeSet<String> =
            shard.unexercised.iter().map(|entry| entry.feature.to_owned()).collect();
        for package in shard.packages {
            let declared: BTreeSet<String> = packages[*package]["features"]
                .as_object()
                .into_iter()
                .flatten()
                .map(|(feature, _)| feature.clone())
                .filter(|feature| feature != "default")
                .collect();
            declared_total += declared.len();
            problems.extend(uncovered_features(package, &declared, &runs, &unexercised));
        }
    }
    assert!(declared_total >= 10, "too few shard package features were examined: {declared_total}");
    assert!(problems.is_empty(), "feature coverage: {problems:#?}");

    // Rejection controls: an unenabled, unexplained feature is reported, as is
    // an explanation for a feature a run enables; each alone is accepted.
    let set = |items: &[&str]| -> BTreeSet<String> { items.iter().map(|item| (*item).to_owned()).collect() };
    let declared = set(&["codec", "suite"]);
    let runs = [BTreeMap::from([("fixture".to_owned(), set(&["codec"]))])];
    assert_eq!(
        uncovered_features("fixture", &declared, &runs, &set(&[])),
        ["fixture/suite is enabled by no run and explained by no entry"]
    );
    assert!(uncovered_features("fixture", &declared, &runs, &set(&["fixture/suite"])).is_empty());
    assert_eq!(
        uncovered_features("fixture", &declared, &runs, &set(&["fixture/codec", "fixture/suite"])),
        ["fixture/codec is listed unexercised, but a run enables it"]
    );
}

fn workflow() -> Yaml {
    serde_yaml::from_str(&repo::read(".github/workflows/ci.yml")).expect("parse ci.yml")
}

/// The composite actions the workflow can run, by `uses: ./<path>`.
fn composite_actions() -> BTreeMap<String, Yaml> {
    let mut actions = BTreeMap::new();
    for path in repo::files(".github/actions", &["yml", "yaml"]) {
        let relative = path
            .strip_prefix(repo::root())
            .expect("an action under the repository")
            .parent()
            .expect("an action file sits in its directory")
            .to_string_lossy()
            .into_owned();
        let text = std::fs::read_to_string(&path).expect("read the action");
        actions.insert(format!("./{relative}"), serde_yaml::from_str(&text).expect("parse the action"));
    }
    actions
}

fn steps(job: &Yaml) -> Vec<&Yaml> {
    job["steps"].as_sequence().into_iter().flatten().collect()
}

fn text(value: &Yaml) -> Option<&str> {
    value.as_str().map(str::trim)
}

/// The keys of a mapping.
fn keys(value: &Yaml) -> BTreeSet<String> {
    value
        .as_mapping()
        .into_iter()
        .flatten()
        .filter_map(|(key, _)| key.as_str().map(str::to_owned))
        .collect()
}

/// The exact commands the plan, test and verify jobs must run.
const PLAN_SCRIPT: &str = r#"echo "shards=$(cargo xtask shards list)" >> "$GITHUB_OUTPUT""#;
const SHARD_SCRIPT: &str = r#"cargo xtask test "$SHARD""#;
const VERIFY_SCRIPT: &str = "cargo xtask shards verify reports";

/// Everything about the plan, test and verify jobs that could let a shard be
/// skipped, dropped, or fail without failing the run.
fn structure_problems(workflow: &Yaml) -> Vec<String> {
    let mut problems = Vec::new();
    let jobs = &workflow["jobs"];
    let plan = &jobs["plan"];
    let test = &jobs["test"];
    let verify = &jobs["verify"];
    for (name, job) in [("plan", plan), ("test", test), ("verify", verify)] {
        if job.is_null() {
            problems.push(format!("there is no `{name}` job"));
        }
        if !job["continue-on-error"].is_null() {
            problems.push(format!("`{name}` may fail without failing the run"));
        }
        for step in steps(job) {
            if !step["continue-on-error"].is_null() {
                problems.push(format!("a `{name}` step may fail without failing the job"));
            }
        }
    }

    // The plan job prints the shard list, whole, into its `shards` output.
    if !plan["if"].is_null() {
        problems.push("`plan` is conditional".to_owned());
    }
    if text(&plan["outputs"]["shards"]) != Some("${{ steps.shards.outputs.shards }}") {
        problems.push("`plan`'s `shards` output is not its `shards` step's".to_owned());
    }
    match steps(plan).into_iter().find(|step| text(&step["id"]) == Some("shards")) {
        Some(step) => {
            if text(&step["run"]) != Some(PLAN_SCRIPT) || !step["if"].is_null() {
                problems.push(format!("`plan`'s `shards` step must be exactly `{PLAN_SCRIPT}`, unconditionally"));
            }
        }
        None => problems.push("`plan` has no `shards` step".to_owned()),
    }

    // The test job runs one job per listed shard, nothing added or removed.
    if text(&test["needs"]) != Some("plan") {
        problems.push("`test` must need exactly `plan`".to_owned());
    }
    if !test["if"].is_null() {
        problems.push("`test` is conditional".to_owned());
    }
    if keys(&test["strategy"]) != BTreeSet::from(["fail-fast".to_owned(), "matrix".to_owned()])
        || test["strategy"]["fail-fast"] != Yaml::Bool(false)
    {
        problems.push("`test`'s strategy must be exactly `fail-fast: false` and the matrix".to_owned());
    }
    if keys(&test["strategy"]["matrix"]) != BTreeSet::from(["shard".to_owned()])
        || text(&test["strategy"]["matrix"]["shard"]) != Some("${{ fromJSON(needs.plan.outputs.shards) }}")
    {
        problems.push(
            "`test`'s matrix must be exactly `shard: ${{ fromJSON(needs.plan.outputs.shards) }}`, \
             with no include or exclude"
                .to_owned(),
        );
    }
    let test_steps = steps(test);
    let run_at = test_steps.iter().position(|step| step["run"].as_str().is_some_and(|run| run.contains("cargo xtask test")));
    match run_at {
        Some(at) => {
            let step = test_steps[at];
            let env_is_shard = keys(&step["env"]) == BTreeSet::from(["SHARD".to_owned()])
                && text(&step["env"]["SHARD"]) == Some("${{ matrix.shard }}");
            if text(&step["run"]) != Some(SHARD_SCRIPT) || !env_is_shard {
                problems.push(format!(
                    "`test` must run exactly `{SHARD_SCRIPT}` with only `SHARD: ${{{{ matrix.shard }}}}`"
                ));
            }
            for (index, step) in test_steps.iter().enumerate() {
                let condition = text(&step["if"]);
                let allowed = condition.is_none() || (index > at && condition == Some("${{ !cancelled() }}"));
                if !allowed {
                    problems.push(format!(
                        "`test` step {index} is conditional ({condition:?}); only reporting steps after \
                         the shard may be, and only on `${{{{ !cancelled() }}}}`"
                    ));
                }
            }
        }
        None => problems.push(format!("`test` never runs `{SHARD_SCRIPT}`")),
    }
    let uploads = test_steps.iter().any(|step| {
        text(&step["uses"]).is_some_and(|uses| uses.starts_with("actions/upload-artifact@"))
            && text(&step["with"]["name"]) == Some("reports-${{ matrix.shard }}")
            && text(&step["with"]["path"]) == Some("target/nextest/ci/")
    });
    if !uploads {
        problems.push("`test` does not upload `target/nextest/ci/` as `reports-<shard>`".to_owned());
    }

    // The verify job checks every shard's reports, whatever happened to them.
    let needs: BTreeSet<&str> = verify["needs"].as_sequence().into_iter().flatten().filter_map(Yaml::as_str).collect();
    if !needs.contains("plan") || !needs.contains("test") {
        problems.push("`verify` must need `plan` and `test`".to_owned());
    }
    if text(&verify["if"]) != Some("always()") {
        problems.push("`verify` must run `if: always()`".to_owned());
    }
    let downloads = steps(verify).iter().any(|step| {
        text(&step["uses"]).is_some_and(|uses| uses.starts_with("actions/download-artifact@"))
            && text(&step["with"]["pattern"]) == Some("reports-*")
            && text(&step["with"]["path"]) == Some("reports")
            && step["with"]["merge-multiple"] == Yaml::Bool(true)
            && step["if"].is_null()
    });
    if !downloads {
        problems.push("`verify` does not download every `reports-*` artifact into `reports`".to_owned());
    }
    let verifies = steps(verify)
        .iter()
        .any(|step| text(&step["run"]) == Some(VERIFY_SCRIPT) && step["if"].is_null());
    if !verifies {
        problems.push(format!("`verify` never runs exactly `{VERIFY_SCRIPT}`"));
    }
    problems
}

/// Jobs that deliberately run some workspace tests a second time, and why.
const DELIBERATE_RERUNS: &[(&str, &str)] = &[(
    "miri",
    "Miri interprets a few unit tests to find undefined behaviour in unsafe code; the shards run \
     them natively",
)];

/// Steps outside the sharded test job that run workspace tests: a `cargo test`,
/// `cargo nextest` or `cargo <tool> test` against the root workspace, or a
/// shard through `cargo xtask test` other than as a build-only step. Composite
/// actions a job uses are scanned as part of that job.
fn duplicate_runs(workflow: &Yaml, actions: &BTreeMap<String, Yaml>) -> Vec<String> {
    let shard_names: BTreeSet<&str> = SHARDS.iter().map(|shard| shard.name).collect();
    let mut found = Vec::new();
    for (name, job) in workflow["jobs"].as_mapping().into_iter().flatten() {
        let name = name.as_str().expect("job name");
        if name == "test" || DELIBERATE_RERUNS.iter().any(|(job, _)| *job == name) {
            continue;
        }
        let mut scripts: Vec<&str> = steps(job).iter().filter_map(|step| step["run"].as_str()).collect();
        for step in steps(job) {
            if let Some(action) = text(&step["uses"]).and_then(|uses| actions.get(uses)) {
                scripts.extend(action["runs"]["steps"].as_sequence().into_iter().flatten().filter_map(|step| step["run"].as_str()));
            }
        }
        for script in scripts {
            for line in script.lines() {
                let words: Vec<&str> = line.split_whitespace().collect();
                let xtask_workspace = words.contains(&"xtask/Cargo.toml");
                let build_only = words.contains(&"--build-only");
                let runs_tests = words.windows(2).any(|pair| pair[0] == "cargo" && matches!(pair[1], "test" | "nextest"))
                    || words.windows(3).any(|triple| triple[0] == "cargo" && triple[1] != "xtask" && triple[2] == "test");
                let runs_shard = words.windows(4).any(|quad| {
                    quad[..3] == ["cargo", "xtask", "test"]
                        && (shard_names.contains(quad[3].trim_matches('"'))
                            || quad[3].starts_with('"')
                            || quad[3].starts_with('$'))
                });
                if (runs_tests && !xtask_workspace) || (runs_shard && !build_only) {
                    found.push(format!("{name}: {}", line.trim()));
                }
            }
        }
    }
    found
}

/// Apply `edit` to a copy of `workflow`.
fn mutated(workflow: &Yaml, edit: impl FnOnce(&mut Yaml)) -> Yaml {
    let mut copy = workflow.clone();
    edit(&mut copy);
    copy
}

fn set(target: &mut Yaml, key: &str, value: &str) {
    target
        .as_mapping_mut()
        .expect("a mapping")
        .insert(Yaml::from(key), serde_yaml::from_str(value).expect("parse the value"));
}

/// The step of the test job that runs the shard.
fn shard_step(workflow: &mut Yaml) -> &mut Yaml {
    workflow["jobs"]["test"]["steps"]
        .as_sequence_mut()
        .expect("test steps")
        .iter_mut()
        .find(|step| step["run"].as_str().is_some_and(|run| run.contains("cargo xtask test")))
        .expect("the shard step")
}

fn plan_step(workflow: &mut Yaml) -> &mut Yaml {
    workflow["jobs"]["plan"]["steps"]
        .as_sequence_mut()
        .expect("plan steps")
        .iter_mut()
        .find(|step| step["id"].as_str() == Some("shards"))
        .expect("the shards step")
}

#[test]
fn ci_runs_every_listed_shard_once_and_nothing_else_runs_their_tests() {
    let workflow = workflow();
    let actions = composite_actions();
    assert!(!actions.is_empty(), "the scan found no composite action under .github/actions");
    let problems = structure_problems(&workflow);
    assert!(problems.is_empty(), "the shard jobs' shape: {problems:#?}");
    let duplicates = duplicate_runs(&workflow, &actions);
    assert!(
        duplicates.is_empty(),
        "these steps run workspace tests outside the shards, so those tests run twice: \
         {duplicates:#?}"
    );

    // Rejection controls: each way a shard could be dropped, skipped or
    // forgiven, applied alone to the real workflow, is reported.
    let controls: Vec<(&str, Yaml)> = vec![
        ("a matrix exclude", mutated(&workflow, |w| set(&mut w["jobs"]["test"]["strategy"]["matrix"], "exclude", "[{shard: data}]"))),
        ("a matrix include", mutated(&workflow, |w| set(&mut w["jobs"]["test"]["strategy"]["matrix"], "include", "[{shard: extra}]"))),
        ("continue-on-error on the shard step", mutated(&workflow, |w| set(shard_step(w), "continue-on-error", "true"))),
        ("continue-on-error on the test job", mutated(&workflow, |w| set(&mut w["jobs"]["test"], "continue-on-error", "true"))),
        ("a slice of the shard list", mutated(&workflow, |w| set(plan_step(w), "run", r#""echo \"shards=$(cargo xtask shards list | jq -c '.[1:]')\" >> \"$GITHUB_OUTPUT\"""#))),
        ("a condition on the shard step", mutated(&workflow, |w| set(shard_step(w), "if", "matrix.shard != 'billing'"))),
        ("a condition on the test job", mutated(&workflow, |w| set(&mut w["jobs"]["test"], "if", "github.event_name == 'push'"))),
        ("fail-fast", mutated(&workflow, |w| set(&mut w["jobs"]["test"]["strategy"], "fail-fast", "true"))),
        ("a verify job that does not always run", mutated(&workflow, |w| set(&mut w["jobs"]["verify"], "if", "success()"))),
        ("a verify step that never runs", mutated(&workflow, |w| {
            let step = w["jobs"]["verify"]["steps"]
                .as_sequence_mut()
                .expect("verify steps")
                .iter_mut()
                .find(|step| step["run"].as_str() == Some(VERIFY_SCRIPT))
                .expect("the verify step");
            set(step, "if", "false");
        })),
    ];
    for (control, workflow) in &controls {
        assert!(!structure_problems(workflow).is_empty(), "{control} must be reported");
    }
    let doubled = mutated(&workflow, |w| {
        let extra: Yaml = serde_yaml::from_str(
            "steps:\n  - run: |\n      cargo nextest run -p zeroship-auth\n  \
             - run: cargo xtask test auth\n  - run: cargo test --manifest-path xtask/Cargo.toml --test main\n  \
             - run: cargo xtask test \"$shard\" --build-only\n  - run: cargo miri test -p zeroship-id --lib\n",
        )
        .expect("parse the control job");
        w["jobs"].as_mapping_mut().expect("jobs").insert(Yaml::from("doubled"), extra);
    });
    assert_eq!(
        duplicate_runs(&doubled, &actions),
        [
            "doubled: cargo nextest run -p zeroship-auth",
            "doubled: cargo xtask test auth",
            "doubled: cargo miri test -p zeroship-id --lib"
        ],
        "a second job running a shard's tests must be reported, and the xtask workspace and \
         build-only steps must not"
    );
}
