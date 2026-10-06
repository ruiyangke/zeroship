//! The end-to-end suites for `zeroship-data-cdc-server`.
//!
//! `autotests = false`, so a new suite is compiled only once it is declared
//! here.

mod boot;
mod low_memlock;
mod relay;
