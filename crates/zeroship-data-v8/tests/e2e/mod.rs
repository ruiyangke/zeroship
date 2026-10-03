//! The end-to-end suites for `zeroship-data-v8`.
//!
//! These launch the real CDC relay and own their PostgreSQL container, so they
//! are declared as `e2e` modules of the single `main` test target.

mod distributed_live;
