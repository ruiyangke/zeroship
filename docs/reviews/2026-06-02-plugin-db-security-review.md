# plugin-db layer — security review (2026-06-02)

Five-lane adversarial security review of the `env.db` native primitive
(`crates/plugin-db/`, ~90K LOC), the database kernel that every creator app
reaches through untrusted JS. Each lane was an independent opus reviewer; the
top findings below were re-verified against source by the orchestrator
(file:line confirmations inline).

## Threat model

- **App code is untrusted JS**, one V8 isolate per app, calling `env.db.*` with
  arbitrary arguments. AI-generated handlers may be buggy or hostile.
- **Many apps share one Postgres**, isolated by a per-app schema; a connection
  pool and the DB instance are shared across all tenants + the control/auth
  planes.
- The native Rust layer is the **security kernel**: it must enforce tenant
  isolation, prevent injection, protect encrypted/masked fields, and bound
  resource use so one tenant can't harm another.

## The central lens: cross-tenant vs. own-app

Lane 2 (tenant isolation) came back **solid** — `app_id` is stamped server-side
from the request path and is **un-spoofable from app JS**, every query is
**fully schema-qualified** (no `search_path` reliance, so pool checkout can't
leak a tenant), cross-app FKs are **rejected outright**, and privileged DDL has
**no creator dispatch**. That is the load-bearing guarantee, and it holds.

Consequently almost every other finding is **own-app severity** — a creator can
defeat *their own* app's protections (masking, crypto integrity, the platform
boundary) or DoS *themselves*. The dangerous exception is **resource exhaustion
of the shared Postgres**, where one tenant degrades **all** tenants. That makes
**DB-1 (connection exhaustion) the single fleet-wide, launch-blocking issue**;
the rest are serious own-app hardening that protect end-user PII and platform
integrity but do not breach the tenant boundary.

## Findings (by severity)

| ID | Sev | Lane | Title | Scope |
| --- | --- | --- | --- | --- |
| **DB-1** | CRITICAL | DoS | Unbounded transactions open fresh raw PG connections (pool-bypassing), no per-app cap, no statement/idle timeout → fleet-wide connection exhaustion | **cross-tenant** |
| **DB-2** | HIGH | DoS | `find({})` with omitted `limit` emits no `LIMIT` → full-collection pull (worker OOM + shared-DB load) | own-app + DB load |
| **DB-3** | HIGH | Crypto | Unmask authorization reads `actor.kind` from app-supplied JS; `kind:"auto"` is the privileged default → app can unmask its own PII/PHI/PCI at will | own-app (end-user PII) |
| **DB-4** | HIGH | Crypto | Deterministic mode reuses AES-GCM `(key, nonce)` across columns — home-rolled SIV nonce omits AAD, `k_enc` is per-app → GHASH-subkey recovery / forgery | own-app (needs DB read) |
| **DB-5** | HIGH | Capability | `DbPlatform` handle capturable from creator JS at **module top-level** (resolver + `env.db` live before the protective `delete`) → self-serve DDL / `setMaskPolicy({})` off / replication | own-app (P9 §8 break) |
| **DB-6** | MEDIUM | Capability | `v8_value_to_serde_json` has no recursion-depth guard → stack-overflow DoS, **end-user-reachable** via forwarded JSON | own-app (worker abort) |
| **DB-7** | MEDIUM | Crypto | Forgeable `__zsmask__` sentinel — `rehydrate_masked_values` mints a `MaskedValue` from any app-fabricated object → chains with DB-3 to target arbitrary `(row,column)` | own-app |
| **DB-8** | MEDIUM | Injection | Write-path document keys reach `quote_ident` with **no** `validate_field_name` (null-byte / 63-byte-truncation collision / reserved-name bypass; SQL break-out still blocked) | own-app |
| **DB-9** | MEDIUM | Injection | FTS trigger interpolates source column names **unquoted** into DDL; safe only by a non-local "CREATE TABLE ran first" ordering invariant | own-app (latent) |
| **DB-10** | MEDIUM | Crypto | Silent fallback from PG admin-table key source to env-var key on **any** error (not just pre-migration) → wrong-key encrypt/decrypt with no signal | own-app |
| **DB-11** | MEDIUM | DoS | `insertMany` / `updateMany` / `deleteMany` have no batch-size / rows-affected cap → write amplification, multi-MB SQL + param + `RETURNING *` materialization | own-app + DB load |
| **DB-12** | MEDIUM | DoS | No per-app cap on subscriptions or replication slots; abandoned-slot GC is opt-in → WAL/`pg_wal` growth, scarce `max_replication_slots` | own-app → shared-DB |
| **DB-13** | LOW | Crypto | Deterministic AAD omits `row_pk` → in-column ciphertext relocation across rows is undetectable (inherent to searchable encryption; under-mitigated) | own-app |
| **DB-14** | LOW | Crypto | Wire `version_flag` is not covered by AEAD AAD — a future `0x02→0x01` downgrade would force the weaker AAD reconstruction. **Must-fix gate on the `0x02` work.** | own-app (future) |
| **DB-15** | LOW | Injection | `SET ROLE` builds the role name with bare `format!` (no `quote_ident`, no `validate_schema`) — the single statement enforcing per-tenant role separation relies on an external "app_id is clean" assumption | tenant-critical (latent) |
| **DB-16** | LOW | Injection | SQLite `$search` passes the query as raw FTS5 MATCH syntax; PG uses `plainto_tsquery` (literal) → dev/prod semantic divergence + a per-request error/DoS on malformed input | parity |
| **DB-17** | LOW | Tenant | `sanitise_app_id` lowercases for slot/publication names while schema names preserve case → latent cross-tenant CDC collision if any caller ever stamps a case-sensitive typed_id as `APP_ID` (non-exploitable today: prod uses lowercase UUIDs) | tenant (latent) |
| **DB-18** | LOW | DoS/leak | Raw Postgres error text (`Key (email)=(…)`) reaches JS → intra-app value-exfiltration oracle for the app's own masked columns + leaks internal constraint/index names | own-app |

## Verified against source (orchestrator confirmations)

- **DB-1** — `backend/postgres.rs:139` `acquire_dedicated_client` calls
  `compio_postgres::connect(&self.url, …)` directly, **not** the bounded
  `Pool` (`lib.rs:694`). No semaphore in `transaction/mod.rs`; no
  `statement_timeout` / `idle_in_transaction_session_timeout` SET anywhere.
- **DB-2** — `crud/mod.rs:618` `opts.get("limit")…` is `None` when omitted;
  `query.rs:2466` only emits `LIMIT` for `Some`. `MAX_QUERY_LIMIT=500` is a
  ceiling on explicit values, not a default.
- **DB-3** — `masked_value.rs:285` (`opts_v.get("actor")`) and `crud/mod.rs:623`
  both take the actor from app JS; `unmask.rs:283-290` → `kind=="auto"` allowed
  when no policy. Actor is **not** bound to the authenticated identity.
- **DB-4** — `aead.rs:94` the synthetic nonce is `HMAC(k_siv, plaintext)[..12]`;
  `mac.update(plaintext)` only — AAD excluded. `keys.rs` derives `k_enc` per
  app, shared across columns.
- **DB-5** — `runtime-entry.ts:157` deletes the resolver in the bootstrap body;
  its own comment reasons only about `fetch`/`rpc` *handlers* (which run later),
  missing that the **user module top-level** evaluates during `import` *before*
  the delete. `globalThis.__zsDbPlatform(globalThis.__zs_env().db)` at user
  top-level captures the live handle.

## Solid defenses (verified — no action)

- **Tenant isolation** (Lane 2): server-stamped un-spoofable `app_id`; fully
  schema-qualified queries; cross-app FK hard-deny; control-plane-only DDL;
  tenant-keyed CDC/broker.
- **Value injection** (Lane 1): every user value is an out-of-band bind
  parameter (PG extended-protocol Bind / SQLite typed binds) — no value is ever
  concatenated into SQL. Filter operators, aggregation functions, vector
  metrics, and FTS are **closed allowlists**; `$regex` is not implemented;
  FTS uses `plainto_tsquery` (literal). Read/DDL identifiers layer
  `validate_*` + `quote_ident`.
- **Crypto primitives** (Lane 3): AES-256-GCM via RustCrypto (not home-rolled
  primitive), tag verified on every decrypt, randomised mode uses fresh `OsRng`
  nonces, `ZeroizeOnDrop` keys, per-app HKDF key separation, length-prefixed
  AAD segments (no concatenation-collision). The deterministic-mode SIV is the
  one home-rolled weak spot (DB-4).
- **Capability/V8** (Lane 4): handle/app-id confusion closed (Rust-owned
  internal-field state, not JS-mutable); cross-app override hardening unit-
  tested; transaction finalizer exactly-once; savepoint depth capped; no raw-SQL
  surface to app JS.
- **Query-shape limits** (Lane 5): filter nesting depth 16, clause count 128,
  `$in` length 100, explicit `limit`/`offset` ceilings, savepoint depth 8 —
  all server-enforced on every read/write path (raw `default={fetch}` deploys
  hit them too).

## Recommended remediation order

1. **DB-1** (launch blocker, fleet-wide): per-app concurrent-transaction /
   dedicated-connection cap (semaphore at the acquire boundary); `SET
   statement_timeout` + `idle_in_transaction_session_timeout` on every app
   connection (next to the `SET ROLE` in `exec.rs` / `transaction/mod.rs`);
   server-enforced wall bound on the tx body.
2. **DB-2** + **DB-11** (resource quantity): default `LIMIT MAX_QUERY_LIMIT`
   when omitted; hard row/byte cap in `exec_query`; `MAX_*_BATCH` caps on the
   `*Many` paths.
3. **DB-3** + **DB-7** (PII boundary): bind the unmask actor to the per-request
   authenticated identity server-side (reject app-supplied `kind`, especially
   `auto`); mint `MaskedValue` only for cells the read pipeline itself masked,
   never by scanning results for the sentinel.
4. **DB-5** (platform boundary): pass the `DbPlatform` handle into the bootstrap
   module as an evaluation argument/import rather than a `globalThis` property
   that is live during user-module evaluation; or make the resolver refuse once
   any user module has begun evaluating.
5. **DB-4** (crypto): fold the AAD into the synthetic nonce (S2V-style) or derive
   a per-`(app,collection,column)` key; ideally adopt vetted `aes-siv`.
6. **DB-6** (boundary DoS): depth-guard `v8_value_to_serde_json` uniformly over
   arrays + objects, applied at decode so it fences insert/update payloads too.
7. **DB-8** + **DB-9** + **DB-15**: apply `validate_field_name` to write-path
   document keys and FTS source columns; route `SET ROLE` through `quote_ident`
   + `validate_schema`.
8. Lows / parity: DB-10 (don't lump `Err` with `Ok(None)` in key lookup),
   DB-14 (gate the `0x02` work on folding the version flag into AAD), DB-16
   (normalize SQLite FTS to literal), DB-17 (reject non-lowercase `app_id` in
   `sanitise_app_id`), DB-18 (scrub PG `DETAIL` for masked columns).

## Per-lane verdicts

- **Lane 1 — Injection:** no exploitable SQL injection; gaps are defense-in-depth
  (DB-8, DB-9, DB-15, DB-16).
- **Lane 2 — Tenant isolation:** sound; only two LOW latent slot-naming items
  (DB-17).
- **Lane 3 — Encryption & masking:** the authorization model (DB-3) and the
  deterministic-mode crypto (DB-4) are the weak spots; primitives and key
  separation are solid.
- **Lane 4 — Capability/V8:** privilege separation well-architected, but the
  P9 §8 boundary is bypassable at module top-level (DB-5); decoder lacks a
  recursion guard (DB-6).
- **Lane 5 — DoS:** query *shape* is bounded, but resource *quantity* is open —
  DB-1 (fleet-wide) and DB-2 are the must-fixes.
