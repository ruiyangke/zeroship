//! `zero-migrate` — the COMPOSITION ROOT. The whole product: the engine plus the
//! backends this build ships.
//!
//! There is almost no logic here, and that is the design. This crate exists to hold
//! ONE fact — WHICH VENDORS EXIST — and to hand it to an engine that cannot see them.
//!
//! ```text
//!   zero-migrate            (this crate) -> core + postgres + sqlite + mysql
//!   zeroship-migrate-core       the engine   -> backend, ir, policy ONLY
//!   zeroship-migrate-postgres   \
//!   zeroship-migrate-sqlite      >           -> backend, ir, policy
//!   zeroship-migrate-mysql      /
//!   zeroship-migrate-backend    the contract -> ir, policy
//!   zeroship-migrate-ir         the wire     -> policy
//!   zeroship-migrate-policy     the PDP      -> leaf
//! ```
//!
//! # Why the composition is its own crate
//!
//! *The core should be neutral, this is the hard limit.* Until this split the engine
//! and the composition were one crate, so the rule was policed by six hand-written
//! censuses over core's own source text — every one of them a grep that could go
//! blind, and several of which had. Moving the three `[dependencies]` lines out makes
//! the rule STRUCTURAL: `zeroship-migrate-core` does not depend on any vendor crate, so
//! naming one in its production source is an unresolved-crate error rather than a
//! finding in a test.
//!
//! That is the whole benefit and it is worth stating what it does NOT cover. Cargo
//! sees CRATE NAMES. It cannot see a vendor's GRAMMAR — `format!("EXCLUDE USING gist
//! …")` names no crate — and it cannot see core's `#[cfg(test)]` modules, which reach
//! the vendors through core's DEV-dependencies. The censuses that measure those two
//! things are still here and are still the only thing that measures them.
//!
//! # The public surface did not move
//!
//! `pub use zeroship_migrate_core::*;` re-exports the engine's entire root, so every
//! `zeroship_migrate::…` path a host already wrote resolves unchanged. The two composition
//! accessors below are what USED to be at the engine's root and could not stay there:
//! they read the shipping list, and the engine no longer has one.

pub use zeroship_migrate_core::*;

// `BackendVendor` is what each vendor crate registers and `VendorSet` is what this
// crate hands out; between them they are the whole vocabulary the composition is
// written in. `BackendRegistry` is deliberately NOT imported here — the engine already
// re-exports it at its root, so the glob above supplies it, and a second `use` would
// shadow that public re-export with a private one.
use zeroship_migrate_backend::registry::{BackendVendor, VendorSet};

/// The shipping backends, named ONCE for the whole product.
///
/// A fourth backend is a `[dependencies]` line in THIS crate's manifest plus an entry
/// in `SHIPPING`, with no edit to the contract crate, no edit to the engine, and no
/// edit to any other vendor. That was already true before the split; what the split
/// added is that the engine cannot quietly reach past this list, because it cannot see
/// the crates the list is made of.
///
/// The set is a compile-time constant rather than a global a host fills at startup,
/// and that is deliberate — see `zeroship_migrate_backend::registry` for why a growable
/// registry would trade a compile error for a runtime one.
const POSTGRES_VENDOR: &BackendVendor = &zeroship_migrate_postgres::VENDOR;
const SQLITE_VENDOR: &BackendVendor = &zeroship_migrate_sqlite::VENDOR;
const MYSQL_VENDOR: &BackendVendor = &zeroship_migrate_mysql::VENDOR;

static SHIPPING: [&BackendVendor; 3] = [POSTGRES_VENDOR, SQLITE_VENDOR, MYSQL_VENDOR];

/// The composition, as the value every engine entry point takes.
const VENDORS: VendorSet = VendorSet::new(&SHIPPING);

/// The backends THIS BUILD ships, as the value every engine entry point takes.
///
/// This is the COMPOSITION, handed out rather than reached for. Nothing below the
/// entry points reads the shipping list: an author, a fold, an engine carries the set
/// it was constructed with, and a free function that needs one is given one. That is
/// what let the engine stop naming the vendor crates — the set became an argument this
/// crate supplies, and this crate is the only place that has to know which backends
/// exist.
///
/// [`shipping_backends`] answers the neighbouring question — what the DESCRIPTORS say
/// — by running the leaf crate's builder over this same set.
#[must_use]
pub const fn shipping_vendors() -> VendorSet {
    VENDORS
}

/// The backends THIS BUILD ships, validated into a [`BackendRegistry`].
///
/// The vendors are separate crates (`zeroship-migrate-postgres`, `zeroship-migrate-sqlite`,
/// `zeroship-migrate-mysql`) and this crate names each of them exactly once, in
/// `SHIPPING`. That list is what replaced the hard-coded three-arm identity match;
/// this function is how a host asks what it got, and it answers by running the leaf
/// crate's own [`BackendRegistry::build`] over the shipping descriptors rather than by
/// restating the id rule here.
///
/// It is derived from the vendors actually compiled in, so the contract crate owns no
/// parallel shipping list that can drift from this build's composition.
///
/// # Panics
///
/// Never in a shipped build: the shipping ids are constants and
/// `tests/dialect_matrix/vendor_registry_owns_shipping_descriptors.rs` proves they
/// satisfy the rule. The `expect` is here so a fourth backend added with a bad or
/// colliding id fails loudly at first use rather than being dropped.
#[must_use]
pub fn shipping_backends() -> BackendRegistry {
    VENDORS
        .descriptors()
        .expect("the shipping backend crates must declare well-formed, distinct dialect ids")
}

// THERE IS NO `#[cfg(test)] mod tests` HERE, AND ITS ABSENCE IS A RULE RATHER THAN AN
// OMISSION.
//
// This file names each vendor crate EXACTLY ONCE, in the three `*_VENDOR` consts, and
// `tests/dialect_matrix/backend_modules_name_one_dialect.rs` asserts that count as the
// positive control for its cross-vendor needle. A unit test here that reached
// `zeroship_migrate_postgres::DIALECT` for an assertion would make the count two and turn
// the one place designed to name a vendor into a place that names it for two different
// reasons.
//
// The assertion that would have lived here — the shipping set composes into a
// `BackendRegistry` with three distinct ids — is
// `tests/dialect_matrix/vendor_registry_owns_shipping_descriptors.rs`, which drives
// `shipping_backends()` through the public surface and reaches each vendor's `DIALECT`
// from the vendor crate itself.

/// Compiles the Rust examples in `docs/embedding.md` as doctests.
///
/// `#[cfg(doctest)]` means this item exists only while rustdoc is collecting
/// doctests, so the guide's prose never lands in the published API docs while its
/// code is still compiled against the real crate. That is the whole point: the
/// embedding guide is the Rust half of the public surface, and until this existed
/// nothing compiled it, so a rename could rot every example in it and leave CI
/// green.
///
/// COVERAGE IS PARTIAL, and worth knowing before trusting a green run: the guide
/// has seven Rust fences and **three** of them are compiled. The rest are
/// ```` ```rust,ignore ```` and are neither compiled nor run, so a rename can still
/// rot them while CI stays green — the exact failure this item was added to
/// prevent, narrowed rather than eliminated.
///
/// The remaining four are not one job. Three need a live backend, an engine and a
/// config to exist before they say anything (`recover_inflight_ddl`,
/// `resolve_pending_contract`, the `PostgresBackend`/`MysqlBackend` pair), so
/// compiling them means standing up fake infrastructure whose drift would then need
/// its own guard. The fourth is a `trait SqlSession` DEFINITION quoted for shape:
/// compiling it would declare a SECOND trait that can silently diverge from the
/// real one while still passing, which is worse than leaving it ignored.
///
/// Where a fragment only lacks a binding, rustdoc's `# ` prefix hides the setup and
/// the fence becomes real coverage — that is how the policy example was closed.
/// Note the cost, since `embedding.md` is also read as plain Markdown in the repo:
/// hidden lines are invisible in rustdoc but VISIBLE there, so each one is
/// boilerplate a human reader pays for. Prefer making genuinely informative setup
/// visible (the policy example shows its charter string) and hiding only `fn main`
/// scaffolding.
///
/// The TypeScript docs are gated the same way from the other side, by the
/// `doc-examples` tests in both JS packages.
#[cfg(doctest)]
#[doc = include_str!("../../../docs/embedding.md")]
pub struct EmbeddingGuideDocTests;
