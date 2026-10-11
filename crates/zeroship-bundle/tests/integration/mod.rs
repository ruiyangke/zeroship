//! Bundle integration suites linked into one test executable.
//!
//! `autotests` is off, so a new `tests/integration/<name>.rs` is compiled by
//! nothing until it is declared here.

mod blob_test;
mod executable_test;
mod hostile_archive_test;
mod manifest_test;
mod runtime_descriptor_ingest_test;
// The suite joins the shared S3 gateway container, which only the Linux
// runners can start.
#[cfg(target_os = "linux")]
mod s3_blob_store;
mod store_test;
mod zstd_window_probe;
