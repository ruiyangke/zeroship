//! The real-world npm-package integration target for `zeroship-runtime`.
//!
//! These six modules drive unmodified npm clients (`pg`, `mysql2`, `ioredis`,
//! `memjs`, node `http`, and the reconnect suite) against live servers. They are
//! merged here rather than into `tests/main.rs` because of the helper they share.
//!
//! `support/node_realworld.rs` owns `lock_env()`, a `static ENV_LOCK` that
//! serialises the runtime's process-wide dev-mode and global-socket-cap cells
//! and restores the previous values on `SettingsGuard::drop`. The entry below
//! declares the helper exactly once and the six modules `use
//! crate::node_realworld`, so there is one lock over one set of cells. Six
//! copies of the static would be six independent mutexes over the same
//! process-wide cells, i.e. no mutual exclusion at all, with each module free to
//! restore dev mode to off while another is mid-request under it.
//!
//! WHAT THIS DOES NOT PROTECT AGAINST
//! ----------------------------------
//! The single lock only covers code that TAKES it. Any module added below that
//! changes those settings without `lock_env()` races every one of these, and
//! the failure surfaces as an unrelated connection being refused rather than
//! as a lock error. That is also why `dev_auth`, `node_net`,
//! `node_net_security`, `node_pg_e2e` and `node_tls` are still their own
//! targets: each carries a private `ENV_LOCK` of its own, and putting any of
//! them in here would reintroduce exactly the two-mutexes-one-setting
//! shape this file exists to remove.
//!
//! The settings are process-wide cells in this process, not process
//! ENVIRONMENT: setting them with `std::env::set_var` would race concurrent
//! libc `getenv`, undefined behaviour that no mutex here could cover, since
//! libc reads the environment from code that never takes this lock.
//!
//! These tests need live servers and they do NOT skip without them: the
//! `ensure_*` helpers `docker start` the container and then panic if the port
//! never opens. A red run here can therefore mean "the container is not on
//! this machine" rather than "the runtime regressed" - read the panic message
//! before believing either.
//!
//! The six modules run CONCURRENTLY as six threads in one process.
//! `lock_env()` serialises the environment between them; nothing serialises the
//! `docker start` calls, and nothing stops two of them contending for the same
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
