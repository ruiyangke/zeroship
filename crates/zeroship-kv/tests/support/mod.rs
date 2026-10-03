//! Redis and Dragonfly fixtures for the KV suites.
//!
//! The servers are the shared [`zeroship_testkit::redis`] fixture; the suites
//! reach them through `crate::support` exactly as they reach any other helper.

pub use zeroship_testkit::redis::fixtures;
