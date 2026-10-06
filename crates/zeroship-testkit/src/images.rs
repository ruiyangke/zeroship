//! Every container image a test fixture starts or builds on, and the testkit
//! recipes CI builds ahead of the tests.
//!
//! The base references are `zeroship_shared_server::images`, re-exported here.
//! [`FETCHING`] names the recipes whose build installs packages from a
//! distribution's mirrors: CI's plan job builds them once, tagged by their
//! content hash, and restores the built images beside the base images, so no
//! test job reaches a package mirror or a registry.

pub use zeroship_shared_server::images::*;

use zeroship_shared_server::image::Recipe;

/// The recipes whose build fetches packages over the network: the `PostgreSQL`
/// image installs `PostGIS` from the distribution, the `MySQL` image installs
/// the `flock` its watchdog checks for, and the S3 gateway image installs
/// util-linux's `flock`.
pub const FETCHING: &[Recipe] = &[
    crate::postgres::RECIPE,
    crate::mysql::RECIPE,
    crate::s3::RECIPE,
];
