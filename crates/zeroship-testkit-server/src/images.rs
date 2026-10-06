//! Every container image a test fixture starts, or builds its own image on,
//! including the reaper the JavaScript suites' testcontainers starts.
//!
//! This list is the one place a fixture's image is named. CI restores exactly
//! these images from its cache, keyed on this list (`cargo xtask images list`),
//! and points the Docker daemon's registry traffic at a closed port before the
//! tests start, so a fixture whose image is missing here fails in CI naming the
//! image instead of pulling it from a public registry.
//!
//! Every reference is `name:tag` with no digest: `docker save` and
//! `docker load` carry an image's tags across, but not the repository digest a
//! `name@sha256:...` reference resolves through, so a digest reference would be
//! absent from a daemon the cache loaded.

/// One image reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Image {
    /// The repository, with its registry host when it is not Docker Hub.
    pub name: &'static str,
    /// The tag.
    pub tag: &'static str,
}

impl Image {
    /// The `name:tag` reference.
    #[must_use]
    pub fn reference(&self) -> String {
        self.to_string()
    }

    /// The image as a testcontainers image to configure and start. A fixture
    /// that does not start its image itself names it by [`Image::reference`].
    pub fn generic(self) -> testcontainers::GenericImage {
        testcontainers::GenericImage::new(self.name, self.tag)
    }
}

impl std::fmt::Display for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.name, self.tag)
    }
}

/// Declare each image constant and [`ALL`] from one list, so no constant can be
/// left out of what the cache restores.
macro_rules! images {
    ($($(#[$doc:meta])* $constant:ident = $name:literal : $tag:literal;)+) => {
        $(
            $(#[$doc])*
            pub const $constant: Image = Image { name: $name, tag: $tag };
        )+
        /// Every image above.
        pub const ALL: &[Image] = &[$($constant),+];
    };
}

images! {
    /// The major below the tenant fence's floor, for the bootstrap's refusal.
    POSTGRES_15_ALPINE = "postgres":"15-alpine";
    /// The major the platform deploys.
    POSTGRES_16 = "postgres":"16";
    POSTGRES_17 = "postgres":"17";
    POSTGRES_18 = "postgres":"18";
    /// The base of the `PostgreSQL` image the platform and the bare server share.
    PGVECTOR_16 = "pgvector/pgvector":"pg16";
    MYSQL_8_4 = "mysql":"8.4";
    REDIS_7 = "redis":"7";
    /// The base of the shared Redis-protocol fixture servers.
    DRAGONFLY_1_37 = "docker.dragonflydb.io/dragonflydb/dragonfly":"v1.37.0";
    /// The KV topology suites' own servers, which configure cluster announcement.
    DRAGONFLY_2_0 = "docker.dragonflydb.io/dragonflydb/dragonfly":"v2.0.0";
    REDPANDA = "docker.redpanda.com/redpandadata/redpanda":"v26.2.2";
    NGINX_1 = "nginx":"1";
    MEMCACHED_1_6 = "memcached":"1.6";
    VERSITYGW = "ghcr.io/versity/versitygw":"v1.3.0";
    MAILPIT = "axllent/mailpit":"v1.31.1";
    /// The reaper testcontainers-node starts beside the containers a JavaScript
    /// suite starts; its tag is the one the installed testcontainers names.
    RYUK = "testcontainers/ryuk":"0.14.0";
}

#[cfg(test)]
mod tests {
    use super::ALL;
    use std::collections::BTreeSet;

    /// Each reference is one `name:tag` the cache can carry, and none is
    /// listed twice.
    #[test]
    fn every_reference_is_a_distinct_digest_free_name_and_tag() {
        assert!(!ALL.is_empty(), "the list names no image");
        let mut seen = BTreeSet::new();
        for image in ALL {
            assert!(
                !image.name.is_empty() && !image.tag.is_empty(),
                "{image:?} lacks a name or a tag"
            );
            assert!(
                !image.reference().contains('@'),
                "{image:?} carries a digest, which `docker load` cannot restore"
            );
            assert!(
                !image.tag.contains(':') && !image.tag.contains('/'),
                "{image:?}'s tag is not a single tag"
            );
            assert!(seen.insert(image.reference()), "{image:?} is listed twice");
        }
    }
}
