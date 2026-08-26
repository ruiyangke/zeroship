//! The integration-test target behind `--features live-db-tests`: the files
//! that CANNOT run without a reachable, migrated PostgreSQL.
//!
//! One `[[test]]` carrying `required-features = ["live-db-tests"]` gates all 41
//! modules below exactly as 41 separate `[[test]]` blocks did. See
//! `tests/main.rs` for why the collapse stops at four targets rather than one,
//! for the measured before-figure, and for the full list of what sharing a
//! process does NOT protect against. The "Live-database test gate" comment in
//! `Cargo.toml` says what the gate encodes and how the set was determined.
//!
//! WHAT THIS DOES NOT PROTECT AGAINST
//! ----------------------------------
//! The short form of `tests/main.rs`'s list, because it bites hardest here:
//! these 41 modules share ONE process and ONE database. The eleven `static
//! Mutex` gates in this directory are per-MODULE, so two modules driving the
//! same fleet-wide, advisory-locked sweep - six separate `RECONCILE_LOCK`s, two
//! separate `SWEEP_LOCK`s - are no longer held apart by anything. Until now the
//! PROCESS did it: cargo runs test binaries one at a time and never two at once.
//!
//! `common` is compiled once for the whole target, so a `static` in it is shared
//! by all 41 modules rather than being one cell per binary. The billing period
//! helper was such a `static`; merging collapsed five memoised months into one
//! and exposed a bug older than the merge, in which a test asserted on a
//! period-wide count it did not own. That is FIXED - every caller now reserves a
//! private window from `common::next_isolated_period()`, whose header explains
//! why a lock cannot do the same job. The general lesson stands for the next
//! `static` added here: sharing a process makes one visible to 41 modules.
//!
//! THREADING
//! ---------
//! `tests/run_billing_suite.sh` runs this target with `--test-threads 1` and has
//! always had to: `workflow_instance_api_test` drives advisory-locked engine
//! ticks and asserts on claim counts, so sibling tests steal each other's claims
//! under the default pool. Collapsing the targets WIDENS the set of tests that
//! flag protects, from one binary's to this one's; it does not introduce a new
//! requirement, but it does mean a bare `cargo test -p zeroship-control
//! --features live-db-tests` - which passes no such flag - now exposes the
//! cross-module gates described above.
//!
//! HOW TO ADD A TEST FILE
//! ----------------------
//! `autotests = false`, so a new `tests/<name>.rs` is compiled by NOTHING until
//! `mod <name>;` appears below (or in `tests/main.rs` if it needs no database).
//! Add it in the same commit as the file, or its tests never run and nothing
//! says so.
//!
//! HOW TO RUN A SUBSET
//! -------------------
//! `cargo test -p zeroship-control --test <file>` no longer resolves. The module
//! path is a prefix of every test name, so filter on it:
//!
//!   cargo test -p zeroship-control --features live-db-tests \
//!       --test live_db env_store::

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
mod delete_app_billing_fk_test;
mod deploy_http_test;
mod deploy_test;
mod device_handlers_test;
mod egress_rules_test;
mod env_store;
mod identity_bridge_test;
mod oauth_clients_test;
mod oauth_grants_handlers_test;
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
mod workflow_instance_api_test;
mod workflow_plugin;
