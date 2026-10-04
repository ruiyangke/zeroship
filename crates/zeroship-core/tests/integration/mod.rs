//! Core integration suites linked into one test executable.
//!
//! `autotests` is off, so a new `tests/integration/<name>.rs` is compiled by
//! nothing until it is declared here.

mod auth_test;
mod compile_fail;
mod generated_secret_scrape;
mod schema_name;
mod service_assertion_test;
mod service_authorization_test;
mod service_identity_test;
mod service_peers_test;
mod superjson_test;
mod tls_provider_test;
mod types_test;
mod user_id_test;
mod workflow_jobs_test;
mod workflow_policy;
mod workflow_schedules_test;
