//! Mandatory PostgreSQL integration tests, included in ordinary cargo test.
//!
//! Run tests/run_billing_suite.sh to prepare a migrated database and run the
//! suite. Fleet sweeps share state, so libtest defaults to serial execution
//! through .cargo/config.toml.
//!
//! With autotests disabled, register new database test modules below. Filter a
//! module with `cargo test -p zeroship-control --test live_db env_store::`.

mod common;

mod account_status_test;
mod app_logs_http_test;
mod app_oauth_client_test;
mod audit_retention_test;
mod authz_guard_oauth_test;
mod authz_guard_supabase_test;
mod billing_credit_test;
mod billing_dispute_test;
mod billing_invoice_payments_test;
mod billing_notify_test;
mod billing_proration_test;
mod billing_read_api_test;
mod billing_reconcile_test;
mod billing_redesign_regression_test;
mod billing_refund_void_test;
mod billing_safety_net_test;
mod billing_tax_test;
mod bootstrap_builder_test;
mod connect_fee_test;
mod app_delete_funnel_test;
mod archive_app_billing_history_test;
mod deploy_http_test;
mod deletion_owes_test;
mod deploy_test;
mod device_handlers_test;
mod egress_rules_test;
mod env_store;
mod internal_service_auth_test;
mod oauth_clients_test;
mod oauth_grants_handlers_test;
mod erasure_preflight_test;
mod organizations_test;
mod orphaned_app_reaper_test;
mod plan_catalog;
mod registry_schema_test;
mod reserved_app_names_test;
mod set_plan_authz_test;
mod spend;
mod spend_limit_http_test;
mod spend_reconcile_test;
mod stream_forwarder_recompute_test;
mod stripe_reconcile_test;
mod stripe_store;
mod stripe_webhook_test;
mod worker_enrolment_test;
mod worker_health_test;
mod workflow_instance_api_test;
mod workflow_plugin;
