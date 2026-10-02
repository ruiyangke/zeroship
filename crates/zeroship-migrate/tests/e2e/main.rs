//! The migration engine's end-to-end suites, linked into one test executable.
//!
//! These spawn `cargo` as a child process, so they live in their own `e2e` target
//! rather than the `integration` binary.

mod workspace_shape;
