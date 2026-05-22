# plugin-db Security Review — Round 10 (2026-05-22)

Target: `crates/plugin-db/` at `/home/ruiyang/Projects/appbase`.
HEAD: `2d34061e` (docs/reviews cycle 11:17 — reviewer reports + deferred
update). Prior round: r9 (cycle 10:47) — 83/100.

Five commits land in plugin-db since r9's HEAD `a6dca645`:

| Commit | Subject | Security delta |
| --- | --- | --- |
| `2fa9472e` | gate dormant `auth/*` subtree behind `hardening` feature | committed (was uncommitted in r9 §1.4) — no new delta |
| `5d9acab8` | surface `mig_lock` state drift via `tracing` (I23) | neutral — observability only, no state-machine race introduced |
| `403b3891` | `validate_field_name` rejects non-ASCII (I12) | **positive — closes r9-carried MINOR (unicode aliasing)** |
| `4cab871a` | 3 doc/visibility cleanups | positive — `replication::slot_status` demoted `pub` → `pub(crate)` |
| `ae5570dc` | unit tests for `queue_or_emit` / drain / clear (I13) | neutral — tests only |
| `2d34061e` | reviewer reports + deferred update | docs only |

This round is a focused re-audit against HEAD for the six prompt
lenses.

---

## 1. Findings, by prompt lens

### 1.1 `validate_field_name` non-ASCII concern (r9 MINOR) — CLOSED

**Verdict: closed by `403b3891`. The r9-carried unicode-aliasing
concern is gone; the contract now matches `validate_collection`.**

```
[CLOSED] crates/plugin-db/src/query.rs:111-136 — validate_field_name
  Why:  Post-`403b3891`, the validator runs the same ASCII-allowlist
        loop `validate_collection` has used since r4:

          if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
              return Err(QueryError::InvalidIdent(format!(
                  "invalid field name: {name} (allowed: ASCII alphanumeric + underscore)"
              )));
          }

        Two new unit tests pin the contract (query.rs:4281-4309):
          - `validate_field_name_rejects_non_ascii` covers café /
            naïve / 日本 / em-dash / space.
          - `validate_field_name_accepts_ascii_allowlist` pins
            id / user_id / createdAt / v2 / _private.

  Net: +1 on score. The MINOR is closed.

  Verification:
    Read crates/plugin-db/src/query.rs:127-134 — ASCII-allowlist
      branch present.
    `git show 403b3891 --stat` — query.rs:42+/1-, 2 tests added.
    `cargo test -p zeroship-plugin-db --lib` (per commit body): 349
      pass (was 347).
```

### 1.2 [I25] `OBJECT_PREFIX` LIKE — retro-closure held

**Verdict: held. The cycle 11:17 retro-closure of [I25] is sound at
HEAD — `replication.rs` has zero `LIKE '<lit>...'` predicates in
code.**

```
[CLEAN] crates/plugin-db/src/replication.rs — LIKE predicates
  Why:  `grep -n "LIKE '"` returns only docstring references:
          replication.rs:25 — module-level doc: "LIKE '__zs_%' predicate"
          replication.rs:57 — fn-level doc: "LIKE '__zs_%' stays selective"

        All four production LIKE sites bind via `$1`:
          replication.rs:413 — watchdog query
          replication.rs:547 — drop_abandoned_slots candidate enumeration
          replication.rs:385 (doc) + replication.rs:476 (doc) describe
            the same binding pattern at the two real sites.

        Cross-check at HEAD: every callsite invokes
          `pool.query_text_params(sql, &[&slot_prefix])`
        with `slot_prefix` derived from `slot_name_like_prefix(app_id)`
        which routes through `sanitise_app_id` first (replication.rs:
        82-101 — empty-reject, `[A-Za-z0-9_]`-only, lowercases on
        success).

        The two `LIKE '__zs_%'` literals in
        `auth/bootstrap.rs:891,953` are inside SECURITY DEFINER CREATE
        FUNCTION bodies (server-side SQL, not Rust interpolation) and
        only compile under `--features hardening` — invisible in
        default builds.

  Fix: none.

  Verification:
    Grep `\sLIKE\s` in crates/plugin-db/src/replication.rs — 7
      matches, all either docstring or `LIKE $1`. No format-string
      interpolation of any const into LIKE.
    Grep `LIKE '` in crates/plugin-db/src/replication.rs — 2 matches,
      both docstring (lines 25 + 57).
```

### 1.3 r9 §1.8a hostname-leak (LOW, carried) — UNCHANGED

```
[LOW, CARRY] crates/plugin-db/src/lib.rs:357-374 — init_pool_async
  source-chain walk
  Why:  Re-read at HEAD. `init_pool_async()` (lib.rs:351-374) still:

          .map_err(|e| {
              let mut msg = format!("db: failed to connect: {e}");
              let mut cur: &dyn std::error::Error = &e;
              while let Some(src) = std::error::Error::source(cur) {
                  msg.push_str(&format!(" — caused by: {src}"));
                  cur = src;
              }
              msg
          })?;

        Identical to r9 §1.8a. The terminal `io::Error` from
        `getaddrinfo` historically embeds the queried hostname, which
        therefore reaches the JS console via `lazy_init_failed`.

  Impact: LOW — unchanged. Same fix path A/B as r8/r9: redact in
    `init_pool_async`, or `tracing::error!` operator-side and
    surface only the top-level kind to the SDK.

  Verification:
    Read crates/plugin-db/src/lib.rs:357-374 — bytewise identical to
      r9's quoted block.
    `git diff a6dca645..HEAD -- crates/plugin-db/src/lib.rs` — only
      the auth-mod cfg gates moved; no change to init_pool_async.
```

### 1.4 r9 §1.8b `is_fatal` substring-match (MEDIUM, carried) — UNCHANGED

```
[MEDIUM, CARRY] crates/plugin-db/src/wal_consumer.rs:708-725 — is_fatal()
  Why:  Re-read at HEAD. The function still substring-matches on the
        lowercased rendered `ConsumerError` string:

          let lc = s.to_ascii_lowercase();
          lc.contains("58p01")
              || lc.contains("does not exist")
                  && (lc.contains("replication slot") || lc.contains("publication"))
              || lc.contains("invalid slot name")

        SQLSTATE 58P01 is locale-stable; the other four substrings
        are English-message-fragile (locale-sensitive PG builds will
        under-classify).

        Producer sites (wal_consumer.rs:399, 403, 412, 423, 446) still
        throw away typed `compio_postgres::Error` via `e.to_string()`
        into `ConsumerError::{Connect,Io,Decode}(String)`. No
        SQLSTATE field added to the variants since r9.

  Impact: MEDIUM — unchanged. Within-app DoS-adjacent on i18n PG
    builds; multi-tenant unaffected.

  Fix: same two options as r8/r9 — (a) `Option<SqlState>` on each
    `ConsumerError` variant or (b) wrap `compio_postgres::Error`
    directly.

  Verification:
    Read crates/plugin-db/src/wal_consumer.rs:708-725 — bytewise
      identical to r9's quoted block.
    `git diff a6dca645..HEAD -- crates/plugin-db/src/wal_consumer.rs`
      — empty (file not touched this cycle).
```

### 1.5 `hardening` feature gate — committed; auth/* hidden in default builds

**Verdict: HELD. The r9 "uncommitted" gate is now committed at
`2fa9472e`; the default-build attack surface continues to exclude
the entire `auth/*` subtree.**

```
[POSITIVE, HELD] crates/plugin-db/Cargo.toml:48 + lib.rs:68-71
  Why:  At HEAD:
          Cargo.toml:48 — `hardening = []` feature declared.
          lib.rs:68-71 — `mod auth` gated:
            #[cfg(all(feature = "hardening", not(feature = "test-helpers")))]
            pub(crate) mod auth;
            #[cfg(all(feature = "hardening", feature = "test-helpers"))]
            pub mod auth;

        Default `cargo build -p zeroship-plugin-db --lib` (without
        `--features hardening`) excludes the entire `auth/*` subtree:
        `bootstrap.rs` (HMAC keys + SECURITY DEFINER CREATE FUNCTION
        bodies), `session.rs` (`mint_session_token`, `init_session`,
        `classify_p0001_detail`), `keys.rs` (rotation), `mod.rs`.

        Cross-checked NO production callers of `crate::auth::` exist
        in the crate's source tree at HEAD:
          $ grep -rn "crate::auth\|self::auth\|super::auth" \
              crates/plugin-db/src/
            crates/plugin-db/src/auth/session.rs:166 — docstring only
            crates/plugin-db/src/auth/session.rs:191 — docstring only

        Integration tests still cover both shapes — the [[test]]
        target lists `required-features = ["test-helpers",
        "hardening"]` (Cargo.toml:55).

        Implication for r10's six lenses:
          - The five P0001 DETAIL discriminator tokens
            (`classify_p0001_detail` in auth/session.rs) only exist
            in `--features hardening` builds.
          - HMAC keys (auth/keys.rs) only exist in `--features
            hardening` builds.
          - Session-token mint (auth/session.rs) only exists in
            `--features hardening` builds.
          - The two `LIKE '__zs_%'` literals in
            `auth/bootstrap.rs:891,953` (server-side SQL inside
            SECURITY DEFINER bodies, not Rust interpolation) only
            compile under `--features hardening`.

  Net: +0 on score (already priced into r9's 83 as an
    uncommitted-but-pending change). Held at HEAD; no regression.

  Verification:
    Read crates/plugin-db/Cargo.toml:36-55 — feature decl + test
      required-features.
    Read crates/plugin-db/src/lib.rs:68-71 — cfg gates intact.
    Grep `feature\s*=\s*"hardening"` in crates/plugin-db/ — 2
      matches, both in lib.rs:68 and lib.rs:70.
```

### 1.6 New surface from cycle 11:17 — none requires re-audit

**Verdict: clean. The 5 commits added zero new SQL strings, zero new
V8-boundary entry points, and zero new public APIs accepting tenant
input.**

```
[CLEAN] git diff a6dca645..2d34061e -- crates/plugin-db/
  Stat:
    Cargo.toml          11+
    context.rs          30+ (set_mig_lock tracing::error! branch +
                             return_mig_client warn branch)
    error.rs             6+ (docstring update)
    exec.rs            133+ (3 unit tests for queue_or_emit / drain /
                             clear; no production code)
    lib.rs               4+ (cfg gate refactor on auth/*)
    query.rs            42+ (ASCII allowlist + 2 tests on
                             validate_field_name)
    replication.rs      12+ (slot_status: pub → pub(crate); docstring)

  V8-boundary delta: zero
    `git diff a6dca645..HEAD --stat -- crates/plugin-db/src/v8_classes/`
      → empty.
    `git diff a6dca645..HEAD -- crates/plugin-db/src/v8_bridge.rs` →
      empty.

  New SQL strings: zero
    `git diff a6dca645..HEAD -- 'crates/plugin-db/src/*.rs'` filtered
    for `(SELECT|INSERT|UPDATE|DELETE|CREATE|DROP|ALTER|TRUNCATE)
    .*(FROM|VALUES|TABLE|SCHEMA|INDEX|CONSTRAINT)` → empty.

  Surface-narrowing changes (positive):
    replication.rs:608 — `slot_status` demoted `pub` → `pub(crate)`
    (no production caller; docstring claimed an aspirational V8
    `replicationStatus` callback that doesn't exist).
    lib.rs:68-71 — `mod auth` moved under `--features hardening`.

  Surface-widening changes: none.

  Fix: none.
```

### 1.7 `mig_lock` state machine — no race introduced by `5d9acab8`

```
[CLEAN] crates/plugin-db/src/context.rs:355-413 — mig_lock helpers
  Why:  `5d9acab8` adds:
          - `tracing::error!` in `set_mig_lock` if a prior lock would
            be shadowed (context.rs:374-382).
          - `tracing::warn!` in `return_mig_client` if the slot is
            empty when restoring (context.rs:406-413).

        Underlying state remains a single `Option<MigrationLock>`
        field on `IsolateDbContext` (context.rs:143). The context is
        per-isolate / per-thread (V8 is single-threaded per isolate;
        the slot lives behind `thread_local!`-style access via
        `context::with` / `context::with_mut`). There is no
        cross-thread access path, hence no race condition the tracing
        observes — the changes are purely observability layered onto
        a single-threaded slot.

        The contract `take_mig_client() → await → return_mig_client()`
        is unchanged; the worker remains recoverable on the
        shadow-replace branch by emitting a tracing event and
        proceeding with `replace` (rather than panic). The slot's
        existing unit tests deliberately exercise the swap-on-replace
        shape; replacing with `debug_assert!` was explicitly
        considered and rejected in the commit body.

  Net: 0 on score — neutral observability tightening.

  Verification:
    Read crates/plugin-db/src/context.rs:355-413 — both helpers at HEAD.
    Read crates/plugin-db/src/context.rs:143 — `mig_lock:
      Option<MigrationLock>` declared directly on the per-isolate
      struct (no `Arc<Mutex>` wrapper).
    `git show 5d9acab8 --stat` — only context.rs touched (+29/-3).
```

---

## 2. Cross-check: things that did NOT regress

Re-verified against HEAD:

- **r7 §1.1 `classify_p0001_detail` (5 DETAIL tokens)** — auth/* is
  now feature-gated since `2fa9472e`. In default builds, the
  surface doesn't exist. In `--features hardening` builds the
  literal-string contract from r7/r8/r9 still holds.
- **r9 §1.2 app-id isolation** — `resolve_setup_app_id`,
  `resolve_watchdog_app_id`, `resolve_drop_abandoned_app_id`,
  `resolve_consumer_app_id` unchanged at HEAD; underscore-prefixed
  `_opts: &Value` forcing function still in place.
- **r9 §1.3 SQL-injection sweep** — re-run against the 5
  cycle-11:17 commits, zero new SQL. The recent commits are
  SQL-neutral.
- **r9 §1.7 reserved-prefix validation** — byte-prefix
  `eq_ignore_ascii_case` checks unchanged at `query.rs:80, 85`. The
  field-name sibling now also enforces the ASCII allowlist (1.1).
- **r4 `sanitise_app_id` contract** — empty-reject +
  `[A-Za-z0-9_]`-only + lowercase on success, with `invalid_app_id`
  via `ValidationFailed`, all unchanged (`replication.rs:82-101`).

No regressions observed across the eight prior rounds' findings.

---

## 3. Score delta vs r9

| | r9 | r10 |
| --- | --- | --- |
| Findings closed since prior round | none (1.8a/1.8b carry) | **1.1 (r9 §1.1.b non-ASCII MINOR) closed by `403b3891`; [I25] retro-closure held** |
| Findings opened this round | none | none |
| Security-positive structural changes | `hardening` feature (uncommitted) | hardening committed at `2fa9472e`; `slot_status` demoted to `pub(crate)` |
| Re-verified clean | all r8 findings + reserved-prefix + bench harness | all r9 cleanlines + cycle 11:17's 5 commits add no new SQL / no new V8 surface / no new state-machine race |
| Carryover | [I43] blocking `pg_advisory_lock`, 1.8a hostname LOW, 1.8b is_fatal MEDIUM | [I43] unchanged, 1.8a unchanged, 1.8b unchanged |

The r9→r10 delta:

- **+1** for closing the unicode-aliasing concern via `403b3891`
  (validate_field_name ASCII allowlist + 2 unit tests). The r9
  audit listed this as a MINOR carry under the reserved-prefix
  block; r10 cleanly closes it with the symmetry-with-
  `validate_collection` fix.

- **+0** for `2fa9472e` (hardening gate committed): already priced
  into r9's 83 as the pending uncommitted change. Held at HEAD; no
  regression.

- **+0** for `5d9acab8` (mig_lock tracing): observability over a
  single-threaded slot; no new race surface; neutral.

- **+0** for `4cab871a` (3 doc/visibility cleanups): the
  `slot_status` `pub → pub(crate)` shrinks the public API surface
  by one symbol, a positive but security-neutral movement (the
  function does no SQL string-building — it just runs a typed
  parameterised query).

- **+0** for `ae5570dc` (tests only).

- **−0** for the two unchanged carries (1.8a hostname LOW, 1.8b
  is_fatal MEDIUM): already priced into r9's 83.

- **Net: +1.** Score moves to **84 / 100**.

What would push past 88 now (no change from r9's list):

1. Close 1.3 (init_pool_async source-chain redaction). Easiest.
2. Close 1.4 by adding `Option<SqlState>` to `ConsumerError::
   {Connect,Io,Decode}` so `is_fatal` reads SQLSTATE directly.
3. Resolve [I43] (`pg_try_advisory_lock` + typed
   `LockContention { code: "lock_not_available" }`).
4. (Optional pre-emptive hardening) Add an `audit-allow-list` test
   that fails if any new `format!(SQL, …)` callsite appears
   outside of `query.rs` + `migrations.rs` — locks in the
   property §1.6 currently verifies by hand.

What would push past 92:

5. Split `hardening` into `hardening-bootstrap` (admin schema +
   keys) and `hardening-session` (mint/init session) sub-features
   so a future control-plane wire-up can enable the schema piece
   without pulling in the session-mint code.
6. Convert `ConsumerError` to typed-SQLSTATE-carrying variants
   (subsumes 1.4 fix).
7. Per-app PG role ownership of WAL slot (r7 push-past-92 carry).
8. Tighten DNS-failure observability: `tracing::error!` with full
   source chain on operator side, sanitised top-level kind to
   tenant (closes 1.3's residual operator-vs-tenant info
   asymmetry).

---

## Score (1-100)

**84 / 100**

Up +1 from r9 (83). Cycle 11:17 closed the r9-listed unicode-aliasing
concern (`validate_field_name` now ASCII-only, matching
`validate_collection`'s allowlist). The retro-closure of [I25]
(replication.rs LIKE param-binds) held under direct grep
verification — only docstrings reference the old literal-LIKE form.
The previously-uncommitted `hardening` feature is now committed at
`2fa9472e` with the gate intact (auth/* hidden from default builds).
No new SQL strings, V8-boundary entry points, or state-machine races
introduced by the 5 cycle-11:17 commits. The two r9 carries (LOW
hostname leak in `init_pool_async`, MEDIUM `is_fatal` substring-match
in `wal_consumer`) remain open at the same severity and same fix
path; no new findings opened this round.
