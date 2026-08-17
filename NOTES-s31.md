# s31 - live-db test binaries that no gate runs

Scratch notes. Committed as they land, not when a conclusion exists.

## Instrument

- `ZEROSHIP_REQUIRE_LIVE_BACKENDS=1` turns a self-skip into a hard failure.
  Used on TARGETED runs only; on a whole-suite run it also panics on the one
  legitimate allowlisted skip (`AUTH_TEST_SMTP_SINK`).
- Skip announcements carry `ZEROSHIP-TEST-SKIPPED` (tests/lib/skip_census.sh:38).
- Live PG confirmed on 127.0.0.1:5440 (PostgreSQL 16.14, `wal_level=replica`).

## TARGETS: 45 test targets carry `required-features = ["live-db-tests"]`

Source: the `[[test]]` blocks in the three Cargo.toml files.

- crates/control/Cargo.toml   -> 43 targets (lines 164-375)
- crates/plugin-db/Cargo.toml -> 1 target (`distributed_live`, line 205)
- crates/migrated/Cargo.toml  -> 1 target (`apply_api_test`, line 83)

Note: ci.yml:519 and ci.yml:604 both say "44 zeroship-control integration
binaries" and "45 targets". The measured control count today is 43, so the
total is 45 only if plugin-db's `distributed_live` is counted -- which those
comments do not do (they say 44 control + 1 migrated). Recorded as a
discrepancy to resolve, not yet a finding.

### control (43)

account_status_test, admin_handlers_test, app_logs_http_test,
app_oauth_client_test, audit_retention_test, authz_guard_oauth_test,
billing_credit_test, billing_dispute_test, billing_invoice_payments_test,
billing_notify_test, billing_proration_test, billing_read_api_test,
billing_reconcile_test, billing_redesign_regression_test,
billing_refund_void_test, billing_safety_net_test, billing_tax_test,
bootstrap_builder_test, connect_fee_test, delete_app_billing_fk_test,
deploy_http_test, deploy_test, device_handlers_test, env_store,
identity_bridge_test, oauth_grants_handlers_test, oauth_handlers_test,
orphaned_app_reaper_test, plan_catalog, pricing_config_test,
registry_schema_test, set_plan_authz_test, spend, spend_limit_http_test,
spend_reconcile_test, stream_forwarder_recompute_test, stripe_reconcile_test,
stripe_store, stripe_webhook_test, token_handlers_test, workflow_engine_test,
workflow_instance_api_test, workflow_plugin

### control test files that are NOT gated (run in `cargo test --workspace`)

authz_guard_supabase_test, billing_pipeline_redpanda_e2e,
durable_workflows_keystone_e2e, no_liquibase_residue_test,
provider_conformance, trusted_clients_test

## Gates found

- tests/run_billing_suite.sh:217 `cargo test -p zeroship-control --features
  live-db-tests` -- NO NAME LIST, so it builds and runs every control target
  whose required-features are met. Same at :227 for zeroship-migrated.
  Exports CONTROL_TEST_DB / AUTH_DB_URL / MIGRATED_TEST_DB, all one DSN.
  Invoked by ci.yml `billing-gate`.
- tests/run_plugin_db_live_suite.sh:129 names `distributed_live` explicitly and
  exports LIVE_DB_TEST_URL. Invoked by ci.yml `plugin-db-live-gate`.
- tests/e2e_durable_workflows.sh:745 also runs `workflow_engine_test`.

So on NAMES, all 45 are reached. The open question is the trap: which of them
actually execute against the database rather than self-skipping inside a run
that still reports passes.

## The `live-db-tests` set is NOT where the hole is

Static reading says every one of the 45 is reached, and by construction rather
than by a name list:

- `cargo test -p zeroship-control --features live-db-tests` runs ALL 43 control
  targets. Only three of them can self-skip at all (workflow_engine_test:238,
  workflow_instance_api_test x5, workflow_plugin.rs:577), and all three gate on
  `CONTROL_TEST_DB`, which run_billing_suite.sh:116 exports. Every other gated
  control target resolves its DSN with a hardcoded fallback to
  `postgresql://postgres:zeroship@localhost:5440/zeroship_billing_test` and
  PANICS if nothing answers - it cannot report a hollow pass.
- Same for zeroship-migrated (`MIGRATED_TEST_DB`, exported at :118).
- plugin-db's `distributed_live` is named at run_plugin_db_live_suite.sh:129
  with `LIVE_DB_TEST_URL` exported at :66.

The brief's premise -- "authz_guard_oauth_test ... no gate runs it" -- does not
hold against the tree as it stands: that binary is inside the by-construction
invocation at run_billing_suite.sh:217, which ci.yml `billing-gate` runs on
every push. To be confirmed by measurement, not just by reading.

## THE ACTUAL HOLE: live-DB binaries that are NOT feature-gated

Being outside `live-db-tests` is what makes a binary invisible. Such a target
IS built by `cargo test --workspace` in the `rust` job, runs with no DSN,
self-skips, and counts as PASSED. The `rust` job's skip census REPORTS this
(ci.yml:583) but deliberately does not fail.

Cross-referencing every workspace test file that reads a DSN through
`test_env!` against the gate scripts:

| binary | var | tests gated | who runs it with a DSN |
| --- | --- | --- | --- |
| crates/authn/tests/service_replay_pg_test.rs | AUTH_DB_URL | 6 of 6 | NOBODY |
| crates/gateway/tests/db_pool_smoke.rs | GATEWAY_POOL_SMOKE_URL | 1 of 1 | NOBODY |
| crates/zeroship-migrate-adapter/tests/smoke_apply_pg.rs | ZERO_MIGRATE_TEST_PG_URL | 1 of 1 | NOBODY |
| crates/zeroship-migrate-adapter/tests/author_and_apply_pg.rs | ZERO_MIGRATE_TEST_PG_URL | 1 of 2 | NOBODY |
| crates/zeroship-migrate-adapter/tests/platform_migrate.rs | ZERO_MIGRATE_TEST_PG_URL | 9 | named by ci.yml but always skips (ci.yml:571 says so) |

Evidence for "NOBODY", each an independent grep over tests/ .github/ deploy/:

- `GATEWAY_POOL_SMOKE_URL` appears in the whole repo only at
  docs/reference/env-vars.md:608. No script, no workflow sets it.
- `ZERO_MIGRATE_TEST_PG_URL` appears only at ci.yml:571 (a comment saying it is
  set nowhere) and docs/reference/env-vars.md:607.
- `AUTH_DB_URL` IS exported (run_auth_suite.sh:69), but that script runs
  zeroship-auth, then a hand-maintained name list at :150-158 covering
  zeroship-authz, zeroship-mailer and six zeroship-gateway targets.
  `zeroship-authn` is not in it, and `zeroship-authn` appears NOWHERE in tests/
  or .github/. That name list is exactly the drift-prone structure
  run_billing_suite.sh removed.

Control on the pattern (an empty grep is not proof): the same search DOES find
the covered ones - `zeroship-gateway:oidc_rp_e2e` at run_auth_suite.sh:158,
`distributed_live` at run_plugin_db_live_suite.sh:129 - so the pattern finds
binaries that are gated, and the misses above are real misses.
