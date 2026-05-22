# plugin-db docs audit — round 8 (2026-05-22)

**Scope:** `crates/plugin-db/src/` preambles + docstrings + inline WHY
comments. Re-anchored on r7 (87/100).

**Brief commits verified:**

- `251d53b4` plugin-db/v8_bridge: row_to_json O(N²) → O(N) via index lookup (I35)
- `18aee490` plugin-db/migrations: fix finalise_backfill F1 warn-shape drift (NEW-R12-2)
- `bac64c0e` plugin-db/context: demote mig_lock accessors to pub(crate) (NEW-R10-1)
- `f6adb68b` plugin-db/context: privatize IsolateDbContext fields (I16)

**TL;DR.** All four cycle-13:17 / 13:47 commits land their docs cleanly.
Three of them carry exemplary cross-references back to upstream code
(commit `251d53b4`'s comment cites `compio-postgres/src/row.rs` line
range; `18aee490`'s comment cites code-critique r11 MINOR-R11-1;
`f6adb68b`'s docstring lists the specific accessor methods and the
deferred ticket). The r7 NEW IMPORTANT × 2 hold-outs (auth/mod.rs:56-62
"opt-in --harden" framing; lib.rs:34-46 "Module visibility note missing
the cfg-fork") were both closed by intervening commit `71a457a1` —
auth/mod.rs now carries an explicit "Compile-time gating" section, and
lib.rs's note explicitly walks the three-arm hardening ladder. The
r7 NIT carry-overs (auth/{bootstrap,keys,session}.rs preambles missing
the gate cross-ref) remain — three sentences would close them. The
five-round `docs/reference/plugin-system.md` Crate structure drift
also remains, now SIX rounds old.

---

## Dimension 1 — `251d53b4` v8_bridge.rs O(N²) → O(N) comment (I35)

```
[OK] crates/plugin-db/src/v8_bridge.rs:355-359 — comment matches code + commit body
  Why: the WHY paragraph names the forcing function ("`RowIndex for str` linear
  `position` scan in compio_postgres"), the [I35] ticket, the asymptotic class
  (O(N²)), and the fix (threading the index drops per-column lookup to O(1)).
  Cross-verified against compio-postgres/src/row.rs:65-82 (`impl RowIndex for str`
  is `iter().position(|d| d.as_name() == self)` with a case-insensitive retry —
  matches the commit body verbatim). The `column_to_json` docstring at lines
  368-371 carries the matching short version ("Uses a numeric column index ...
  so `Row::try_get` / `Row::raw_value` skip the linear name lookup") and points
  back to `row_to_json` for the full rationale.
  Spot-check: every `try_get::<_, T>(...)` / `raw_value(...)` call in the
  function now takes `idx` (lines 377, 382, 387, 392, 397, 403, 409, 420);
  no `name` survives.
```

## Dimension 2 — `18aee490` migrations.rs finalise_backfill F1 warn comment

```
[OK] crates/plugin-db/src/migrations.rs:647-651 — comment matches body + the other 5 sites
  Why: the WHY comment names the unification PR ("code-critique r11 MINOR-R11-1
  (unified across the 6 F1 warn sites)"), the discriminator-field operators grep
  on (`transition`), and honestly flags the value-shape difference ("quote the
  terminal status string so it lines up with the other 5 sites' string-literal
  discriminators"). The body at lines 652-662 carries `audit_err = %audit_err` +
  `transition = ?terminal` matching the unified shape.
  Cross-check: `rg 'transition\s*=' crates/plugin-db/src` returns exactly 6 hits:
    backend/postgres.rs:499 transition = "Failed/invalid_index"
    backend/postgres.rs:551 transition = "Failed/data_violation"
    backend/postgres.rs:600 transition = "Failed/index_build"
    migrations.rs:657       transition = ?terminal      (this site)
    orchestrator/register_model/apply.rs:181 transition = "Applied"
    orchestrator/register_model/apply.rs:212 transition = "Failed"
  Six sites all use field-name `transition`; the 5 string-literal sites produce
  e.g. `transition="Failed/data_violation"`, while this site produces
  `transition=Failed` (or `transition=Failed { … }` for the variant with a
  payload). The comment self-consistently acknowledges the value-shape drift
  while pinning the field-name contract that operators grep on.
```

## Dimension 3 — `bac64c0e` mig_lock accessor visibility flip (no docstring changes)

```
[OK] No docstring drift introduced. The 5 accessors (has_mig_lock,
  clear_mig_lock, take_mig_client, return_mig_client, mig_lock_snapshot) carry
  the same docstrings they had pre-flip; only the visibility token changed
  (`pub` → `pub(crate)`). `return_mig_client`'s docstring still correctly cites
  the `tracing::error!` pairing (the r7 fix from 4cab871a is intact).
```

## Dimension 4 — `f6adb68b` IsolateDbContext field privatisation (I16)

```
[OK] crates/plugin-db/src/context.rs:65-74 — "all fields are private" assertion is true
  Why: the docstring on `IsolateDbContext` (struct at line 76) explicitly names
  the closure: "Direct field access from inside the crate is rejected at compile
  time. This closes deferred [I16]". Grep verification: zero `ctx.{pool,db_url,
  tx_conn,auto_tx_owned,tx_token_counter,pending_emits,mig_lock,running_consumers,
  backend,registered_models}` direct accesses outside `context.rs` (only matches
  are in context.rs's own #[cfg(test)] tests at lines 575-806, which is fine —
  same module).
  Cross-check the field list: lines 78, 82, 86, 97, 107, 121, 127, 144, 151, 157,
  166 — all 11 fields have no visibility token (private). The 6 `MigrationLock`
  fields at lines 49-62 remain `pub(crate)` (called out in the commit body —
  `set_mig_lock` constructs them from outside the impl). The docstring is scoped
  to `IsolateDbContext`, so "all fields are private" applies only to its 11
  fields, which is true. Self-consistent.

[OK] crates/plugin-db/src/context.rs:122-127 — `tx_token_counter` docstring still accurate
  Why: "Monotonic counter feeding [`Self::tx_token`]. Incremented inside
  [`Self::next_tx_token`]; never reset". With the field now private, the assertion
  "only `next_tx_token` should touch it" is enforced at compile time. The
  IsolateDbContext-level docstring at 65-74 cross-references this exactly.
```

## Dimension 5 — r7 hold-outs since cycle 12:47

```
[CLOSED — was r7 NEW IMPORTANT] crates/plugin-db/src/auth/mod.rs:56-71 — gate framing
  Why: commit `71a457a1` (in the log, between r7 and r8) rewrote the section.
  Lines 56-67 now carry "## Compile-time gating (`hardening` Cargo feature)" with
  the correct `#[cfg(feature = "hardening")]` framing, citation of `2fa9472e`
  (cycle 10:47), and explicit "Default builds do NOT compile this module".
  Lines 69-71 explicitly retract the old `--harden` runtime-flag framing: "The
  original `--harden` CLI flag in the proposal is one possible runtime opt-in
  once this module ships; it is NOT how the subtree is currently gated."

[CLOSED — was r7 NEW IMPORTANT] crates/plugin-db/src/lib.rs:34-56 — Module visibility note
  Why: same commit (`71a457a1`) appended a paragraph at lines 48-56 covering the
  three-arm ladder: "The `auth` module carries an extra cfg dimension (`hardening`
  Cargo feature, commit `2fa9472e`, cycle 10:47): without `--features hardening`
  the subtree is compile-out, regardless of `test-helpers`." Walks all three
  arms (default, default+test-helpers, hardening-on) and cross-references the
  `required-features = ["test-helpers", "hardening"]` on the integration test
  target.
```

## Dimension 6 — net-NEW drift since r7

```
[OK] No new drift introduced by the four audited commits.
  Each commit's docstring/comment is internally consistent and matches the
  upstream code change verbatim. The `251d53b4` v8_bridge comment is exemplary
  (cites compio-postgres line range + asymptotic class + the [I35] ticket); the
  `18aee490` migrations comment is exemplary (cites the unification PR + the
  value-shape difference + the operator-grep contract); the `f6adb68b`
  IsolateDbContext docstring is exemplary (asserts the property, names the
  ticket, lists the accessor methods).
```

## Dimension 7 — Module preambles still present

```
[OK] Every `crates/plugin-db/src/**/*.rs` opens with a `//!` preamble.
  Spot-check: for f in crates/plugin-db/src/*.rs crates/plugin-db/src/{auth,
  backend,orchestrator,v8_classes}/*.rs crates/plugin-db/src/orchestrator/
  register_model/*.rs; do head -1 "$f" | grep -q "^//!" || echo "MISSING: $f"
  ; done — zero hits.
```

## Dimension 8 — Stale references, semantic drift, magic numbers

```
[OK] No stale [I*] references claiming "open" or "TODO" status.
  Verification: rg 'deferred \[I|open\b.*\[I|TODO.*\[I' crates/plugin-db/src
  → 2 hits, both in past-tense closure context (context.rs:71 "This closes
  deferred [I16]"; v8_bridge.rs:357 "[I35]: that made `row_to_json` O(N²)").

[OK] error.rs §2 `Result<_, String>` hold-out enumeration still accurate.
  Verification: `rg -n '-> Result<[^>]*,\s*String>' crates/plugin-db/src`
  returns 6 production signatures (1 each in: auth/session.rs::hex_nibble,
  lib.rs::init_pool_async, v8_classes/migration.rs::parse_commit_spec,
  v8_classes/migration.rs::parse_spec, v8_classes/migrations.rs::
  parse_name_and_collection, orchestrator/register_model/validate.rs). All 6
  map cleanly to error.rs §2's 5 categories (wire-contract, pure parsers,
  JS-input parsers, cold-init, plus the test-helpers carve-out). The hex_decode
  helper called out at error.rs:20 also still exists (auth/session.rs:381) —
  enumeration is honest.

[OK] No "code claims X but does Y" semantic drift detected this round.
  The cycle-13:17 commits closed the largest outstanding case (the
  finalise_backfill F1 warn shape), and the cycle-13:47 privatisation made the
  "tx_token_counter only touched by next_tx_token" assertion compile-enforced.

[OK] No magic-number drift found.
  Spot-checked OID literals at v8_bridge.rs:376-440 (BOOL=16, INT2=21, INT4=23,
  INT8=20, FLOAT4=700, FLOAT8=701, UUID=2950, TIMESTAMP=1114, TIMESTAMPTZ=1184,
  DATE=1082) — match postgres_types::Type constants. The `946684800000` constant
  (2000-01-01 epoch ms) at line 424 carries its derivation comment.
```

## Dimension 9 — Carry-overs as one-liners

```
[NIT — UNCHANGED from r7] auth/bootstrap.rs:1-9 — preamble could cite the hardening cargo gate (one sentence).
[NIT — UNCHANGED from r7] auth/keys.rs:1-9 — preamble could cite the hardening cargo gate.
[NIT — UNCHANGED from r7] auth/session.rs:1-19 — preamble could cite the hardening cargo gate.
[NIT — UNCHANGED from r3/r4/r5/r6/r7] docs/reference/plugin-system.md:315-344 — stale "Crate structure" tree (six paths that don't exist); SIX rounds carry-over.
```

---

## Score: 91 / 100  (r7: 87;  +4)

**Delta breakdown (+4 from r7):**

- **+3** — r7's two NEW IMPORTANTs both closed by `71a457a1`. auth/mod.rs:56-71
  now carries an explicit "Compile-time gating" section with the correct
  `#[cfg(feature = "hardening")]` framing + explicit retraction of the
  `--harden` CLI flag; lib.rs:34-56 walks the three-arm ladder.
- **+2** — three of the four cycle-13:17 / 13:47 commits ship exemplary
  cross-referenced docs: `251d53b4`'s v8_bridge comment names the
  compio-postgres line range + asymptotic class; `18aee490`'s migrations
  comment names the unification PR + the value-shape difference;
  `f6adb68b`'s IsolateDbContext docstring asserts the property + names
  the accessor methods.
- **+0** — `bac64c0e` is pure visibility flip, no docstring changes; no
  delta. (The mig_lock-accessor docstrings carry the r7 fixes intact.)
- **−0.5** — three NIT carry-overs (auth/{bootstrap,keys,session}.rs
  preambles) still don't cross-reference the cargo gate. Reader-experience
  cost is small; new contributors landing on auth/mod.rs see the gating
  section there first.
- **−0.5** — SIX-round-old `docs/reference/plugin-system.md` stale Crate
  structure tree still carries forward (out-of-scope NOTE; AGENTS.md
  routes new contributors here, so the entry-point cost has been
  compounding for six rounds).

**To break 93 next round:**

1. **Land the three NIT preamble cross-refs** for
   `auth/{bootstrap,keys,session}.rs`. One sentence each: "Compiled only
   when `--features hardening` is on; see `auth/mod.rs` for the gating
   rationale." Three-file edit; closes the last of the r7 → r8 carry-overs.
2. **Close the SIX-round `plugin-system.md` Crate structure tree.** Replace
   the six stale paths with the actual layout. Out-of-scope for the
   crate itself but the entry-point experience for new contributors is
   degrading on a multi-round axis.
3. **Move `MigrationLock` field construction inside a
   `MigrationLock::new(...)` ctor** so the 6 `pub(crate)` fields on
   `MigrationLock` can also be privatised. The `f6adb68b` commit body
   explicitly flagged this as the follow-up; doing it would let the
   IsolateDbContext docstring's "all fields are private" assertion
   extend symmetrically to the sibling struct.

If items 1+2 land, the score breaks 93 next round. Item 3 is a +1 ceiling
move toward 94-95.
