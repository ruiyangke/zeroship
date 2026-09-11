#![recursion_limit = "256"]
//! Workflow API and control-plane binding tests with owned backing services.
mod common;
#[path = "workflow_support/postgres.rs"]
mod workflow_postgres;
mod workflow_instance_api_test;
mod workflow_plugin;
mod workflow_provisioning;
