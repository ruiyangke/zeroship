//! The real-world npm-package integration target for `zeroship-runtime`.
//!
//! These six modules drive unmodified npm clients (`pg`, `mysql2`, `ioredis`,
//! `memjs`, node `http`, and the reconnect suite) against live servers. They
//! were six separate executables; they are merged here rather than into
//! `tests/main.rs` because of the helper they share.
//!
//! `support/node_realworld.rs` owns `lock_env()`, a `static ENV_LOCK` that
//! serialises `std::env::set_var` / `remove_var` for `ZEROSHIP_DEV` and
//! `ZEROSHIP_NET_GLOBAL_MAX_SOCKETS` and restores the previous values on
//! `EnvGuard::drop`. Each of the six used to reach it with its own
//! `#[path = "support/node_realworld.rs"] mod node_realworld;`. In separate
//! processes that was six copies of one file and it did not matter. In one
//! process it would be six copies of the STATIC - six independent mutexes over
//! a single process environment, i.e. no mutual exclusion at all, with each
//! module free to restore `ZEROSHIP_DEV` to unset while another is mid-request
//! under it. The entry below declares the helper exactly once and the six
//! modules `use crate::node_realworld`, so there is one lock again.
//!
//! WHAT THIS DOES NOT PROTECT AGAINST
//! ----------------------------------
//! The single lock only covers code that TAKES it. Any module added below that
//! mutates the environment without `lock_env()` races every one of these, and
//! the failure surfaces as an unrelated connection being refused rather than
//! as a lock error. That is also why `dev_auth`, `node_net`,
//! `node_net_security`, `node_pg_e2e` and `node_tls` are still their own
//! targets: each carries a private `ENV_LOCK` of its own, and putting any of
//! them in here would reintroduce exactly the two-mutexes-one-environment
//! shape this file exists to remove.
//!
//! These tests need live servers and they do NOT skip without them: the
//! `ensure_*` helpers `docker start` the container and then panic if the port
//! never opens. A red run here can therefore mean "the container is not on
//! this machine" rather than "the runtime regressed" - read the panic message
//! before believing either.
//!
//! That container dependence already showed itself across the merge.
//! `node_pg_tls_e2e` FAILED on the pre-merge run of 2026-08-20 ("Connection
//! terminated unexpectedly", 500 not 200) and passed on both post-merge runs.
//! Nothing in the merge fixed it and nothing here should be read as having
//! fixed it - the TLS Postgres container was up the second time. It is the
//! clearest example in this crate of a test whose verdict is about the machine
//! rather than about the code.
//!
//! Second isolation loss, specific to this target: each of the six files held
//! exactly ONE test, so cargo used to run them as six sequential processes.
//! They are now six threads in one process and run CONCURRENTLY. `lock_env()`
//! serialises the environment between them; nothing serialises the `docker
//! start` calls, and nothing stops two of them contending for the same
//! container if a future module reuses one.
//!
//! `Cargo.toml` sets `autotests = false`: a new file is compiled by nothing
//! until it is listed below.

#[path = "support/node_realworld.rs"]
mod node_realworld;

mod node_http_e2e;
mod node_ioredis_e2e;
mod node_memjs_e2e;
mod node_mysql2_e2e;
mod node_pg_tls_e2e;
mod node_reconnect_e2e;
