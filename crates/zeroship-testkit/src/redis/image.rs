//! The Dragonfly cluster image: the upstream image plus the shared watchdog
//! and the wrapper that runs every node as the watchdog's one tracked child.
//!
//! Tagged by the content of everything that builds it, so a worktree whose
//! Dockerfile, watchdog or cluster wrapper moved never overwrites another
//! worktree's image and never runs a cluster built from the wrong recipe.

use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use testcontainers::{runners::SyncBuilder, GenericBuildableImage, GenericImage};

/// The image name every worktree builds under; the tag carries the content hash.
pub const NAME: &str = "zeroship-testkit-dragonfly-cluster";

/// The tag the image is built and run under.
///
/// It hashes the Dockerfile, the watchdog and the cluster wrapper - the three
/// files that decide what the image contains - so a recipe edit moves the tag.
#[must_use]
pub fn tag() -> String {
    let mut hasher = Sha256::new();
    for bytes in [
        include_bytes!("Dockerfile").as_slice(),
        include_bytes!("../watchdog.sh").as_slice(),
        include_bytes!("cluster.sh").as_slice(),
    ] {
        hasher.update(bytes);
    }
    let digest = format!("{:x}", hasher.finalize());
    digest[..12].to_string()
}

/// The image reference (`name:tag`) the shared cluster is run from.
#[must_use]
pub fn reference() -> String {
    format!("{NAME}:{}", tag())
}

/// The upstream Dragonfly image plus the shared watchdog and cluster wrapper.
///
/// The build is keyed to the content tag: when the daemon already carries that
/// tag the build is skipped, so every process of a run shares one build rather
/// than rebuilding it. Later calls in this process return the same tag without
/// asking the daemon again.
///
/// # Errors
/// When the Docker daemon cannot build the image.
pub fn build() -> Result<GenericImage, Box<dyn std::error::Error + Send + Sync>> {
    static BUILT: OnceLock<Result<(), String>> = OnceLock::new();
    let tag = tag();
    let outcome = BUILT.get_or_init(|| {
        if image_exists(&tag) {
            return Ok(());
        }
        GenericBuildableImage::new(NAME, tag.as_str())
            .with_dockerfile_string(include_str!("Dockerfile"))
            .with_data(include_bytes!("../watchdog.sh").to_vec(), "watchdog.sh")
            .with_data(include_bytes!("cluster.sh").to_vec(), "cluster.sh")
            .build_image()
            .map(|_| ())
            .map_err(|error| error.to_string())
    });
    match outcome {
        Ok(()) => Ok(GenericImage::new(NAME, tag.as_str())),
        Err(error) => Err(error.clone().into()),
    }
}

/// Whether the daemon already carries `name:tag`.
fn image_exists(tag: &str) -> bool {
    crate::shared::image_exists(&format!("{NAME}:{tag}"))
}
