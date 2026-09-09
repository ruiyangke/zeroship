//! The CDC relay service.
//!
//! Source dependencies point INWARD, and this crate is the OUTERMOST ring of the
//! data plane: it may name the inner tiers, and nothing may name it. That is not
//! a convention here, it is the design. The relay exists so the process that
//! executes creator code stops holding `REPLICATION`, and a privilege boundary
//! that any other crate can link is not a boundary - it is the appearance of
//! one. `AGENTS.md`'s "Privilege follows the PROCESS, not the function" is the
//! governing invariant, and the binary shape is how this crate satisfies it.
//!
//! # Nothing links this crate
//!
//! There is a lib target, and it exists for exactly one reason: a `platform`
//! binary must publish its configuration to `zeroship-config-contract`, and that
//! tool links [`config`] to read the registry the compiler emitted. A bin-only
//! crate cannot do that (`crates/zeroship-config-contract/src/registry.rs`'s
//! `platform_specs` calls into each server's LIB), so "ships no lib" is not
//! available as an enforcement mechanism. The enforceable property is stated in
//! `Cargo.toml` and checked by `tests/data_crate_closure_gate.sh` arm 3: no
//! SHIPPED binary's normal-dependency closure contains this crate, with the
//! non-shipped configuration checker as the single named exception.
//!
//! # Where the contracts live, and why not all in one place
//!
//! Anything two processes must agree on is a CONTRACT and does not live here.
//! It has two homes, and the split is deliberate:
//!
//! * **`zeroship-cdc-wire`** owns everything the worker and the relay exchange -
//!   the framing, the eleven frame types, the subscribe request and its permit
//!   commitment, and the `DatastoreId` / `ClusterId` / `DatabaseEpoch` /
//!   `GrantGeneration` vocabulary. It is a leaf with no I/O, no V8 and no driver.
//! * **`zeroship-data-core`** owns the data plane's domain tier - `DbError`,
//!   `DbBinding`, the broker, the read set. The operator's rule permits this
//!   crate to depend on it, and today nothing here needs to.
//!
//! Routing the WIRE contract into `zeroship-data-core` would break both crates'
//! stated properties at once. `zeroship-cdc-wire` refuses `zeroship-core` by
//! name because "a crate whose stated property is no I/O cannot have an HTTP
//! client in its normal closure and still mean it", and `zeroship-data-core`
//! reaches `cyper` through `zeroship-core` (measured, 3 occurrences). The other
//! direction is worse: `zeroship-data-core` is the floor of the worker's data
//! plane, so putting the relay's wire types there would put the worker's whole
//! data plane in the relay's closure - the exact coupling this binary exists to
//! prevent.
//!
//! # The error handling here is internal, on purpose
//!
//! `zeroship-data-postgres` owns `pg_error::classify`, the translation from
//! `compio_postgres::Error` into the data plane's `DbError`. This crate does not
//! share it and will not. A binary that nothing links has no shared-vocabulary
//! problem: there is no consumer to agree with, so a classifier here answers
//! only to this process. The alternative that was considered and rejected -
//! extracting the classifier into a lower crate both sides name - would push a
//! vendor translator toward `zeroship-data-core`, which
//! `tests/data_crate_closure_gate.sh` refuses outright.
//!
//! The arithmetic that made the sharing question look forced does not survive
//! measurement either. Of the nine `pg_error::classify` sites in
//! `crates/zeroship-plugin-db/src/replication.rs`, three are inside
//! `ensure_worker_slot` (relay-only), one is inside `watchdog_query` (which has
//! a live V8 caller and stays), and five are in the drop family (called from
//! both sides today). The adapter keeps `classify` regardless, at no cost -
//! `zeroship-data-postgres` is already in the worker's closure.
//!
//! # What this process does today: nothing, and it says so
//!
//! [`main`](../zeroship_data_cdc_server/index.html) resolves configuration,
//! answers `--check-config` honestly, and then refuses to start. That is the
//! same shape `zeroship-workflow-scheduler` has carried since its own tier was
//! declared ahead of its loop, and it is chosen for the same reason: the
//! configuration surface is operator-visible whether or not the process runs,
//! and a `platform` classification is a requirement to register rather than a
//! judgement call.
//!
//! **NO FILE WAS MOVED INTO THIS CRATE, AND THAT IS THE MEASURED ANSWER RATHER
//! THAN A DEFERRAL.** `crates/zeroship-plugin-db/src/wal_consumer.rs` imports
//! `SuppressGuard`, `has_subscribers` and `publish` from
//! `zeroship_data_orm::broker`, and all three target PROCESS-WIDE
//! `LazyLock<Mutex<..>>` statics. Move that file here verbatim and it compiles,
//! every gate stays green, and `publish` reaches a different process's broker
//! with zero subscribers while `SuppressGuard::activate` suppresses nothing in
//! the worker - so the worker keeps emitting locally while this process believes
//! it has taken authority. `docs/proposals/2026-08-28-cdc-service.md` states the
//! verdict for that file as "Split and rewrite; do not move the file", and this
//! crate honours it by containing no decode loop at all rather than a moved one.
//!
//! # What blocks the decode loop
//!
//! Two things, neither of which is code in this crate:
//!
//! 1. **The keying vocabulary has no entity.** The rewritten slot provisioning
//!    is re-keyed from `app_id` to `DatastoreId`, and `DatastoreId` occurs
//!    nowhere outside `crates/zeroship-cdc-wire` - no table, column or
//!    control-plane record mints one.
//! 2. **The deployment is on `PostgreSQL` 16.** `deploy/compose/docker-compose.yml`
//!    pins `postgres:16`; the relay's design assumes the 18.4 move.
//!
//! Until both clear, filling this crate means inventing an entity or moving a
//! file. Neither is in scope, and the second is the silent failure above.

pub mod config;

/// Why this process refuses to start, in the words an operator sees.
///
/// One string, exported so `main` and any future test assert the same text
/// rather than two spellings that can drift apart.
pub const RELAY_UNAVAILABLE: &str = "the zeroship CDC relay is not yet wired: change decoding still runs inside the worker (zeroship-plugin-db's wal_consumer), and this process holds no replication slot. It refuses to start rather than idle while the worker believes authority has moved.";
