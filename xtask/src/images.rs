//! The fixture images CI restores from its cache instead of pulling or building.
//!
//! `list` prints every base reference in `zeroship_testkit::images::ALL` and the
//! content-hashed reference of every recipe in `zeroship_testkit::images::FETCHING`,
//! which CI hashes into the cache key and hands to `docker save`. `pull` pulls
//! only the base references the daemon lacks and builds only the recipes it
//! lacks, so a key change fetches the new images and not the cached ones.
//! `check` fails naming every reference the daemon lacks, which is what a
//! restored cache must not leave.

use crate::Result;
use clap::Subcommand;
use std::process::{Command, Stdio};
use zeroship_testkit::images::{ALL, FETCHING};

#[derive(Subcommand)]
pub enum Action {
    /// Print every fixture image reference, one per line.
    List,
    /// Pull the base images and build the fetching recipes the Docker daemon
    /// does not carry.
    Pull,
    /// Fail naming every fixture image the Docker daemon does not carry.
    Check,
}

/// Every reference CI restores: the base images, then the built recipes.
fn references() -> Vec<String> {
    ALL.iter()
        .map(|image| image.reference())
        .chain(FETCHING.iter().map(|recipe| recipe.reference()))
        .collect()
}

pub fn run(action: Action) -> Result<()> {
    match action {
        Action::List => {
            for reference in references() {
                println!("{reference}");
            }
            Ok(())
        }
        Action::Pull => {
            for image in ALL {
                let reference = image.reference();
                if present(&reference)? {
                    continue;
                }
                let status = Command::new("docker").args(["pull", &reference]).status()?;
                if !status.success() {
                    return Err(format!("docker pull {reference}: {status}").into());
                }
            }
            for recipe in FETCHING {
                let built = recipe.build()?;
                eprintln!("built {built}");
            }
            Ok(())
        }
        Action::Check => {
            let all = references();
            let mut missing = Vec::new();
            for reference in &all {
                if !present(reference)? {
                    missing.push(reference.as_str());
                }
            }
            if missing.is_empty() {
                eprintln!("the daemon carries all {} fixture images", all.len());
                return Ok(());
            }
            Err(format!(
                "the Docker daemon lacks fixture images {}; a test would have to pull or build them",
                missing.join(", ")
            )
            .into())
        }
    }
}

/// Whether the daemon carries `reference`.
fn present(reference: &str) -> Result<bool> {
    Ok(Command::new("docker")
        .args(["image", "inspect", "--format", "{{.Id}}", reference])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}
