//! A stock image with the shared-server watchdog on top.
//!
//! A server kind whose image the testkit does not otherwise customise - a stock
//! PostgreSQL major, say - still needs the watchdog as its PID 1 to be a shared
//! server. [`with_watchdog`] builds `FROM <base>` plus the watchdog, tagged by
//! the content of everything that builds it, so a worktree whose watchdog moved
//! never runs a server built from another worktree's recipe.

use sha2::{Digest, Sha256};
use testcontainers::{runners::SyncBuilder, GenericBuildableImage};

/// The image name every derived image is built under; the tag carries the
/// content hash, which covers the base.
pub const NAME: &str = "zeroship-testkit-watchdog";

/// Where the watchdog sits in a derived image.
pub const WATCHDOG: &str = "/usr/local/bin/zeroship-watchdog";

/// The recipe a derived image is built from.
fn dockerfile(base: &str) -> String {
    format!("FROM {base}\nCOPY watchdog.sh {WATCHDOG}\nRUN chmod 0755 {WATCHDOG}\n")
}

/// The tag `base` plus the watchdog is built under.
#[must_use]
pub fn tag(base: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(dockerfile(base).as_bytes());
    hasher.update(include_bytes!("watchdog.sh"));
    let digest = format!("{:x}", hasher.finalize());
    digest[..12].to_string()
}

/// The reference (`name:tag`) of `base` plus the watchdog, built when the
/// daemon does not already carry it.
///
/// The build is skipped when the daemon already carries the tag, so every
/// process of a run shares one build.
///
/// # Errors
/// When the Docker daemon cannot build the image.
pub fn with_watchdog(base: &str) -> Result<String, String> {
    let tag = tag(base);
    let reference = format!("{NAME}:{tag}");
    if crate::shared::image_exists(&reference) {
        return Ok(reference);
    }
    GenericBuildableImage::new(NAME, tag.as_str())
        .with_dockerfile_string(dockerfile(base))
        .with_data(include_bytes!("watchdog.sh").to_vec(), "watchdog.sh")
        .build_image()
        .map(|_| reference)
        .map_err(|error| format!("could not build {base} with the watchdog: {error}"))
}
