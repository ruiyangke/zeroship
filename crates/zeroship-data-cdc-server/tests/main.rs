//! The `zeroship-data-cdc-server` test suites in one binary. PostgreSQL is
//! mandatory.
//!
//! Both suites launch the relay as a real process, so they are declared as
//! `e2e` modules. `boot` states a process-wide crypto provider, so its case is
//! ignored in the shared run and executed alone in a child copy of this binary
//! by its spawner.

mod e2e;
