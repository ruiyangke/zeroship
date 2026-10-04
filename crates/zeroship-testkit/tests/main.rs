//! The `zeroship-testkit` single test target.
//!
//! `tests/integration/` is a module of this binary, never a second target, so
//! the shared-server contract and its child-process spawner share one process
//! image and the spawner names the children by their module path.

mod integration;
