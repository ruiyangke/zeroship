# plugin-db — API Surface Review (r7)

- **Date:** 2026-05-22
- **Target:** `crates/plugin-db/`
- **Lens:** API surface — visibility scoping, re-exports, error
  variants, test-helper gating, dormant subtrees.
- **Inputs since r6 (78/100):**
  - `f1c5184e` — `DbError::Configuration` gains `hint: Option<String>`
    + `DbError::config_hinted()` constructor.
  - `3d79d2da` — docs-only audit fixes (no surface change).
  - `f6043126` — extracts `classify_detail_token` (`fn`, module-private)
    + SQLSTATE-typed checks.
  - `deeefe18` — `migrations::coded_db` routed through the shared
    `error::prefix_message`.

This audit re-walks the surface from scratch; it does NOT trust
prior round verdicts on items that touch the recent commits.

---

## Dimension 1 — `DbError::Configuration { hint }` backwards-compat

`crates/plugin-db/src/error.rs:55-148`. The variant gained a third
field:

```rust
#[non_exhaustive]
pub enum DbError {
    ...
    Configuration {
        code: &'static str,
        message: String,
        hint: Option<String>,
    },
    ...
}
```

External crates that exhaustively match on `DbError::Configuration {
... }` would break. Three reasons that's safe here:

1. **The enum is `#[non_exhaustive]`** (`error.rs:55`). External
   matches against `Configuration { code, message }` already required
   a `..` rest pattern; adding `hint` extends the in-pattern bindings
   only, not the compile contract.
2. **No external crate matches `Configuration` exhaustively.** Grep
   for `DbError::Configuration\s*\{` across the workspace returns 8
   sites, all in `crates/plugin-db/` itself (the 5 construction sites
   from the commit, plus `error.rs` itself for `to_op_error` /
   `Display` / `into_string`). Tests `error.rs:769` rebuild the
   variant with all three fields after the change.
3. **The runtime materialises the wire format via
   `OpError::coded(code, message, hint)`** — JS surface is unchanged.
   SDK callers that read `err.code` / `err.hint` were already coded
   to expect optional hint.

**Verification:**

```bash
rg "DbError::Configuration\s*\{" crates/
rg "DbError::Configuration" crates/ -l
```

[OK] No surface regression. `#[non_exhaustive]` + crate-internal-only
matching makes the field addition safe.

## Dimension 2 — `config_hinted()` constructor scope

`error.rs:281-302`:

```rust
pub fn config(code: &'static str, message: impl Into<String>) -> Self
pub fn config_hinted(
    code: &'static str,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> Self
```

`config()` is `pub`; the hinted variant is also `pub`. Same shape as
the `validation` / `validation_hinted` pair at `error.rs:305-324` —
the established convention. Both constructors only stamp the
variant; the wire-format invariant (Display + into_string + Configuration
arm of to_op_error) is exercised by tests/`error.rs:766-829`.

**Verification:**

```bash
rg "DbError::config_hinted|config_hinted\(" crates/
# 2 hits — both in error.rs (declaration + rdoc reference).
```

[OK] Constructor visibility matches the partner `config`; no call
sites yet but the surface is correct.

## Dimension 3 — `classify_detail_token` visibility

`crates/plugin-db/src/auth/session.rs:193`:

```rust
fn classify_detail_token(detail: &str) -> Option<(&'static str, &'static str)>
```

Module-private (`fn`, not `pub fn`). It's only called from
`classify_p0001_detail` two lines up + the 9 unit tests in the same
file. Same module, no cross-module need. The docstring at line 184
explicitly justifies the extraction: "extracted ... so the
SDK-contract surface (the 5 session refusal codes) is unit-testable
without standing up a real `compio_postgres::Error` fixture."

**Verification:**

```bash
rg "classify_detail_token" crates/plugin-db/
# 11 hits, all in auth/session.rs.
```

[OK] Correct scope. The pure-function extraction pattern is the
right shape for SDK-contract surfaces (matches the test-coverage r8
guidance the commit message cites).

## Dimension 4 — `DbError` variants — full count + reachability

12 variants, all reachable from production code:

| Variant | Construction sites | JS-visible? |
|---|---|---|
| `SchemaRefused` | `orchestrator::register_model::run_pipeline` (boundary) | yes (envelope) |
| `ValidationFailed` | `validation()` + `validation_hinted()` + `From<QueryError>` | yes |
| `UniqueViolation` / `FkViolation` / `NotNullViolation` / `CheckViolation` | `from_pg` SQLSTATE 23xxx | yes |
| `Serialization` | `from_pg` 40001/40P01 | yes (hint = retry) |
| `LockContention` | `from_pg` 55P03/55006 | yes (hint = backoff) |
| `Transient` | `from_pg` 08xxx/53xx/57xx | yes (hint = backoff) |
| `Configuration` | `config()` + `config_hinted()` + 5 literal sites | yes |
| `Coded` | `migrations.rs` lifecycle helpers | yes |
| `Internal` | `internal()` + `from_pg` unknown SQLSTATE | yes |

Every variant has at least one construction site outside `error.rs`.
The Configuration legs picked the right policy: 2 sites carry
meaningful operator hints (wal_consumer `not_provisioned`,
replication `wal_level_not_logical`), 3 are invariant-class with
`hint: None` (lazy_init_failed, backend_not_initialized,
cic_configuration). That's the correct split — invariant breaches
the SDK can't help recover from.

**Verification:**

```bash
rg "DbError::(Configuration|ValidationFailed|UniqueViolation|FkViolation|NotNullViolation|CheckViolation|Serialization|LockContention|Transient|Coded|Internal|SchemaRefused)" crates/plugin-db/src/ -l
```

[OK] No dead variants.

## Dimension 5 — `#[doc(hidden)] pub fn` sweep

17 `#[doc(hidden)]` sites across the crate:

| File | Count | All gated by `cfg(any(test, feature = "test-helpers"))`? |
|---|---|---|
| `lib.rs` | 7 | yes (lines 209, 237, 261, 298, 313, 322, 330) |
| `migrations.rs` | 6 | yes (lines 780, 794, 806, 835, 847, 859 — all `exec_*_with_pool`) |
| `wal_consumer.rs` | 4 | yes (132, 168, 174, 186) — `any_app_suppressed` + 2 LEGACY_SUPPRESSION_KEY shims; 3 of these are `pub(crate)`, only `any_app_suppressed` is `pub fn` (but `pub(crate)` per :133). Need to re-verify. |
| `replication_ops.rs` | 2 | yes (319, 327 — `is_consumer_registered_for_tests`, `clear_consumer_registry_for_tests`) |
| `exec.rs` | 1 | yes (333 — `exec_mutation_with_emit_for_tests`) |

Cross-check on `wal_consumer.rs`:

- L132 `pub(crate) fn any_app_suppressed` — `#[doc(hidden)]` is
  redundant since the visibility is already crate-private; harmless.
- L168 `const LEGACY_SUPPRESSION_KEY` — `#[doc(hidden)]` on a
  module-private constant; harmless.
- L174, L186 — both `pub(crate) fn`. Same as L132: harmless docstring
  noise on a crate-private item.

No production-visible `pub fn` is hidden behind `doc(hidden)` —
i.e. nothing is "secretly public". All hidden items are either
test-helpers (cfg-gated to `pub`) or already `pub(crate)`.

**Verification:**

```bash
rg "#\[doc\(hidden\)\]" crates/plugin-db/src/
# 17 hits, each followed by either a cfg-gated pub or a pub(crate).
```

[OK] No surface holes via `doc(hidden) pub fn`.

## Dimension 6 — Cfg-fork test-helpers visibility

`lib.rs:48-101` enumerates the modules. The shape is unchanged
since r6:

```rust
// Always pub:
pub mod broker;
pub mod error;
pub mod query;
pub mod v8_classes;

// Always crate-private:
pub(crate) mod backend;
pub(crate) mod context;
pub(crate) mod crud;
pub(crate) mod diff;
pub(crate) mod read_set;
pub(crate) mod v8_bridge;

// Crate-private in release, pub under `test-helpers`:
audit, auth, exec, migrations, orchestrator, replication,
replication_ops, wal_consumer
```

The cfg-fork is necessary because `tests/integration.rs` (target
gated on `required-features = ["test-helpers"]`) does
`zeroship_plugin_db::orchestrator::register_model::exec_register_model_with_pool(...)`,
`zeroship_plugin_db::audit::ensure_audit_table_exists(...)`,
`zeroship_plugin_db::wal_consumer::WalConsumer::new(...)`,
`zeroship_plugin_db::replication::ensure_publication_and_slot(...)`,
`zeroship_plugin_db::auth::ensure_admin_schema(...)`, etc. Each
module is reached.

The "always pub" 4 are reached even without the feature:
`tests/subscription_finalizer.rs` and `tests/db_v8_class.rs` pull
`v8_classes::*`; integration.rs pulls `broker::ChangeEvent`,
`error::DbError`, `query::*`. The cfg-fork covers what's needed and
nothing more.

[OK] Shape is correct. No new modules added in r6→r7 that would
need to be added to the fork.

## Dimension 7 — `auth/*` dormancy

`auth/` exports 8 public items (`auth/mod.rs:64-102`):

- modules: `bootstrap`, `keys`, `session`
- re-exports: `ensure_admin_schema`, `BootstrapOutcome`,
  `rotate_session_keys`, `RotationOutcome`, `init_session`,
  `mint_session_token`, `MintedToken`, `SessionInit`
- constants: `ADMIN_SCHEMA`, `PLATFORM_ROLE`, `APP_ROLE_TEMPLATE`,
  `DEFAULT_TOKEN_TTL_SECS`, `NONCE_RETENTION_SECS`

Consumers across the workspace:

```
crates/plugin-db/tests/integration.rs   31 references (all auth::*)
crates/plugin-db/src/error.rs            docstring mentions only
crates/plugin-db/src/auth/session.rs     internal refs to bootstrap
                                          (CREATE FUNCTION docstring)
```

Zero JS-bridge wiring. No `v8_classes/*` file mentions `auth::*`.
No `v8_bridge.rs` callback dispatches to `mint_session_token` /
`init_session`. `DbPlugin::register()` (lib.rs:177-202) only
installs `install_auto_tx_globals` — nothing from `auth`.

This is the same MAJOR carry from r5 M1 / r6 [I-CARRY-2]. The
module is opt-in per the P8c bootstrap ceremony (the docstring at
`auth/mod.rs:56-62` calls it out: "backwards compatibility: the
hardened path is opt-in"), but the in-tree wiring that would
make it opt-IN-able does not exist — no `--harden` flag, no
control-plane invocation, no JS-facing path. The whole subtree
lives only for the integration tests.

A subtree that the runtime cannot reach is dead surface from the
crate's own perspective. The right move is to either (a) wire it
into the control-plane provisioning path (the proposed home) or
(b) gate the whole `auth` module behind a non-default Cargo feature
(`hardening`) so production builds don't even compile it.

**Verification:**

```bash
rg "crate::auth::|super::auth::|use crate::auth" crates/plugin-db/src/
# 2 hits, both in auth/session.rs docstrings — no production consumer.

rg "auth::(bootstrap|keys|session|mint_session_token|init_session|\
       mint_and_init|rotate_session_keys|ensure_admin_schema)" crates/plugin-db/src/
# Only the docstring back-references — zero call sites.
```

[CARRY] Same severity as r6 (−5).

## Dimension 8 — `pub use` re-exports sweep

```
auth/mod.rs:68-70   3 re-exports — facade for the auth subtree
backend/mod.rs:50   1 re-export — PostgresBackend
```

`auth/mod.rs` re-exports are a flat facade for the 3 submodules.
That's the right shape when the submodules are leaf consumers; if
the goal were to also expose `MintedToken` directly under
`zeroship_plugin_db::auth::MintedToken` (which the tests use,
line 3548) it works as written. No name collisions.

`backend/mod.rs:50` lifts `PostgresBackend` so `crate::backend::Backend`
+ `crate::backend::PostgresBackend` are siblings — common pattern when
the trait + canonical impl are colocated. No external workspace
consumer (the type is `pub(crate)` at the crate level via
`pub(crate) mod backend;`), so the re-export only helps `lib::src/`
intra-crate ergonomics — fine.

[OK] No over-re-exported leaves. No collisions.

---

## Findings

### [MAJOR-R7-1] `auth/*` remains dormant (5 carry)

`crates/plugin-db/src/auth/{mod,bootstrap,keys,session}.rs`

- **Why:** 8 `pub` items and 5 `pub const`s on a module that no
  production caller reaches. Same penalty as r5 M1, r6 [I-CARRY-2].
- **Fix:** Either wire `auth::ensure_admin_schema` into a
  `DbPlugin::register` setup-phase hook gated by a runtime flag, or
  gate the whole `auth` module behind a non-default Cargo feature
  (`hardening = []` in Cargo.toml).
- **Verification:**
  ```bash
  rg "use crate::auth|crate::auth::" crates/plugin-db/src/
  ```

### [MAJOR-R7-2] `replication::OBJECT_PREFIX` is over-public

`crates/plugin-db/src/replication.rs:59`:

```rust
pub const OBJECT_PREFIX: &str = "__zs_";
```

- **Why:** No external workspace consumer. The constant is only
  referenced from `replication.rs` itself (publication_name,
  slot_name, two LIKE predicates). Exposing it `pub` invites a
  future contributor to format-string-interpolate it from outside
  the module (the deferred [I25] security MINOR documents this risk).
- **Fix:** `pub const OBJECT_PREFIX` → `pub(crate) const`. Zero call
  sites outside the file; the visibility downgrade is mechanical.
- **Verification:**
  ```bash
  rg "OBJECT_PREFIX" crates/ --type rust
  # All hits inside replication.rs.
  ```

### [INFO-R7-1] `DbError::config_hinted` constructor has no call sites

`error.rs:292`.

- **Why:** The constructor is `pub` (correct for symmetry with
  `config()`), but the 5 existing `DbError::Configuration { ... }`
  literal constructions did not migrate to the constructor. Two of
  them (wal_consumer `not_provisioned`, replication
  `wal_level_not_logical`) explicitly carry an operator hint and
  would be the natural callers.
- **Fix:** Optional cleanup. Migrate the 2 hinted construction sites
  from struct literal to `DbError::config_hinted(...)`; the 3 hint-less
  sites stay on `DbError::config(...)`. Pure ergonomics; no surface
  change.
- **Verification:**
  ```bash
  rg "DbError::config_hinted\(|DbError::Configuration\s*\{" \
     crates/plugin-db/src/
  ```

### [INFO-R7-2] `wal_consumer::SuppressGuard` activate-only API

`wal_consumer.rs:141-161`:

```rust
pub struct SuppressGuard { app_id: String }
impl SuppressGuard {
    pub fn activate(app_id: &str) -> Self { ... }
}
// no other inherent methods; drop unsuppresses.
```

- **Why:** Single-method `impl` blocks are a code smell on RAII
  guards. `SuppressGuard::activate(x)` reads as `new`. Some readers
  expect `SuppressGuard::new` for the constructor. Not a wire bug —
  surface ergonomics only. Carry from r5/r6 (unchanged).
- **Fix:** Rename `activate` → `new` (or implement `From<&str>`).
  Internal callers (`wal_consumer.rs:464` line) can update in the
  same diff. Tests would need 1 update.

### [INFO-R7-3] `#[doc(hidden)]` on `pub(crate)` items is redundant

`wal_consumer.rs:132, 168, 174, 186` — `#[doc(hidden)]` annotates
items already `pub(crate)`. `doc(hidden)` is only meaningful on
public items (the rustdoc renderer skips non-pub items by default).
The annotation works but lies about the surface ("hidden public") —
the items are NOT public.

- **Fix:** Drop the `#[doc(hidden)]` on the four `pub(crate)`
  items. Keep it on the cfg-gated `pub fn` items in `lib.rs`,
  `migrations.rs`, `replication_ops.rs`, `exec.rs` where it's load-
  bearing (those flip to `pub` under `test-helpers`).
- **Verification:**
  ```bash
  rg -B1 "^pub\(crate\)" crates/plugin-db/src/wal_consumer.rs | \
    grep "doc(hidden)"
  ```

### [INFO-R7-4] r6 MAJOR-R6-1 partially resolved

`context.rs:423-426`:

```rust
#[cfg(any(test, feature = "test-helpers"))]
pub fn mark_consumer_running(&mut self, app_id: &str) { ... }
```

r6 flagged this as dead in production builds. The fix in this cycle
moved it behind a cfg-gate so it no longer compiles in release. Good
— production binary surface no longer carries the footgun.

- **Carry:** the 5 test-only call sites in the same file
  (`mark_consumer_running_is_idempotent`, …) still use the
  unconditional variant. That's fine for `#[test]`-decorated
  functions (also cfg-gated to test builds), but conceptually they
  test the production-unreachable path. Whether to switch them to
  `try_mark_consumer_running(...).unwrap()` is a test-discipline
  call, not an API-surface one.

### [INFO-R7-5] r6 MAJOR-R6-2 resolved

The `replication_ops.rs` module docstring (lines 32-51) was rewritten
to enumerate BOTH error legs from `WalConsumer::new` — `not_provisioned`
(Configuration) AND `invalid_app_id` (ValidationFailed). The doc now
matches the implementation. Recovered.

### [INFO-R7-6] r6 MAJOR-R6-3 resolved

`use crate::error::DbError` is no longer in `replication_ops.rs`
(grep returns zero hits). The dead import was deleted. Recovered.

---

## Carry items unchanged from r6

- [I-CARRY-1] `pub(crate)` items that never escape their own module
  (cosmetic).
- The migrations `_with_pool` test-helper shim layer — sound
  pattern, still adds 6 cfg-gated entry points.

---

## Score

**81 / 100** (**+3 vs r6's 78**)

**Improvements that drove the +3:**

- **`hint: Option<String>` extension on `Configuration` is safe and
  well-paired** (+1): the variant change rides on `#[non_exhaustive]`
  (no external-crate breakage) and pairs with a
  `config_hinted()` constructor that mirrors the existing
  `validation_hinted()` convention. The 2 hinted construction sites
  (wal_consumer, replication) put operator-actionable text in the
  `.hint` slot where the SDK can surface it verbatim.

- **`classify_detail_token` is correctly module-private** (+1):
  the pure-function extraction enables direct unit tests without
  fabricating a `compio_postgres::Error`. Same shape as the
  `classify_p0001_detail` pattern that r5/r6 praised. Tests pin all
  5 codes + the unknown-token fall-through.

- **Two of three r6 MAJORs resolved** (+1): MAJOR-R6-2 (docstring
  drift) and MAJOR-R6-3 (dead `use`) closed; MAJOR-R6-1
  (`mark_consumer_running` dead pub method) is now cfg-gated to test
  builds — production binary surface clean.

**Deductions that prevented a steeper rise:**

- **MAJOR-R7-1 `auth/*` still dormant** (−5, unchanged): same
  8 pub items, 5 pub consts, zero production callers. The
  highest-value next move on this crate's surface, period.

- **MAJOR-R7-2 `OBJECT_PREFIX` over-public** (−2): mechanical
  visibility downgrade `pub` → `pub(crate)` waiting to be made.
  No external consumer, but a footgun if a future contributor
  format-string-interpolates it from outside the file.

- **INFO-R7-2/3 surface debt** (−0, noted): activate-only
  `SuppressGuard` API + 4 `doc(hidden)` annotations on
  `pub(crate)` items. Ergonomic noise, not a defect.

**Score sub-ranges:**

- **86+** would require: gate `auth/*` behind a Cargo feature OR
  wire it into the runtime (MAJOR-R7-1). Single highest-leverage
  move.
- **88+** would require: also downgrade `OBJECT_PREFIX` to
  `pub(crate)` (MAJOR-R7-2; 1-line edit) and migrate the 2 hinted
  Configuration sites to `config_hinted()` (INFO-R7-1; cosmetic
  ergonomic win).
- **93+** would require: also resolve I-CARRY-1 (drop
  `pub(crate)` items with no cross-module consumer) and INFO-R7-2/3
  (rename `SuppressGuard::activate` → `new`; drop redundant
  `doc(hidden)` on `pub(crate)` items).

**Highest-leverage next move:**

```toml
# crates/plugin-db/Cargo.toml
[features]
default = []
test-helpers = []
hardening = []                # <-- new
```

```rust
// crates/plugin-db/src/lib.rs
#[cfg(any(feature = "hardening", test, feature = "test-helpers"))]
pub mod auth;
#[cfg(not(any(feature = "hardening", test, feature = "test-helpers")))]
mod auth_unused {}            // placeholder no-op
```

…plus one line in `tests/integration.rs`'s `required-features` to
add `"hardening"`. Compiles the 4 auth files out of release builds
entirely until the control plane wires them in. Recovers the −5 in
one diff.
