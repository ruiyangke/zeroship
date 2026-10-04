//! The MySQL image the shared server runs: the upstream image plus the shared
//! shell watchdog and the `flock` the watchdog and the blind-container refusal
//! need.
//!
//! Built through [`zeroship_shared_server::image::build`], which tags the image
//! by the content of the Dockerfile and every file it copies, so a worktree whose
//! recipe moved never overwrites another worktree's image and never runs a
//! server built from the wrong recipe.

use std::sync::OnceLock;

/// The image name every worktree builds under; the tag carries the content hash.
const NAME: &str = "zeroship-testkit-mysql";

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
    BUILT
        .get_or_init(|| {
            zeroship_shared_server::image::build(
                NAME,
                include_str!("Dockerfile"),
                &[
                    ("watchdog.sh", zeroship_shared_server::image::WATCHDOG_SCRIPT),
                ],
            )
        })
        .clone()
}
