//! The integration-test target a bare `cargo test -p zeroship-control` builds:
//! the files that pass with NO database.
//!
//! WHY THIS FILE EXISTS
//! --------------------
//! Cargo links one executable per `tests/*.rs`, and every one of those
//! executables statically links this crate and its whole dependency graph. The
//! 48 files in this directory cost 48 such executables. Measured on this tree
//! with `cargo test -p zeroship-control --features live-db-tests --no-run`,
//! 2026-08-20 (see the commit message for the after-figure):
//!
//!   before  48 integration executables, 11,168,086,856 bytes
//!
//! `tests/common/` was compiled once per binary (45 of them declared it) and is
//! now compiled three times, once per target that uses it.
//!
//! WHY FOUR TARGETS AND NOT ONE
//! ----------------------------
//! `required-features` is a property of a TARGET, so a file cannot be gated
//! separately from the target it belongs to. The five modules below are
//! deliberately ungated (see the "Live-database test gate" comment in
//! `Cargo.toml`): they pass with no database and must keep running under a bare
//! `cargo test --workspace`. The 41 files in `tests/live_db.rs` carry
//! `required-features = ["live-db-tests"]`. Merging those two would either gate
//! the ungated ones out of the default build or ungate the rest into it; both
//! destroy the gate.
//!
//! Two more files keep a `[[test]]` of their own because being a MODULE breaks
//! them, and neither was fixed by editing the test:
//!
//!   `workflow_engine_test`   clones the migrated database per test with
//!                            `CREATE DATABASE ... WITH TEMPLATE`, which
//!                            PostgreSQL refuses while any other session holds
//!                            the template open. Sibling modules of one binary
//!                            hold exactly that. Cargo runs targets one at a
//!                            time, so its own target keeps working the way it
//!                            does today.
//!   the residue guard        `tests/no_l*_residue_test.rs` greps this crate's
//!                            `src/`, `tests/` and `benches/` for the name of
//!                            the dead migration tool, exempting only its own
//!                            path. A `mod <that file>;` line in an entry file
//!                            IS an occurrence of the name, so declaring it here
//!                            would make the guard fail on its own declaration.
//!                            That is also why this comment spells the file with
//!                            a wildcard: naming it here would trip the same
//!                            check from the entry that declares nothing.
//!
//! WHAT THIS DOES NOT PROTECT AGAINST
//! ----------------------------------
//! Collapsing binaries buys link time and disk. It buys NO isolation, and it
//! SPENDS some: these modules now share one process, so anything one of them
//! corrupts process-wide is visible to its neighbours, where a separate
//! executable per file made that impossible. Specifically, and stated as what
//! was checked rather than what is hoped:
//!
//!   process environment  CHECKED. `std::env::set_var` / `remove_var` appear
//!                        nowhere in `crates/control/tests/`; the `set_var`
//!                        hits in `env_store.rs` are `EnvStore::set_var`, which
//!                        writes a Postgres row. Nothing here can perturb
//!                        another module's config.
//!   `static` gates       CHECKED, and this is the one that got WORSE. Eleven
//!                        `static Mutex` gates in this directory serialise a
//!                        module's own sweep-driving tests: SIX separate
//!                        `RECONCILE_LOCK`s (billing_credit, billing_proration,
//!                        billing_reconcile, billing_refund_void, billing_tax,
//!                        stripe_reconcile), TWO separate `SWEEP_LOCK`s (spend,
//!                        spend_reconcile), plus `FX_LOCK`, `TICK_GATE` /
//!                        `SPEND_TICK_GATE`, `REAPER_TEST_LOCK` and
//!                        bootstrap_builder's `LOCK`. As modules of one binary
//!                        they stay DISTINCT statics, so two modules driving the
//!                        SAME fleet-wide, advisory-locked sweep are no longer
//!                        kept apart by anything. The process boundary used to
//!                        do it for free: cargo runs test binaries one at a
//!                        time, never two at once. It cannot do it now.
//!
//!                        The gates do not even cover their own sweep. TEN
//!                        modules drive `billing_reconcile` (billing_credit,
//!                        billing_notify, billing_proration, billing_reconcile,
//!                        billing_redesign_regression, billing_refund_void,
//!                        billing_safety_net, billing_tax, spend,
//!                        stripe_reconcile - counted 2026-08-20 by grepping the
//!                        directory for the module path, not by reading the lock
//!                        names) and only six of them hold a `RECONCILE_LOCK`.
//!                        FIVE drive `SpendEngine`, of which two hold a
//!                        `SWEEP_LOCK`. So the ceiling on safety here is the
//!                        thread count, not the locks.
//!
//!                        `tests/run_billing_suite.sh` passes `--test-threads 1`
//!                        and always has, which is what makes the gated target
//!                        safe; a bare `cargo test -p zeroship-control
//!                        --features live-db-tests` does NOT pass it, and that
//!                        is the run where these gates are exposed. If that run
//!                        is to be made reliable, the fix is ONE gate per sweep
//!                        in `tests/common/`, not eleven private ones - which
//!                        edits test bodies and was deliberately left undone
//!                        here.
//!   shared `common`      CHECKED, a behaviour change, and the one that found a
//!                        REAL PRE-EXISTING BUG. Read this before "fixing" it.
//!
//!                        `common::isolated_closed_period_now()` memoises a
//!                        random far-future month in a `OnceLock`. `common` is
//!                        compiled once per TARGET, so its five callers
//!                        (billing_credit / billing_proration /
//!                        billing_reconcile / billing_refund_void / billing_tax)
//!                        used to draw five months and now share one. Each of
//!                        those tests asserts on rows keyed by its own app id,
//!                        so they are fine.
//!
//!                        `billing_safety_net_test` is NOT fine, and was not
//!                        before this merge either. It draws its OWN period
//!                        (`unique_closed_period_now`, a fresh `Uuid` per call,
//!                        2400 possible months) and then asserts
//!                        `subjects_checked == 1`. But `reconcile_pass` counts
//!                        EVERY subject in that period, summed over every meter
//!                        the period contains
//!                        (`cron/billing_reconcile.rs`, `add_safety_net_summary`
//!                        + the per-meter loop). The test owns its app; it does
//!                        NOT own its period. The assertion holds only while its
//!                        random draw misses every month another test seeded.
//!
//!                        MEASURED 2026-08-20, same script, same 78 seeding
//!                        tests, counting loaded periods (ones where a draw
//!                        would make `subjects_checked != 1`):
//!
//!                          main        6 loaded periods, worst 96,  128 subjects
//!                          this branch 2 loaded periods, worst 126, 128 subjects
//!
//!                        The 128 contaminating subjects are IDENTICAL. Merging
//!                        did not create them, it concentrated them: fewer
//!                        months to hit, more damage when hit. On the model of 4
//!                        draws against 2400 months that is ~1.0 percent per run
//!                        before and ~0.33 percent after - so a red run here is
//!                        REAL and roughly three times RARER than it was, not a
//!                        regression this merge introduced.
//!
//!                        The repair is per-test period ISOLATION, not a lock: a
//!                        lock cannot help, because the rows outlive it and the
//!                        collision is a later test DRAWING an earlier test's
//!                        month. Hand every caller a distinct month from one
//!                        process-global allocator in `tests/common/`, which
//!                        makes the count 0 by construction - and which only
//!                        BECOMES correct once these files share a process, as
//!                        they now do.
//!   ports, temp paths    CHECKED. No test binds a fixed port (the only literal
//!                        ports in the directory are `:5440` inside DSN
//!                        fallbacks), and every `std::env::temp_dir()` path is
//!                        suffixed with a fresh `Uuid`.
//!   global tracing       CHECKED. No `tracing_subscriber` init, no
//!                        `set_global_default`, anywhere in this directory.
//!   the database         NOT protected, and never was. Every live-DB module
//!                        shares one Postgres and one role; a module that leaves
//!                        rows behind changes what its neighbours see. That is
//!                        unchanged by this refactor, but it is now the ONLY
//!                        boundary left, so it carries more weight.
//!
//! HOW TO ADD A TEST FILE
//! ----------------------
//! `Cargo.toml` sets `autotests = false` and declares exactly four `[[test]]`
//! targets. A new `tests/<name>.rs` is compiled by NOTHING until it is listed in
//! one of them. Add `mod <name>;` here if it passes without a database, or in
//! `tests/live_db.rs` if it does not, in the SAME commit as the file - otherwise
//! its tests never run and nothing says so.
//!
//! HOW TO RUN A SUBSET
//! -------------------
//! `cargo test -p zeroship-control --test <file>` no longer resolves for a
//! merged file. The module path is a prefix of every test name, so filter on it:
//!
//!   cargo test -p zeroship-control --test main provider_conformance::

mod common;

mod authz_guard_supabase_test;
mod billing_pipeline_redpanda_e2e;
mod durable_workflows_keystone_e2e;
mod provider_conformance;
mod trusted_clients_test;
