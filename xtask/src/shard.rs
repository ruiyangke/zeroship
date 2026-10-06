//! Run one shard: prepare what its tests need, run its untimed tests, run its
//! packages under nextest and their doctests, and run its repeats.
//!
//! Every run leaves its reports in nextest's store under the workspace root,
//! [`REPORTS`]: the JUnit report of what ran (`junit-<shard>-<label>.xml`), the
//! listing of everything the run's build contains (`list-<shard>-<label>.json`)
//! and the tests the untimed step ran (`untimed-<shard>.txt`). CI's `verify`
//! job reads them back (`cargo xtask shards verify`).

use crate::{cargo, checked, data, memlock, prepare, Result};
use serde_json::Value;
use std::path::{Path, PathBuf};
use xtask::shards::{
    doctest_args, list_args, nextest_args, primary_expression, Prepare, Shard, Untimed,
};

/// Where nextest's `ci` profile writes its JUnit report, relative to the
/// workspace root: nextest's store directory is `target/nextest` under the
/// workspace root whatever the cargo target directory, and `.config/nextest.toml`
/// leaves it there.
pub const REPORTS: &str = "target/nextest/ci";

/// What a shard invocation does.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Prepare, run the tests and the doctests.
    Run,
    /// Build every artifact the run needs, and run nothing.
    Build,
}

pub fn run(shard: &Shard, filter: Option<&str>, mode: Mode) -> Result<()> {
    if mode == Mode::Run {
        if shard.starts_services() {
            memlock::require()?;
        } else {
            memlock::raise()?;
        }
    }
    let metadata = workspace_metadata()?;
    let reports = reports_directory(&metadata)?;
    std::fs::create_dir_all(&reports)?;
    if mode == Mode::Run {
        forget_reports(shard, &reports)?;
    }
    for step in shard.prepare {
        if mode == Mode::Build && !step.builds() {
            continue;
        }
        prepare_step(*step, &metadata)?;
        crate::cancelled()?;
    }
    if mode == Mode::Build {
        for untimed in shard.untimed {
            let mut args = untimed.args();
            args.extend(["--locked", "--no-run"]);
            checked(cargo().args(&args), &format!("{} untimed build", shard.name))?;
        }
        nextest(shard, Shard::PRIMARY, shard.features, None, mode, &reports)?;
        for repeat in shard.repeats {
            nextest(shard, repeat.label, repeat.features, None, mode, &reports)?;
        }
        return Ok(());
    }
    // Every run is attempted, so one failing suite does not hide the next one's
    // verdict, and the first failure is the one reported.
    let untimed = if filter.is_none() {
        untimed_tests(shard, &reports)
    } else {
        Ok(())
    };
    let expression = primary_expression(shard, filter);
    let tests = nextest(
        shard,
        Shard::PRIMARY,
        shard.features,
        expression.as_deref(),
        mode,
        &reports,
    );
    let docs = if filter.is_none() {
        doctests(shard, &metadata)
    } else {
        Ok(())
    };
    let mut outcome = untimed.and(tests).and(docs);
    for repeat in shard.repeats {
        eprintln!("Repeating {} ({}) because {}", shard.name, repeat.label, repeat.reason);
        let repeated = nextest(
            shard,
            repeat.label,
            repeat.features,
            expression.as_deref(),
            mode,
            &reports,
        );
        outcome = outcome.and(repeated);
    }
    outcome
}

fn prepare_step(step: Prepare, metadata: &Value) -> Result<()> {
    match step {
        Prepare::HostChain => prepare::build_host(),
        Prepare::ServiceBinaries => prepare::build_services(),
        Prepare::CdcRelay => prepare::build_relay(),
        Prepare::DataPosture => data::posture(metadata),
        // The servers are held by the process-wide statics these calls fill,
        // so every test process of the run joins them rather than booting its
        // own after the idle grace.
        Prepare::BareServer => {
            let _lease = zeroship_testkit::postgres::server::warm();
            Ok(())
        }
        Prepare::PlatformServer => {
            zeroship_testkit::postgres::platform();
            Ok(())
        }
        Prepare::MigrateServerPlatform => {
            zeroship_testkit::postgres::migrate_server_platform();
            Ok(())
        }
    }
}

/// Remove this shard's reports from an earlier run, so the ones left are this
/// run's.
fn forget_reports(shard: &Shard, reports: &Path) -> Result<()> {
    for entry in std::fs::read_dir(reports)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let ours = [
            format!("junit-{}-", shard.name),
            format!("list-{}-", shard.name),
            format!("untimed-{}.", shard.name),
        ]
        .iter()
        .any(|prefix| name.starts_with(prefix.as_str()));
        if ours {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

/// Run the shard's untimed tests with `cargo test`, and record the tests each
/// run as `<binary id> <test name>` lines in `untimed-<shard>.txt`.
fn untimed_tests(shard: &Shard, reports: &Path) -> Result<()> {
    let record = reports.join(format!("untimed-{}.txt", shard.name));
    let mut lines = String::new();
    for untimed in shard.untimed {
        eprintln!(
            "Running {} outside nextest because {}",
            untimed.expression(),
            untimed.reason
        );
        let mut args = untimed.args();
        args.push("--locked");
        checked(cargo().args(&args), &format!("{} untimed tests", shard.name))?;
        for test in listed(untimed)? {
            lines.push_str(&format!("{} {test}\n", untimed.binary_id()));
        }
    }
    if !shard.untimed.is_empty() {
        std::fs::write(record, lines)?;
    }
    Ok(())
}

/// The tests `untimed` selects, by libtest's own listing.
fn listed(untimed: &Untimed) -> Result<Vec<String>> {
    let mut args = untimed.args();
    args.extend(["--locked", "--", "--list", "--format", "terse"]);
    let output = cargo().args(&args).output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    let tests: Vec<String> = String::from_utf8(output.stdout)?
        .lines()
        .filter_map(|line| line.strip_suffix(": test"))
        .map(str::to_owned)
        .collect();
    if tests.is_empty() {
        return Err(format!("{} selects no test", untimed.expression()).into());
    }
    Ok(tests)
}

/// One nextest run, followed by its JUnit report kept under a name of its own
/// and the listing of everything the run's build contains.
fn nextest(
    shard: &Shard,
    label: &str,
    features: &[&str],
    expression: Option<&str>,
    mode: Mode,
    reports: &Path,
) -> Result<()> {
    let args = nextest_args(shard.packages, features, expression, mode == Mode::Build);
    let outcome = checked(cargo().args(&args), &format!("{} {label}", shard.name));
    if mode == Mode::Run {
        let report = reports.join("junit.xml");
        if report.exists() {
            std::fs::rename(&report, reports.join(format!("junit-{}-{label}.xml", shard.name)))?;
        }
        let listing = cargo().args(list_args(shard.packages, features)).output()?;
        if !listing.status.success() {
            return outcome.and(Err(format!(
                "{} {label} listing: {}",
                shard.name,
                String::from_utf8_lossy(&listing.stderr)
            )
            .into()));
        }
        std::fs::write(
            reports.join(format!("list-{}-{label}.json", shard.name)),
            listing.stdout,
        )?;
    }
    outcome
}

/// The doctests of the shard's packages, over the shard's whole selection.
fn doctests(shard: &Shard, metadata: &Value) -> Result<()> {
    if !shard.packages.iter().any(|name| has_doctests(metadata, name)) {
        return Ok(());
    }
    checked(
        cargo().args(doctest_args(shard.packages, shard.features)),
        &format!("{} doctests", shard.name),
    )
}

fn has_doctests(metadata: &Value, name: &str) -> bool {
    metadata["packages"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|package| package["name"] == name)
        .flat_map(|package| package["targets"].as_array().into_iter().flatten())
        .any(|target| target["doctest"] == true)
}

pub fn workspace_metadata() -> Result<Value> {
    let output = cargo()
        .args(["metadata", "--no-deps", "--format-version", "1", "--locked"])
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    Ok(serde_json::from_slice(&output.stdout)?)
}

fn reports_directory(metadata: &Value) -> Result<PathBuf> {
    metadata["workspace_root"]
        .as_str()
        .map(|root| Path::new(root).join(REPORTS))
        .ok_or_else(|| "cargo metadata names no workspace root".into())
}
