//! The data plane's domain tier.
//!
//! Source dependencies point INWARD. This crate is the innermost ring that the
//! data plane owns: the backends, the engine and the V8 adapter all name it,
//! and it names none of them.
//!
//! # What lives here, and why only this
//!
//! * [`error`] - `DbError`, the typed error hierarchy. Its variants name a
//!   vendor type ZERO times; the type was never the problem. What was
//!   vendor-bound is the TRANSLATION, and that now lives with the vendor:
//!   `zeroship-data-postgres` translates `compio_postgres::Error` and
//!   `zeroship-data-sqlite` translates `rusqlite::Error`, each into this
//!   `DbError`. The lowering in the other direction - `DbError` to the
//!   runtime's `OpError` - is the ADAPTER's translator and lives in
//!   `zeroship-plugin-db`, because `OpError` is a delivery mechanism and a
//!   domain type may not name one.
//! * [`binding`] - `DbBinding`, the immutable identity of one `env.db` binding
//!   (app id + deploy token). A pure value type with no dependencies at all.
//! * [`budgets`] - the DB-1 execution budgets. Three `u32` constants, moved
//!   here from `zeroship-plugin-db` on 2026-09-02 because BOTH a vendor tier
//!   (`backend/pg_session_sql.rs`, which renders them into PostgreSQL GUCs) and
//!   the engine (`transaction/driver.rs`, which derives the cross-backend
//!   protocol deadline from `DB_IDLE_IN_TX_TIMEOUT_MS`) name them. Two tiers
//!   naming one module is what puts it at rank 0; the module's own header had
//!   already said so.
//!
//! # What deliberately does NOT live here
//!
//! `descriptor.rs` looks like a domain module and is not one. Both of its
//! production functions call `context::with` / `context::with_mut` - the
//! per-isolate connection and schema cache - and `context.rs` belongs to the
//! ENGINE. Placing the descriptor here would make `data-core` depend on
//! `data-engine` while `data-engine` depends on `data-core`: a Cargo cycle,
//! unbuildable. It travels with the engine instead.
//!
//! `to_op_error` does not live here either, for the reason given above: it
//! returned `zeroship_runtime::state::OpError` from the core, which is a domain
//! type reaching outward at a delivery mechanism. It is now the
//! `ToOpError` extension trait in `zeroship-plugin-db`.

pub mod binding;
pub mod budgets;
pub mod capability;
pub mod storage;
pub mod error;
