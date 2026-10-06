//! The Redpanda image the shared broker runs: the upstream broker image plus
//! the shared shell watchdog. The Redpanda entrypoint execs `rpk`, so
//! `Entrypoint::Watchdog` makes the watchdog the container's entrypoint rather
//! than passing it through the image's.
//!
//! Built from a [`zeroship_testkit_server::image::Recipe`], which tags the image
//! by the content of the Dockerfile and every file it copies, so a worktree whose
//! recipe moved never overwrites another worktree's image and never runs a
//! server built from the wrong recipe.

use std::sync::OnceLock;

use zeroship_testkit_server::image::{Recipe, WATCHDOG_SCRIPT};

/// The recipe the image is built from.
pub const RECIPE: Recipe = Recipe {
    name: "zeroship-testkit-redpanda",
    base: zeroship_testkit_server::images::REDPANDA,
    body: include_str!("Dockerfile"),
    files: &[("watchdog.sh", WATCHDOG_SCRIPT)],
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
