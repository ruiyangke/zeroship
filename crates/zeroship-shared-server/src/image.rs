//! A stock image with the shared-server watchdog on top.
//!
//! A server kind whose image no fixture otherwise customises - a stock
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

/// The watchdog script itself, for a fixture whose own Dockerfile copies it in
/// beside its customisations rather than building on [`with_watchdog`].
pub const WATCHDOG_SCRIPT: &[u8] = include_bytes!("watchdog.sh");

/// The recipe a derived image is built from.
fn dockerfile(base: &str) -> String {
    format!("FROM {base}\nCOPY watchdog.sh {WATCHDOG}\nRUN chmod 0755 {WATCHDOG}\n")
}

/// The tag `base` plus the watchdog is built under.
#[must_use]
pub fn tag(base: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(dockerfile(base).as_bytes());
    hasher.update(WATCHDOG_SCRIPT);
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
    if crate::image_exists(&reference) {
        return Ok(reference);
    }
    GenericBuildableImage::new(NAME, tag.as_str())
        .with_dockerfile_string(dockerfile(base))
        .with_data(WATCHDOG_SCRIPT.to_vec(), "watchdog.sh")
        .build_image()
        .map(|_| reference)
        .map_err(|error| format!("could not build {base} with the watchdog: {error}"))
}

/// The reference (`name:tag`) of the image `dockerfile` builds from `files`,
/// built when the daemon does not already carry it.
///
/// The tag is the content hash of the Dockerfile and every file handed to the
/// build, names included, so a recipe edit moves the tag and two worktrees with
/// different recipes never share an image. The watchdog is not added for the
/// caller: a recipe that runs a shared server copies [`WATCHDOG_SCRIPT`] in
/// itself, as one of `files`.
///
/// # Errors
/// When the Docker daemon cannot build the image.
pub fn build(name: &str, dockerfile: &str, files: &[(&str, &[u8])]) -> Result<String, String> {
    let mut hasher = Sha256::new();
    hasher.update(dockerfile.as_bytes());
    for (path, bytes) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
    }
    let tag = format!("{:x}", hasher.finalize())[..12].to_string();
    let reference = format!("{name}:{tag}");
    if crate::image_exists(&reference) {
        return Ok(reference);
    }
    let mut image =
        GenericBuildableImage::new(name, tag.as_str()).with_dockerfile_string(dockerfile);
    for (path, bytes) in files {
        image = image.with_data(bytes.to_vec(), *path);
    }
    image
        .build_image()
        .map(|_| reference)
        .map_err(|error| format!("could not build {name}: {error}"))
}
