//! The Dragonfly cluster image: the upstream image plus the shared watchdog and
//! the wrapper that runs every node as the watchdog's one tracked child.
//!
//! Built from a [`zeroship_testkit_server::image::Recipe`], which tags the image
//! by the content of the Dockerfile and every file it copies, so a worktree whose
//! recipe moved never overwrites another worktree's image and never runs a
//! server built from the wrong recipe.

use std::sync::OnceLock;

use zeroship_testkit_server::image::{Recipe, WATCHDOG_SCRIPT};

/// The recipe the image is built from.
pub const RECIPE: Recipe = Recipe {
    name: "zeroship-testkit-dragonfly-cluster",
    base: zeroship_testkit_server::images::DRAGONFLY_1_37,
    body: include_str!("Dockerfile"),
    files: &[("watchdog.sh", WATCHDOG_SCRIPT), ("cluster.sh", include_bytes!("cluster.sh"))],
};

/// The reference (`name:tag`) of the image, built when the daemon does not
/// already carry it.
///
/// Every process of a run shares one build, and later calls in this process
/// return the same reference without asking the daemon again.
///
/// # Errors
/// When the Docker daemon cannot build the image.
pub fn reference() -> Result<String, String> {
    static BUILT: OnceLock<Result<String, String>> = OnceLock::new();
    BUILT.get_or_init(|| RECIPE.build()).clone()
}
