//! The `zeroship-workflow-manager` test suites in one binary.
//!
//! The async integration cases need this limit on their own crate root.

#![recursion_limit = "256"]

mod support;

mod integration;
