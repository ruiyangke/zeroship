# plugin-db Security Review — Round 5 (2026-05-22)

Target: `crates/plugin-db/` at `/home/ruiyang/Projects/appbase`,
HEAD `d2e7e22` (post-r4 cycle 03:25). Fresh re-audit; do not assume r4
findings still hold.

Commits since r4 audited:

- `0049d9be` + `91830cca` — [I28] `Result<_, String>` sweep across
  `auth/bootstrap.rs`, `auth/keys.rs`, `auth/session.rs`,
  `replication.rs`, `replication_ops.rs`, `diff.rs`,
  `backend/postgres.rs`, `wal_consumer.rs`. ~30 sites now typed
  `DbError` with SQLSTATE classification preserved through to the V8
  boundary.
- `bd1e7ce1` — [I42] `OrchestratorLockGuard::release()` defers
  `released = true` until AFTER the unlock await completes.
- `5ceb6daa` — [I36] `query.rs::validate_collection` reserved-prefix
  check via byte-eq, no allocation per CRUD dispatch.

---

## 1. Audit dimensions — findings

### 1.1 [I28] auth/* sweep — error-classification audit

**Verdict: sound, with one fragile-substring observation.**

Each helper now returns `Result<_, DbError>` with SQLSTATE
classification routed through a per-module `coded_sql(context, e)`
helper that prepends a `"auth/<layer>: <ctx>: "` prefix to the
human-readable body while preserving the variant. The four prefix-
eligible variants (`UniqueViolation`, `FkViolation`,
`NotNullViolation`, `CheckViolation`, `Serialization`,
`LockContention`, `Transient`, `Internal`) get prefixed; the four
structured variants whose `.code` is part of the SDK contract
(`Configuration`, `ValidationFailed`, `Coded`, `SchemaRefused`) are
left verbatim.

I walked every call site for security-relevant classification choice:

| Site | Classification | Correct for SDK retry semantics? |
| --- | --- | --- |
| `auth/bootstrap.rs:103-117, 128-130, 162-178` (probe + create role / schema / tables) | `coded_sql("…", e)` — preserves SQLSTATE | Yes — connection errors → `Transient`, unique → `UniqueViolation`. |
| `auth/keys.rs:90, 125, 153, 132, 160` (rotate / current / previous key id) | `coded_sql` for SQL, `DbError::internal(format!)` for parse failures | Yes — parse-fail is a programming bug and `internal` is appropriate. |
| `auth/session.rs:117-127` (`pg_backend_pid` parse) | `DbError::internal` for parse fail | Correct — backend_pid is platform-internal; parse failure is non-retriable. |
| `auth/session.rs:152` (`sign_session`) | `coded_sql("sign_session", e)` | Yes — preserves SQLSTATE; `'no active HMAC key'` RAISE flows through as `DbError::Internal` (P0001 has no class mapping). Acceptable: rotation cron failure is operationally severe and `internal` is the catch-all `.code` the SDK can branch on. |
| `auth/session.rs:199-236` (`init_session`) | Three substring-promotions to `ValidationFailed` with `session_*` codes; fallback through `coded_sql` | See 1.2 below. |

No classification arrives at the SDK with a code that would make
retry behave incorrectly: `Transient` and `Serialization` get hinted
"retry"-branded, every typed-input refusal goes to `ValidationFailed`
which the SDK treats as non-retriable.

**No finding.** The [I28] auth/* sweep is internally consistent.

---

### 1.2 `init_session` P0001 RAISE substring promotion — fragility

**Verdict: correct today, structurally fragile against future RAISE
strings.**

Code (`auth/session.rs:199-236`):

```rust
.map_err(|e| {
    let mut msg = format!("{e}");
    let mut cur: &dyn std::error::Error = &e;
    while let Some(src) = std::error::Error::source(cur) {
        msg.push_str(" | ");
        msg.push_str(&format!("{src}"));
        cur = src;
    }
    if msg.contains("nonce replay detected") {
        DbError::validation("session_nonce_replay", "auth/session: nonce replay detected")
    } else if msg.contains("signature expired") {
        DbError::validation("session_signature_expired", "auth/session: signature expired")
    } else if msg.contains("invalid session-init signature") {
        DbError::validation("session_invalid_signature", "auth/session: invalid signature")
    } else {
        coded_sql("init_session", e)
    }
})
```

SQL function (`auth/bootstrap.rs:519-587`) emits five P0001 RAISEs:

| RAISE text | Substring matched? | Resulting `.code` |
| --- | --- | --- |
| `'session-init signature expired'` | `"signature expired"` ✓ | `session_signature_expired` |
| `'invalid actor_kind: %', p_actor_kind` | none | `internal` (P0001 unclassified) |
| `'nonce too short (need >=16 bytes)'` | none | `internal` |
| `'session-init nonce replay detected'` | `"nonce replay detected"` ✓ | `session_nonce_replay` |
| `'invalid session-init signature'` | `"invalid session-init signature"` ✓ | `session_invalid_signature` |

Today's promotion logic is **functionally correct**:

- Order of checks matches the SQL's RAISE order — `expires_at < NOW()`
  fires first in SQL, but expired-token messages contain only
  `"signature expired"` so they correctly land at the second arm
  (nonce check ordered first in Rust is fine because the SQL never
  produces a message containing both `"nonce replay detected"` AND
  `"signature expired"` simultaneously: nonce check runs AFTER expiry
  check in SQL, so an expired-token never reaches the nonce insert).
- `actor_kind`/`actor_id` are SessionInit fields and not user-supplied
  at the plugin-db boundary (only `Replication`-mint code constructs
  `SessionInit`, always with literal strings — verified by Grep
  `SessionInit \{|SessionInit\(` on `crates/`). So substring
  injection via `actor_kind = "nonce replay detected"` is not a
  current attack path.

**Fragility observation:**

1. **A future RAISE EXCEPTION USING DETAIL/HINT addition** could
   introduce a new P0001 message that incidentally contains
   `"signature expired"` (e.g. `'key signature expired (re-rotate
   needed)'` for a different bug), which would silently mis-route to
   `session_signature_expired` and the SDK would tell the caller to
   retry with a fresh nonce when the real fix is to wait for the
   cron. The promotion logic is decoupled from the SQL source of
   truth.
2. **The substring check sees the FULL source chain** — i.e. anything
   in `compio_postgres::Error`'s chain (statement text, parameter
   echoes, hint strings). Today's compio-postgres formatter doesn't
   echo bound BYTEA params verbatim, but a future driver change that
   added "context: SELECT __zeroship_admin.init_session(…)" to the
   source chain would expose the substring matcher to the literal
   SQL string, which **does** contain the function name
   `init_session` but not the RAISE keywords. Still, the
   reliance-on-formatter-behaviour is a latent coupling.

**[FINDING — MINOR]** `auth/session.rs:215-229` — P0001 RAISE
classification is by substring match on the rendered error chain,
not by SQLSTATE + a structured discriminator.

  - **Why:** future RAISE additions (or a compio-postgres formatter
    change that includes more of the SQL chain) can silently mis-
    classify a new error as `session_signature_expired` /
    `session_nonce_replay` / `session_invalid_signature`, causing the
    SDK to follow the wrong retry branch.
  - **Fix:** RAISE EXCEPTION ... USING ERRCODE = 'P0001', DETAIL =
    'session_signature_expired' (the SQL emits a stable
    machine-readable token), then read `e.code() == "P0001"` AND
    `e.detail() == "session_signature_expired"` in Rust. The current
    helper functions in compio-postgres already expose `e.code()` (see
    `error.rs:152`); a `e.detail()` accessor is similarly accessible
    via the underlying tokio-postgres `DbError` type. The SQL change
    is a one-line addition per RAISE.
  - **Verification:**
    ```
    $ rg -n "msg.contains" crates/plugin-db/src/auth/session.rs
    crates/plugin-db/src/auth/session.rs:215:            if msg.contains("nonce replay detected") {
    crates/plugin-db/src/auth/session.rs:220:            } else if msg.contains("signature expired") {
    crates/plugin-db/src/auth/session.rs:225:            } else if msg.contains("invalid session-init signature") {
    ```
    Three string-contains gates with no SQLSTATE coupling.

Not exploitable today; severity MINOR because the only inputs that
reach the substring matcher are RAISE messages emitted by code that
we control in the same module.

---

### 1.3 SQL-injection sweep — re-walk post-[I28]

I re-walked every `format!`-built SQL site touched by the [I28] sweep
to confirm classification changes did not introduce new injection
surface.

| File:line | What it interpolates | Why safe |
| --- | --- | --- |
| `auth/bootstrap.rs:128-130, 141-146, 154-156, 207-208` | `ADMIN_SCHEMA`, `PLATFORM_ROLE`, `APP_ROLE_TEMPLATE` | `const` literals from `auth/mod.rs`. No user input. |
| `auth/bootstrap.rs:200-258, 312-405, 519-587, 731-797` | Same constants inside CREATE FUNCTION bodies | Constants only. |
| `auth/keys.rs:86, 117, 144` | `ADMIN_SCHEMA` only | Constant. |
| `auth/session.rs:139, 184` | `ADMIN_SCHEMA` only | Constant. |
| `replication.rs:200` | `pub_name` (`sanitise_app_id`) + `schema_ref` (`quote_ident(app_id)`) | Sanitised + quote_ident. |
| `replication.rs:386-387` | `OBJECT_PREFIX` inside `LIKE '__zs_%'` | `const &str = "__zs_"`. |
| `replication.rs:504-512` | `OBJECT_PREFIX` inside `LIKE`; `$1` text-bound for floor_bytes | Constant + parameterised. |

No new injection finding from the [I28] sweep. The pre-existing
convention-deviation footprint (`backend/postgres.rs:400-404, 461-463`
raw `app_id`/`spec.name`; `audit.rs` 18 raw `"{app_id}"` sites;
`replication.rs:200` raw `"{pub_name}"`) is **unchanged** by these
commits.

---

### 1.4 App-id isolation — new entry points?

**Verdict: no new entry points. CRITICAL fix at `309ed52f` still
holds.**

Re-walked every `#[v8_method]` in `crates/plugin-db/src/v8_classes/`:

- `Db::*` — passes `self.app_id` (mint-time stamp).
- `Db::start_replication_consumer` — routes through
  `resolve_consumer_app_id(stamped, &opts)` which returns `stamped`
  verbatim. Unit tests at `db.rs:430-462` exercise string, object,
  number, array, null override shapes.
- `Replication::setup` — routes through `resolve_setup_app_id`. Unit
  tests at `replication.rs:170-212`.
- `Replication::watchdog` / `Replication::drop_abandoned` — **DO NOT
  scope to `self.app_id`**. See 1.5 below.
- `Collection`, `Transaction`, `Subscription`, `Migrations`,
  `Migration` — all consume `app_id` from a mint-time stamp;
  none reads from a JS arg.

**No regression to [I31] closure.** The two `resolve_*_app_id`
helpers' `_opts` parameters are deliberately discarded with regression
unit tests.

---

### 1.5 `Replication::watchdog` / `Replication::dropAbandoned`
       — cross-app cluster-wide exposure to tenant JS

**Verdict: NEW FINDING — surfaced by re-walking the v8_classes
methods.**

`Replication::watchdog` and `Replication::drop_abandoned` (in
`v8_classes/replication.rs:71-95`) are `#[v8_method]` callable from
tenant JS via `env.db.replication.watchdog()` /
`env.db.replication.dropAbandoned({inactiveSeconds: N})`. Both
dispatch through `replication::watchdog_query(&pool)` /
`replication::drop_abandoned_slots(&pool, inactive_seconds)`, neither
of which scopes by `app_id`:

`replication.rs:374-414` (`watchdog_query`):

```rust
let sql = format!(
    r"SELECT slot_name, active, restart_lsn::text AS restart_lsn,
            confirmed_flush_lsn::text AS confirmed_flush_lsn,
            …
       FROM pg_replication_slots
       WHERE slot_name LIKE '{OBJECT_PREFIX}%'"
);
```

`replication.rs:503-512` (`drop_abandoned_slots`):

```rust
let candidates_sql = format!(
    r"SELECT slot_name
      FROM pg_replication_slots
      WHERE slot_name LIKE '{OBJECT_PREFIX}%'
        AND active = false
        AND ( restart_lsn IS NULL
           OR pg_wal_lsn_diff(pg_current_wal_lsn(), restart_lsn) >= $1 )"
);
```

The `LIKE '__zs_%'` predicate matches `__zs_slot_<every_app>` —
cluster-wide enumeration / reaping.

Concrete attack paths:

1. **Cross-app enumeration of tenants** (info-disclosure). App A
   calls `env.db.replication.watchdog()`; the response includes the
   `slot_name` of every other app on the same Postgres cluster.
   `slot_name` = `__zs_slot_<sanitised_app_id>` — i.e. the sanitised
   `app_id` of every tenant. Maps every co-tenant for the worker's
   Postgres cluster.
2. **Cross-app DoS via slot reaper.** App A calls
   `env.db.replication.dropAbandoned({inactiveSeconds: 0})`. The
   reaper enumerates every `__zs_*` slot with `active = false` AND
   `restart_lsn IS NULL` (newly-provisioned-but-not-yet-streaming
   slots fall in this bucket; `floor_bytes = max(0, 0) = 0`, so the
   `>= $1` predicate is always true), then issues
   `pg_drop_replication_slot($1)` for each. Other apps' subscribers
   then need to resync from the current WAL head — silent state loss
   plus a one-time reconnection storm. Whether the underlying
   `pg_drop_replication_slot()` succeeds depends on the pool's role:
    - If the pool connects with a role that has REPLICATION (typical
      for the platform worker today — `init_pool_async` uses a single
      `DB_URL` and the C1 setup requires REPLICATION-capable
      privileges), the DROP succeeds for any non-active slot. Cross-
      app DoS confirmed.
    - If a future deployment binds per-app NOREPLICATION roles (per
      the B8c proposal), the DROP fails per-row with
      `permission denied`; the watchdog enumeration still works and
      info-disclosure stands. Reaper failure is logged but the JS
      response promises a `string[]` of dropped names — failed rows
      bail out the entire call with `permission_denied`, surfacing
      the privilege check failure to tenant JS (a side-channel
      probe).
3. **WAL retention bloat amplifier.** The reaper *could* be misused
   to drop healthy slots whose subscribers are briefly inactive
   (between WebSocket reconnects, during cold-start). Each drop
   forces a resync — cross-app QoS impact even on currently-paying
   tenants.

The `dropAbandoned` SQL also has a separate ergonomic concern: line
514 passes `floor_bytes.to_string()` as `$1` for the `pg_wal_lsn_diff
>= $1` predicate. Postgres parses text-bound numerics, but for a
sufficiently large signed-int value text-cast could surface
unexpected coercion. Not a security issue (the LHS is BIGINT and the
RHS bounds it), but the convention in the rest of the codebase is
typed-param binding.

**[FINDING — CRITICAL]** `v8_classes/replication.rs:71-95` —
`Replication::watchdog()` and `Replication::dropAbandoned()` expose
cluster-wide enumeration + slot deletion to tenant JS with no
`app_id` scoping. Sibling fix to the [I31] cross-app setup
override closed at `309ed52f`.

  - **Why:** App A can enumerate every other app on the cluster and
    can (depending on pool role) drop every other app's WAL slot,
    forcing cross-tenant resyncs. The `Replication::setup` hijack
    fix at `309ed52f` scoped `setup` to `self.app_id` but left these
    two siblings cluster-wide. The module doc (line 1) describes the
    methods as "the `db.replication.*` operator namespace" — they
    were designed for platform-operator callers, not tenant code.
  - **Fix (preferred):** demote both methods from `#[v8_method]` on
    the tenant-visible `Replication` v8_class. Move them to a
    control-plane HTTP endpoint scoped to operator credentials, the
    same way `__zeroship_admin.drop_abandoned_slots(BIGINT)` is
    already installed as SECURITY DEFINER with EXECUTE granted ONLY
    to `PLATFORM_ROLE` (`auth/bootstrap.rs:919-938`). The Rust
    `replication::drop_abandoned_slots` could then call the
    `__zeroship_admin` wrapper via SQL function, with the function's
    GRANT being the access gate.
  - **Fix (compatibility-preserving alternative):** scope both
    methods to `self.app_id`. For `watchdog`, filter the SQL
    `WHERE slot_name = $1` with the single per-app slot name. For
    `dropAbandoned`, fail unless `slot_name = __zs_slot_<self.app_id>`
    and `inactive_seconds > 0`. This collapses both into "per-app
    health probe" / "per-app self-reaper" — tenant-safe and
    operationally useful.
  - **Verification:**
    ```
    $ rg -n "watchdog_query|drop_abandoned_slots" \
        crates/plugin-db/src/replication.rs \
        crates/plugin-db/src/replication_ops.rs \
        crates/plugin-db/src/v8_classes/replication.rs
    crates/plugin-db/src/replication.rs:374:pub async fn watchdog_query(pool: &Pool)
    crates/plugin-db/src/replication.rs:472:pub async fn drop_abandoned_slots(pool: &Pool, inactive_seconds: i64)
    crates/plugin-db/src/replication_ops.rs:90:pub fn replication_watchdog_dispatch
    crates/plugin-db/src/replication_ops.rs:124:pub fn replication_drop_abandoned_dispatch
    crates/plugin-db/src/v8_classes/replication.rs:71:    fn watchdog<'s>
    crates/plugin-db/src/v8_classes/replication.rs:82:    fn drop_abandoned<'s>
    ```
    Both methods are decorated `#[v8_method]` — i.e. callable from
    any tenant JS context that holds a `Replication` instance, which
    every app does via `env.db.replication`.

This sub-finding was latent in r4 (the cross-app `setup` hijack fix
at `309ed52f` shipped the same week and the audit focus was on the
fixed method) and **was not flagged in r4's section 1.4**. The
`309ed52f` commit message and the module-level doc-comment in
`v8_classes/replication.rs` both acknowledge the cross-app risk
pattern; the sibling methods escaped that audit.

---

### 1.6 Blocking `pg_advisory_lock` at `bootstrap.rs:107` — [I43]

**Verdict: still open. No change since r4.**

`OrchestratorLockGuard::acquire` (`lock_guard.rs:97-110`) calls
`backend.acquire_advisory_lock(...)`, which is the blocking variant
(`SELECT pg_advisory_lock(...)`). The migration pipeline uses
`try_acquire_advisory_lock` with `migration_already_active` fail-fast;
the register-model pipeline does not.

This is unchanged from the r4 finding (section 1.5). [I43] in
backlog. No new mitigations introduced; the RAII refactor at
`cbd12944` left the blocking call intact.

No new finding; status carries over from r4.

---

### 1.7 Replication-setup DoS within an app — repeated calls under
       typed-error rail

**Verdict: bounded by idempotency. No new attack.**

`ensure_publication_and_slot` is idempotent (probe-then-create for
both publication and slot). The [I28] sweep typed the failure rail
(`wal_level_not_logical` → `DbError::Configuration` rather than
flattened `Internal`), which has zero effect on the DoS surface:

- Every call still probes `pg_publication` (one SELECT) + may
  CREATE PUBLICATION (one DDL) + probe `pg_replication_slots` (one
  SELECT) + maybe `pg_create_logical_replication_slot` (one DDL).
- Slot name is deterministic per app, so repeated `setup()` for the
  same app reaches the same slot — no slot proliferation.
- Per-call cost is roughly 2-4 round-trips on a fast Postgres. A
  tight loop from one app can saturate the worker's pool against
  `ensure_pool()` but that's the per-query auto-tx finding from r4
  (1.9 in r4), not new here.

The typed `wal_level_not_logical` returned to JS provides better SDK
ergonomics (the SDK can show "ask your operator to set
wal_level=logical" instead of a generic 500), which is a security
positive (operators see meaningful diagnostics instead of opaque
errors).

**No finding.**

---

### 1.8 `OrchestratorLockGuard::Drop` credential / PII leak — re-audit

**Verdict: no credential leak; same as r4's 1.7.**

`Drop::drop` (`lock_guard.rs:178-206`) emits a `tracing::error!` with
fields `key = self.key, tag = self.tag`. The `key` format is
`zs_reg:<app_id>` and the `tag` is the static `"register_model"`.

- `app_id` is a routable tenant identifier (typed_id
  `app_<base62-uuidv7>`). Appears in URLs, audit rows, manifest
  entries — **not** a secret. Logging it is consistent with the rest
  of the codebase.
- `tag` is a static `"register_model"` literal.

No nonces, no signatures, no row data, no connection-URL fragment.
The [I42] reorder defers the `released = true` flip until AFTER the
unlock await, which means a cancellation mid-await leaves
`released = false` and `client = Some(_)` — Drop's log fires with the
same `key/tag` payload. Still no secrets.

**No finding.** Confirms r4 conclusion.

---

### 1.9 Reserved-prefix length validation interaction post-[I36]

**Verdict: correct ordering, no edge case.**

`validate_collection` (`query.rs:61-99`) executes in order:

1. empty check
2. null-byte check
3. `len() > 63` → reject
4. `bytes.len() >= 3 && bytes[..3].eq_ignore_ascii_case(b"pg_")`
5. `bytes.len() >= 10 && bytes[..10].eq_ignore_ascii_case(b"__zeroship")`
6. allowlist `is_ascii_alphanumeric() || c == '_'`

Edge cases I confirmed do NOT bypass:

- 1-byte / 2-byte names — gated by `bytes.len() >= 3` guard at step
  4 (the empty-string filter at step 1 + the charset gate at step 6
  still bound the input space).
- `len() > 63` — gated at step 3 BEFORE the prefix indexing; the
  step-4/5 length guards `>= 3` / `>= 10` are then defensive.
- Unicode multi-byte first bytes (`[0xC2..=0xF4]`) — never byte-equal
  `b"pg_"` or `b"__zeroship"` under `eq_ignore_ascii_case` (which
  case-folds only ASCII letters). Step 6 rejects them at the charset
  gate.
- Uppercase variants (`"PG_FOO"`, `"__ZEROSHIP_…"`) — match via
  `eq_ignore_ascii_case` and are correctly rejected.

The byte-eq optimization is strictly equivalent to the prior
`to_ascii_lowercase().starts_with` pattern for valid inputs and is
**stricter** for invalid ones (the prior code allocated a String for
the `pg_…` rejection branch; the new code never allocates).

**No finding.** [I36] confirmed sound for the second consecutive
round.

---

### 1.10 Replication slot info disclosure via `slot_status`

**Verdict: appears scoped, but boundary depends on caller.**

`replication::slot_status` (`replication.rs:566-584`) is scoped by
`slot_name(app_id)` — the per-app slot name. It is exposed to the
v8 boundary only through… let me check.

```
$ rg -n "slot_status" crates/plugin-db/src
crates/plugin-db/src/replication.rs:566:pub async fn slot_status(
```

No callsite in `v8_classes/` or `replication_ops.rs`. The function
is `pub` but **not currently wired to a v8_method**. Defense-in-
depth observation: marking it `pub` (rather than `pub(crate)`)
leaves a future caller free to expose it without an app-id audit.

**[FINDING — MINOR]** `replication.rs:566` —
`replication::slot_status` is `pub` but uncalled from the V8 bridge.
A future contributor wiring it to a v8_method without scoping by
mint-time `self.app_id` would reintroduce the cross-app hijack the
`resolve_*_app_id` helpers were built to prevent.

  - **Why:** the function takes `app_id: &str` as a free parameter
    and emits SQL keyed on `slot_name(app_id)`. A v8_method that
    derived `app_id` from JS opts would be a cross-app probe.
  - **Fix:** demote to `pub(crate)` and document the convention in
    the doc-comment ("v8_classes wiring must always pass
    `self.app_id`"). Equivalent to the pattern the 5ceb6daa commit
    applied to `wal_consumer.rs` (`#[doc(hidden)] pub fn` →
    `pub(crate)`).
  - **Verification:**
    ```
    $ rg -n "slot_status" crates/plugin-db
    crates/plugin-db/src/replication.rs:566:pub async fn slot_status(
    ```
    Only the definition; no consumer.

---

### 1.11 Replication `dropAbandoned` per-call cost amplifier

**Verdict: tied to 1.5 finding; documented for completeness.**

`drop_abandoned_slots` (`replication.rs:472-557`) issues:

1. One SELECT enumerating all `__zs_*` candidates.
2. For each candidate row: one `pg_drop_replication_slot($1)` call
   sequentially.

If 1.5 is closed by per-app scoping, the candidate count is at most
1 (the calling app's own slot). If 1.5 is closed by demotion to
operator-only, the function continues to enumerate cluster-wide but
only the operator can call it.

No additional finding beyond 1.5.

---

## 2. New / regression check from r4→r5 commits

| Commit | Touches | Security delta |
| --- | --- | --- |
| `0049d9be` | `auth/bootstrap.rs`, `auth/keys.rs`, `auth/session.rs`, `backend/postgres.rs`, `diff.rs`, `replication.rs`, `replication_ops.rs`, `wal_consumer.rs`, `tests/integration.rs` | Typed-error sweep: SDK gains `.code` discrimination on auth + replication failures. `wal_level_not_logical` becomes a typed Configuration variant instead of a flattened Internal. `session_signature_expired` / `session_nonce_replay` / `session_invalid_signature` typed at the dispatch boundary so the SDK can branch without substring matching the JS error message. **Net security positive**, modulo the 1.2 fragility note (the substring branch was *moved* from SDK-side to Rust-side, not eliminated — a structured SQLSTATE-DETAIL discriminator would close it). 10 new unit tests pin the contract; 3 e2e integration tests assert typed variant + canonical code. No regression. |
| `91830cca` | `replication.rs` | Trailing `.into_string()` cleanup left from `0049d9be`. Pure compile-only fix; no surface change. |
| `bd1e7ce1` | `orchestrator/lock_guard.rs` | [I42]: defer `released = true` flip until AFTER unlock await. Improves Drop-on-cancel observability (a missed log under cancellation is now caught). No new surface; defense-in-depth on the catastrophic-path leak detection. Log payload unchanged → same conclusion as 1.8: no credential leak. |

No regressions introduced. The [I28] sweep's `coded_sql` helpers
(one per file: `auth/bootstrap`, `auth/keys`, `auth/session`,
`replication`, `diff`) all match the same shape: prefix the four
prefix-eligible variants, leave the structured `Configuration` /
`ValidationFailed` / `Coded` / `SchemaRefused` alone. This is the
same invariant the `audit::coded_sql` and `backend::postgres::coded_db`
helpers maintain. Consistent across the codebase.

---

## 3. Summary table

| Finding | Severity | Status |
| --- | --- | --- |
| CRITICAL — `Replication::watchdog()` / `Replication::dropAbandoned()` exposed to tenant JS with cluster-wide scope | **NEW r5** | Open. Sibling fix to `309ed52f`. |
| MINOR — `init_session` P0001 RAISE classification by substring match on the rendered error chain | **NEW r5** | Open. Functional today; fragile against future RAISE strings. |
| MINOR — `replication::slot_status` is `pub` without v8 caller; risks cross-app reintroduction if wired naively | **NEW r5** | Open. Demote to `pub(crate)`. |
| IMPORTANT — `bootstrap.rs:107` blocking `pg_advisory_lock` ([I43]) | r3/r4 | Open. Unchanged. |
| IMPORTANT — `backend/postgres.rs:400-404, 461-463` raw `app_id`/`spec.name` interpolation | r3/r4 | Unchanged. |
| IMPORTANT — `audit.rs` 18 raw `"{app_id}"` sites + duplicate `validate_app_id` (no length cap) | r3/r4 | Unchanged. |
| IMPORTANT — `replication.rs:200` raw `"{pub_name}"` builder | r3/r4 | Unchanged. |
| MINOR — `validate_schema` / `audit::validate_app_id` accept leading digits + no 63-byte length cap | r3/r4 | Unchanged. |
| MINOR — `diff::count_violating_not_null` `pub`, unvalidated | r3/r4 | Unchanged. |
| MINOR — `auto_tx.rs:exec_auto_begin` per-query `compio_postgres::connect` with no per-app cap | r3/r4 | Open. |
| MINOR — `init_session` `search_path` includes `public` | r3/r4 | Unchanged. |
| MINOR — WAL consumer cross-tenant visibility Rust-enforced (deferred to P8c) | r3/r4 | Unchanged. |
| **Verified at r5: r4's 1.4 closure (`309ed52f`) still holds for `setup` + `startReplicationConsumer`** | r3/r4 verification | Intact. |
| **Verified at r5: [I36] byte-eq prefix check still sound** | r3/r4 verification | Intact (1.9). |

---

## 4. Score

**74 / 100** (down from 81 in r4).

Justification:

- **+3** for the [I28] sweep across auth/* and replication.rs — the
  SDK now sees structured `.code`s on every auth/replication failure
  rather than a flattened `internal`. `wal_level_not_logical`,
  `session_signature_expired`, `session_nonce_replay`,
  `session_invalid_signature`, `invalid_app_id` are all SDK-branchable
  constants now. This is a meaningful defense-in-depth and SDK
  ergonomics improvement.
- **+1** for [I42] — better Drop observability under cancellation.
- **-10** for the NEW CRITICAL `Replication::watchdog` /
  `Replication::dropAbandoned` cross-app exposure (section 1.5). The
  `309ed52f` audit closed the same vector for `setup` but missed the
  two siblings on the same v8_class. Cluster-wide enumeration is
  always-on info disclosure; cluster-wide reaper is conditional DoS
  (depends on pool role's REPLICATION privilege).
- **-1** for the substring-promotion fragility in `init_session`
  (1.2). Functional today but the wire contract should be SQLSTATE
  + DETAIL, not a rendered-string substring check.

The r5 score reflects the discovery of a CRITICAL that was latent in
the prior audits, not a regression introduced by the r4→r5 commits.
The r4→r5 commits themselves are all clean: positive on observability
and SDK ergonomics, zero new surface. **Without the 1.5 finding the
score would be 83 / 100** (the +4 net positive from [I28] + [I42]
applied to r4's 81 baseline, with a small -2 deduction for 1.2 + 1.10
sub-findings).

What would push to 90+:

1. Close 1.5 (`watchdog` + `dropAbandoned` cross-app exposure) by
   either demoting to operator-only or scoping to `self.app_id`.
   This is the highest-priority delta.
2. Convert 1.2's substring matcher to SQLSTATE + DETAIL — SQL emits
   `RAISE EXCEPTION 'session-init signature expired' USING ERRCODE
   = 'P0001', DETAIL = 'session_signature_expired'`; Rust reads
   `e.code() == "P0001" && e.detail() == "session_signature_expired"`.
3. Demote 1.10's `replication::slot_status` to `pub(crate)`.
4. Resolve [I43] (blocking advisory lock).

What would push past 90:

5. The four remaining convention-deviation injection sites (1.3)
   replaced with `quote_ident`.
6. Per-app semaphore in `crate::context` capping in-flight auto-tx
   connection acquires.
7. Per-app Postgres role ownership of the WAL slot (P8c —
   proposal-tracked).
