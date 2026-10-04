//! Redis and Dragonfly fixtures for the KV suites.
//!
//! The servers are the shared `zeroship_testkit::redis` fixture; it hands out
//! only URLs, ports and a minted prefix (see its module doc) and carries no
//! dependency on `compio-redis`, so this module builds the typed
//! `RedisConfig` itself from its plain endpoints.

pub use zeroship_testkit::redis::case_prefix;

/// The shared Redis and Dragonfly servers, typed for this crate's own
/// `Redis` backend.
pub struct Fixtures;

#[must_use]
pub fn fixtures() -> Fixtures {
    Fixtures
}

impl Fixtures {
    /// The shared standalone server's configuration.
    #[must_use]
    pub fn redis_config(&self) -> zeroship_kv::RedisConfig {
        zeroship_kv::RedisConfig::new(zeroship_kv::Topology::Standalone {
            endpoint: zeroship_testkit::redis::redis().endpoint(),
        })
    }

    /// The shared cluster's configuration.
    #[must_use]
    pub fn cluster_config(&self) -> zeroship_kv::RedisConfig {
        zeroship_kv::RedisConfig::new(zeroship_kv::Topology::Cluster {
            seeds: zeroship_testkit::redis::cluster().endpoints(),
        })
    }
}
