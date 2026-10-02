//! Separate-process relay tests. PostgreSQL is mandatory.
//!
//! Registering the suites here keeps one binary per crate. `boot` states a
//! process-wide crypto provider, so its case is ignored in the shared run and
//! executed alone in a child copy of this binary by its spawner.

mod boot;
mod relay;
