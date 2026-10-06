//! The `zeroship-testkit-server` test executable.
//!
//! `tests/integration/` is a module of this binary, never a second target, so
//! the protocol contract and its child-process spawner share one process image
//! and the spawner names the children by their module path.

mod integration;
