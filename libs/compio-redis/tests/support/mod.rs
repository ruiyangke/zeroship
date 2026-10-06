//! The shared Redis and Dragonfly fixture every test process of the worktree
//! joins, reached through `zeroship-testkit`.
//!
//! `zeroship-testkit`'s `redis` module hands out only URLs, ports and a
//! minted prefix and carries no dependency on this crate (`compio-redis`),
//! which is what lets this dev-dependency point back at it: `cargo xtask test
//! repository`'s `no_crate_dev_depends_on_a_package_that_links_it`
//! forbids the edge shape where a package's normal closure reaches a crate
//! whose own tests dev-depend on it, and a `RedisConfig` built inside
//! the testkit would be exactly that edge.

pub use zeroship_testkit::redis::case_prefix;

use compio_redis::{Client, Pool};

/// The shared Redis and Dragonfly servers' plain connection strings.
pub struct Fixtures;

#[must_use]
pub fn fixtures() -> Fixtures {
    Fixtures
}

impl Fixtures {
    #[must_use]
    pub fn redis_url(&self) -> String {
        zeroship_testkit::redis::redis().url()
    }

    #[must_use]
    pub fn cluster_urls(&self) -> Vec<String> {
        zeroship_testkit::redis::cluster().urls()
    }
}

pub async fn connect(url: &str) -> Client {
    Client::connect(url)
        .await
        .expect("connect to the shared Redis fixture")
}

pub async fn connect_pool(url: &str, size: usize) -> Pool {
    Pool::connect(url, size)
        .await
        .expect("connect pool to the shared Redis fixture")
}
