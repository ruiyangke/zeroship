//! A stock image with the testkit-server watchdog on top.
//!
//! A server kind whose image no fixture otherwise customises - a stock
//! `PostgreSQL` major, say - still needs the watchdog as its PID 1 to be a
//! shared server. [`with_watchdog`] builds `FROM <base>` plus the watchdog, tagged by
//! the content of everything that builds it, so a worktree whose watchdog moved
//! never runs a server built from another worktree's recipe.
//!
//! A build of one reference is serialised across processes by an exclusive
//! `flock` under the worktree's `target`: a daemon that does not yet carry the
//! image gets one build, not one per process that found it absent.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use sha2::{Digest, Sha256};
use testcontainers::{runners::SyncBuilder, GenericBuildableImage};

/// The image name every derived image is built under; the tag carries the
/// content hash, which covers the base.
pub const NAME: &str = "zeroship-testkit-watchdog";

/// Where the build locks live, under the worktree's `target`.
const LOCK_DIR: &str = "target/zeroship-testkit-image-locks";

/// How long a process waits for another to finish building one reference.
const BUILD_LOCK_WAIT: Duration = Duration::from_mins(10);

/// Where the watchdog sits in a derived image.
pub const WATCHDOG: &str = "/usr/local/bin/zeroship-watchdog";

/// The watchdog script itself, for a fixture whose own Dockerfile copies it in
/// beside its customisations rather than building on [`with_watchdog`].
pub const WATCHDOG_SCRIPT: &[u8] = include_bytes!("watchdog.sh");

/// The image references this process has started a build for, in order.
///
/// [`record_build`] appends a reference immediately before `build_image`, so a
/// concurrency test counts one reference's builds even if the process builds
/// others at the same time. A reference the daemon already carries is never
/// appended: no build ran.
static BUILDS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// The image references this process has started a build for.
///
/// The test-visible build counter: a concurrency test counts its own unique
/// reference here, so the count never depends on timing.
///
/// # Panics
/// When the build log is poisoned, which no path in this crate does.
#[must_use]
pub fn built_images() -> Vec<String> {
    BUILDS
        .lock()
        .expect("the build log is never poisoned")
        .clone()
}

/// Record that this process is about to build `reference`.
fn record_build(reference: &str) {
    BUILDS
        .lock()
        .expect("the build log is never poisoned")
        .push(reference.to_owned());
}

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
/// The build is skipped when the daemon already carries the tag, and a build of
/// that tag is serialised across processes: the first process builds it, and the
/// ones that found it absent wait on the lock and then find it present.
///
/// # Errors
/// When the Docker daemon cannot build the image, or another process holds the
/// build lock past [`BUILD_LOCK_WAIT`].
pub fn with_watchdog(base: &str) -> Result<String, String> {
    let tag = tag(base);
    let reference = format!("{NAME}:{tag}");
    build_locked(&reference, || {
        GenericBuildableImage::new(NAME, tag.as_str())
            .with_dockerfile_string(dockerfile(base))
            .with_data(WATCHDOG_SCRIPT.to_vec(), "watchdog.sh")
            .build_image()
            .map(|_| ())
            .map_err(|error| format!("could not build {base} with the watchdog: {error}"))
    })?;
    Ok(reference)
}

/// A Dockerfile recipe built on a base image from [`crate::images`].
///
/// `body` is the Dockerfile without its `FROM` line, which the recipe prepends
/// from `base`, so the base is named in that one list and the tag moves when the
/// base does.
#[derive(Clone, Copy, Debug)]
pub struct Recipe {
    /// The image name the recipe builds under; the tag carries the content hash.
    pub name: &'static str,
    /// The image the recipe builds on.
    pub base: crate::images::Image,
    /// The Dockerfile after its `FROM` line.
    pub body: &'static str,
    /// The files the Dockerfile copies in, as (path in the context, contents).
    pub files: &'static [(&'static str, &'static [u8])],
}

impl Recipe {
    fn dockerfile(&self) -> String {
        format!("FROM {}\n{}", self.base, self.body)
    }

    /// The reference (`name:tag`) the recipe builds under, without building.
    #[must_use]
    pub fn reference(&self) -> String {
        reference(self.name, &self.dockerfile(), self.files)
    }

    /// The reference (`name:tag`) of the recipe's image, built when the daemon
    /// does not already carry it.
    ///
    /// # Errors
    /// As [`build`].
    pub fn build(&self) -> Result<String, String> {
        build(self.name, &self.dockerfile(), self.files)
    }
}

/// The reference (`name:tag`) the image `dockerfile` builds from `files` is
/// tagged under: the content hash of the Dockerfile and every file handed to
/// the build, names included.
fn reference(name: &str, dockerfile: &str, files: &[(&str, &[u8])]) -> String {
    format!("{name}:{}", content_tag(dockerfile, files))
}

/// The content hash a recipe is tagged with.
fn content_tag(dockerfile: &str, files: &[(&str, &[u8])]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(dockerfile.as_bytes());
    for (path, bytes) in files {
        hasher.update(path.as_bytes());
        hasher.update([0]);
        hasher.update(bytes);
    }
    format!("{:x}", hasher.finalize())[..12].to_owned()
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
/// When the Docker daemon cannot build the image, or another process holds the
/// build lock past [`BUILD_LOCK_WAIT`].
pub fn build(name: &str, dockerfile: &str, files: &[(&str, &[u8])]) -> Result<String, String> {
    let tag = content_tag(dockerfile, files);
    let reference = format!("{name}:{tag}");
    build_locked(&reference, || {
        let mut image =
            GenericBuildableImage::new(name, tag.as_str()).with_dockerfile_string(dockerfile);
        for (path, bytes) in files {
            image = image.with_data(bytes.to_vec(), *path);
        }
        image
            .build_image()
            .map(|_| ())
            .map_err(|error| format!("could not build {name}: {error}"))
    })?;
    Ok(reference)
}

/// Build `reference` once across every process of the worktree.
///
/// The absent check is repeated after the exclusive lock is taken: the process
/// that built the image releases the lock on return, and the waiters find the
/// image present and return without building. The lock file is keyed by
/// `reference` under the worktree's `target`, so two worktrees never contend.
///
/// The held lock is an open file on the stack: returning, a build error and a
/// panic all drop it and release the `flock`.
fn build_locked(reference: &str, build: impl FnOnce() -> Result<(), String>) -> Result<(), String> {
    if crate::image_exists(reference) {
        return Ok(());
    }
    let path = lock_path(reference)?;
    let lock = crate::open_lock(&path)?;
    crate::lock_exclusive(&lock, BUILD_LOCK_WAIT, "image build lock")?;
    if crate::image_exists(reference) {
        return Ok(());
    }
    record_build(reference);
    build()
}

/// The lock file one image reference is built under.
fn lock_path(reference: &str) -> Result<PathBuf, String> {
    let dir = crate::root().join(LOCK_DIR);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
    let mut hasher = Sha256::new();
    hasher.update(reference.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    Ok(dir.join(format!("{}.lock", &digest[..12])))
}

#[cfg(test)]
mod tests {
    use super::Recipe;
    use crate::images::{POSTGRES_16, POSTGRES_18};

    const RECIPE: Recipe = Recipe {
        name: "zeroship-recipe-fixture",
        base: POSTGRES_16,
        body: "RUN true\n",
        files: &[("watchdog.sh", b"one")],
    };

    /// A recipe's reference is its name and a content tag that moves with the
    /// base, the body and every copied file, and with nothing else: CI keys its
    /// image cache on it and the shards find the image the plan job built by it.
    #[test]
    fn a_recipe_reference_moves_with_its_content_and_only_with_it() {
        let reference = RECIPE.reference();
        let (name, tag) = reference.split_once(':').expect("name:tag");
        assert_eq!(name, RECIPE.name);
        assert!(
            tag.len() == 12 && tag.chars().all(|c| c.is_ascii_hexdigit()),
            "{tag} is not a content tag"
        );
        assert_eq!(RECIPE.reference(), reference, "the reference is deterministic");
        let moved = [
            Recipe { base: POSTGRES_18, ..RECIPE },
            Recipe { body: "RUN false\n", ..RECIPE },
            Recipe { files: &[("watchdog.sh", b"two")], ..RECIPE },
            Recipe { files: &[("other.sh", b"one")], ..RECIPE },
        ];
        for recipe in moved {
            assert_ne!(recipe.reference(), reference, "{recipe:?} must move the tag");
        }
    }
}
