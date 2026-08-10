# S1 NetPolicy producer + Phase E (JsDriverBackend) — convergence plan

> Status: **proposal / design-only**. Operator was offline when this was written; nothing here is implemented or committed. This document exists to be approved (or amended) on return, then implemented.
>
> **DECISION (2026-06-27, operator):** S1 grant home = **table-authoritative + manifest-request-layer** (NOT pure-manifest, NOT pure-table). The authoritative grant is the operator-owned `app_net_grants` table the runtime reads (§1.2); the manifest carries an *optional, inert* `net.requests[]` intent that surfaces a requested-vs-granted diff and must be promoted to a table row before it does anything (default-deny in between; OAuth-scope precedent). Wildcards always keep a human gate; exact-host:port requests MAY auto-promote via policy but still flow THROUGH the table (audit + single enforcement source). Creator channel expresses only `Denied`/reviewed `Allowlist`; `Trusted` stays operator/migrate-only. **Tracked as backlog task #215; deferred until the migrate track is finished.**
>
> Sources synthesized: `docs/reviews/2026-06-27-node-net-tls-arch-review.md` + `docs/proposals/2026-06-27-node-net-tls-runtime-design.md` (node-net-tls worktree); `docs/proposals/2026-06-26-mysql-third-backend-design.md` + the landed `zeroship-migrate` seam (appbase-migrate worktree); `AGENTS.md` (trust + control-plane model).

---

## 0. Thesis — one direction, two unblocks

`node:net`/`node:tls` now run live on the zero-tokio compio runtime: real `pg`, `mysql2`, and `ioredis` JS drivers connect over native sockets, gated by a fail-closed `NetPolicy`. That capability is the foundation for a single architectural bet:

> **JS drivers are the transport. Rust owns policy, orchestration, and schema; V8 owns the wire protocol.**

This bet sidesteps writing bespoke compio protocol drivers (no `compio-mysql`, no per-engine auth/TLS handshake code) by letting the mature npm driver ecosystem run inside a `NetPolicy`-gated isolate. Two deferred items must be decided to fully unlock it, and they unlock it from opposite ends:

- **S1 — the NetPolicy control-plane producer.** The runtime is a faithful *consumer* of a policy that **nothing currently produces** for creator apps. Until a producer exists, every creator isolate boots `NetPolicy::Denied` and `node:net` is unresolvable. S1 designs the operator-authored grant seam that lets *creator* apps reach allowlisted hosts (self-hosted DB/SMTP escape hatch). This is the **low-trust, Allowlist** end of the bet.

- **Phase E — the JsDriverBackend.** The migrate engine wants live MySQL (and, as a proof, PG) by running `mysql2`/`pg` over `node:net` inside a `NetPolicy::Trusted` operator isolate, instead of building native drivers. This is the **high-trust, Trusted** end of the same bet.

They share the runtime primitive (`NetPolicy` + `node:net`) but **not** the trust model, **not** the construction site, and **not** the dependency chain. Keeping them distinct is the whole safety story: a creator app can never reach the `Trusted` posture the migrate executor uses, even if the control plane or grant table were compromised.

---

## 1. S1 — the NetPolicy control-plane PRODUCER seam

### 1.1 The gap in one sentence

`NetPolicy` enforcement is fully built and fail-closed in `crates/runtime/src/transport/net_policy.rs`, but the **only** non-test construction site is `worker/src/cache.rs:482` under `#[cfg(test)]`. The real app-load path (`make_isolate`/`load_app`, `cache.rs:288-297`) never calls `.net_policy(...)`, so every creator isolate boots `RuntimeState::net_policy = NetPolicy::Denied` (`core/state.rs:737`) and `node:net` is unresolvable for all creator code. The runtime is a consumer of a policy nothing produces. S1 designs the producer.

The load-bearing constraint that dictates every decision: **the allowlist is operator-authored, never app-self-declared** (`net_policy.rs:9-15`). That single rule is why the grant cannot live in the manifest.

### 1.2 WHERE the grant lives — the trust boundary

**Not the manifest.** The manifest is app-authored (`@zeroship/vite-plugin` emits it; control stores it verbatim in `apps.manifest_json`). A `net_allowlist` field would be creator-controlled wire data — exactly the self-declaration the review forbids; a malicious creator deploys a manifest granting `*.evil.com:443`. The OAuth-scope precedent *confirms* the rule rather than breaking it: scopes are declared in the manifest but **control-plane-validated server-side** before becoming the authoritative `app_scope_defs` record. For outbound TCP the review demands the grant be **reviewed**, not merely format-checked — so the authored grant is a **separate, operator-owned record**, never the manifest echo.

**The home: a dedicated control-plane grant table**, keyed by app, decoupled from the deploy:

```
zeroship.app_net_grants
  app_id     uuid  references zeroship.apps(id) on delete cascade
  host       text  -- exact "smtp.sendgrid.net" or reviewed wildcard "*.db.example.com"
  port       int   -- 1..=65535
  granted_by text  -- operator identity (audit)
  granted_at timestamptz
  note       text  -- "SendGrid SMTP relay, ticket ZS-1234"
  primary key (app_id, host, port)
```

- **Not on the deploy path.** A `zeroship deploy` cannot write here; only the operator/admin control-plane surface can. Keyed on `app_id` (not `deploy_hash`), the grant survives redeploys — it is a standing capability, not a per-build artifact.
- **Validated at authoring time, server-side**, running the same checks the runtime's `ReviewedAllowlist::operator_reviewed` / `validate_reviewed` enforce (no bare `*`, no non-`*.` wildcards, no wildcard IPs, no frontable suffixes). The producer path uses `try_new`/`operator_reviewed` (return `Result`) so a bad entry surfaces as a 4xx in the operator UI — **never** reaches `HostPort::new`'s panic (`net_policy.rs:135`; review S9).
- **Request vs grant are different records, different tables, different writers** — that separation *is* the trust boundary.

### 1.3 HOW it flows — the wire seam (worker channel, not gateway)

`node:net` runs in the **worker**, so the grant rides the **worker-facing** `AppVersionInfo` (polled at `/internal/versions`), not the gateway-facing `RouteEntry`.

**Wire type — `AppVersionInfo` gets a `net_policy` field** (`crates/core/src/types.rs:50`). A new owned `core` type (core must not depend on `runtime`):

```rust
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct AppNetPolicy {
    pub allow: Vec<NetAllowEntry>,   // empty = Denied; non-empty = reviewed Allowlist
    pub max_sockets: u32,
    pub egress_ceiling_bytes: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct NetAllowEntry { pub host: String, pub port: u16 }

// on AppVersionInfo:
#[serde(default)]
pub net_policy: AppNetPolicy,   // default = empty allow = Denied
```

`#[serde(default)]` keeps it forward-loadable and means "no grant row ⇒ Denied" with zero ceremony (mirrors how `SpendState`/`AccountState` default onto `RouteEntry`). **`Trusted` is intentionally NOT representable** in `AppNetPolicy` — the creator channel can express only `Denied` or reviewed `Allowlist`. `Trusted` is reserved for the operator-internal migrate vector (§2), so a compromised grant row can never escalate a creator app to host-unrestricted egress.

**Producer — `control` `get_versions` joins the grant table** (`crates/control/src/registry.rs:476`, already `LEFT JOIN`s the plan catalog for `runtime_limits_json`):

- A read over `zeroship.app_net_grants` keyed by `app_id` builds `allow`.
- `max_sockets` / `egress_ceiling_bytes` come from the **plan catalog** (tier property: how *much*), not the grant table (which hosts). Free tier = low caps; paid = higher.
- No grant rows ⇒ `AppNetPolicy::default()` ⇒ worker constructs `Denied`. Safe by construction.

**Consumer — `cache::load_app` constructs the `NetPolicy`** (thread through `sync.rs:282` and `handler.rs:1229`, same as `info.runtime` flows; translate-and-apply at the builder `cache.rs:288-297`):

```rust
let net_policy = if app_net.allow.is_empty() {
    NetPolicy::Denied
} else {
    // Per-entry skip-and-log, NOT all-or-nothing: one malformed grant row
    // must not silently brick every other granted host for the app. A bad
    // entry is dropped + logged; the rest still resolve. (Should-fix:
    // collect::<Result<Vec>>() would deny the whole app on a single typo.)
    let mut entries = Vec::with_capacity(app_net.allow.len());
    for e in &app_net.allow {
        match HostPort::try_new(e.host.clone(), e.port) {
            Ok(hp) => entries.push(hp),
            Err(err) => tracing::error!(
                app_id=%app_id, host=%e.host, port=e.port, error=%err,
                "net grant entry rejected at load; skipping this host"),
        }
    }
    if entries.is_empty() {
        NetPolicy::Denied
    } else {
        match NetPolicy::allowlist(entries, app_net.max_sockets, app_net.egress_ceiling_bytes) {
            Ok(p) => p,
            // operator_reviewed rejected the SET as a whole (e.g. a frontable
            // wildcard slipped the authoring gate) → fail closed for the app.
            Err(e) => { tracing::error!(app_id=%app_id, error=%e, "net grant set rejected; denying"); NetPolicy::Denied }
        }
    }
};
let mut builder = Runtime::builder()
    .modules(modules).env_vars(env_vars).limits(limits).plugins(plugins)
    .app_id(app_id)
    .net_policy(net_policy);   // <-- the producer call the review says is missing
```

`NetPolicy::allowlist` re-runs `operator_reviewed` (`net_policy.rs:80-90`), so the runtime **re-validates** every entry (defense in depth; a hand-edited DB row cannot inject an unreviewed wildcard) and **fails closed to `Denied`**, never aborts the isolate. Two layers of fail-closed: per-*entry* parse errors skip-and-log (one typo doesn't brick the whole app); a whole-*set* `operator_reviewed` reject (a frontable/bare wildcard that escaped the authoring gate) denies the app entirely. In practice authoring-time `validate_reviewed` makes whole-set rejects unreachable — the runtime `allowlist()` reject is dead defense — but the per-entry skip is the live ergonomic control. Add `net_policy` to the `needs_reload` comparison (`sync.rs:173`) so revoking a grant tears down and rebuilds the isolate on the next reconcile tick (same SEC-7 staleness reasoning as env-version reloads), rather than waiting for LRU eviction. **Revocation SLA:** up to one reconcile/poll interval (the `/internal/versions` tick) + a full isolate swap; document this as the grant-revocation latency (matches the limits/env-version precedent exactly).

End-to-end: operator INSERT (validated) → `get_versions` JOIN → `/internal/versions` poll → `cache::load_app` → `Runtime::builder().net_policy(Allowlist)` → `connect.rs:114-119` gate per `connect()`. No new transport, no new poll loop.

### 1.4 Product UX — request (low-trust) vs grant (high-trust)

Outbound raw TCP is a **capability**, granted out-of-band like privileged scopes.

- **Request (creator side):** (a) a manifest `net.requests: [{host, port, reason}]` hint emitted by the AI builder — treated strictly as a request surface, diffed requested-vs-granted on deploy, ungranted hosts boot `Denied`; and/or (b) a dashboard "Request outbound connection" form creating a pending review item.
- **Grant (operator side):** a human (or an automated policy for low-risk exact-host destinations) reviews against `validate_reviewed` and writes the `app_net_grants` row. Same shape as OAuth scopes (declare → authority validates) but with a **human gate** because the blast radius (confused-deputy egress) warrants it.
- **AI-builder framing:** "send order confirmations via SendGrid" → builder emits the manifest hint + an `@zeroship/email`-style wrapper; SMTP fails closed (capability-denied / `ERR_MODULE_NOT_FOUND`) until an operator grants `smtp.sendgrid.net:587` — a reviewable, visible state, not a silent open door. Most creators should prefer higher-level `@zeroship/*` packages over `fetch`; raw `node:net` is the deliberately-gated escape hatch for self-hosted DB/SMTP.

### 1.5 Billing — two distinct axes, do not conflate

> **Prerequisite-set correction (verified against branch HEAD, not the review doc).** An earlier draft treated the egress fuse (S2), CA-pinning (T1), the `runtime_tls` split (A3), and the socket-leak fix (AL1/AL2) as open prerequisites. **They are all already committed on `node-net-tls`.** `reset_dispatch_egress()` (`node/net/caps.rs:20-24`) zeroes both `native_net_egress_bytes` and `native_net_egress_exhausted`, and it is wired at the top of `call_fetch_handler` (`core/runtime.rs:1584`) — **per dispatch**. Commit `50564122` ("egress brick (S2) + real ca-pinning (T1) + full-duplex plain TCP (AL1)") and `26be1fa8` (A3/A4/A6 `runtime_tls` split + centralized connect-auth) landed them. The fuse no longer latches for the isolate's life; it resets every dispatch. **The genuinely-open coupled items are only S3, S7, and S9** (see §1.6). Do not re-derive prerequisites from the review doc — derive them from `git log` on the branch.

- **`egress_ceiling_bytes` (per-isolate hard fuse, set by this producer)** is a catastrophe stop, not the billing control. Because S2 landed, the fuse is **per-dispatch**: a trip closes the offending socket and surfaces `ERR_NET_EGRESS_CAP` for the rest of *that* dispatch, then `reset_dispatch_egress` clears it on the next `call_fetch_handler` entry. The producer therefore sets the ceiling as a generous *single-dispatch* blast-radius backstop (a compromised dependency exfiltrating in one request), not a durable economic limit. Durable enforcement stays on the gateway spend path. There is no longer a "must land together with S2" coupling — S2 is done; the only remaining sizing question is the per-plan ceiling value (operator decision, §4-Q5).
- **Spend enforcement (recoverable, the real economic control)** stays where metering lives: `net_egress_bytes` accrues into the period (`caps.rs:262-268` records it on every successful write), the spend engine drives Warn→Degrade→Block, the gateway throttles/402s. Net **ingress is currently unmetered (review S3)** — a granted app pulling unbounded data from an allowlisted host (`COPY … TO STDOUT`) is an invisible download-proxy; emit `net_ingress_bytes` alongside the producer, since grants make this reachable for the first time. **This is genuinely open** and is the one billing item that must ship *with* S1 (grants are what make ingress abuse reachable).

### 1.6 Coupled review items to sequence with S1 (re-derived against branch HEAD)

**Already landed on `node-net-tls` — NOT prerequisites (verify with `git log`, not the review):**
- ~~S2~~ per-dispatch egress fuse reset — `caps.rs:20` + `runtime.rs:1584` (commit `50564122`). Done.
- ~~T1~~ CA-pin-not-augment on the admin/trusted channel — `50564122`. Done (matters for Phase E §2.3, already present).
- ~~A3~~ `runtime_tls` feature split (decouple `node:tls` from WS) — `26be1fa8`. Done.
- ~~AL1/AL2~~ full-duplex plain TCP + socket-leak fix — `50564122`. Done.

**Genuinely open — sequence WITH S1:**
- **S3** (`net_ingress_bytes` metering) — ships with S1; grants make ingress abuse reachable for the first time. The producer adds nothing here, but the metering counter must exist before grants go live.
- **S7** (move `FRONTABLE_WILDCARD_SUFFIXES`, `net_policy.rs:219`, to an operator-editable catalog row; runtime const stays as a compiled-in backstop so a catalog outage fails *closed*, not open).
- **S9** (use `HostPort::try_new` / `operator_reviewed` on the producer + consumer path — both return `Result`, so a bad row surfaces as a 4xx at authoring time and a `Denied` fallback at load time, never `HostPort::new`'s panic at `net_policy.rs:135`).

### 1.7 Risks / caveats

- **DNS rebinding / TOCTOU.** The allowlist matches the hostname *string* at connect; SSRF resolves the IP separately. The host-string allowlist and the IP-level SSRF guard are **two independent rails** — a grant must never be read as "SSRF-exempt." Allowlist (like Trusted) still goes through SSRF resolution.
- **Grant ↔ deploy lifecycle skew.** Grants persist across redeploys (intended); a redeploy that drops a dependency leaves a stale grant. Consider `granted_at` + TTL / periodic operator review.
- **Reload amplification.** Wiring `net_policy` into `needs_reload` makes a grant edit a full isolate swap (destroys module state) on the next reconcile tick — correct (matches limits/env precedent) but not a hot-reload.

---

## 2. Phase E — branch convergence + the JsDriverBackend (live MySQL via node:net)

### 2.1 Grounding — what the code already shows

1. **The "P2b" migrate seam refactor is already landed.** `MigrationBackend` (`apply/backend/mod.rs:153`) carries `type SessionSnapshot` (`:157`), `ddl_is_transactional()` (`:167`), the dialect-driven `uses_two_phase_path()` (`:175`), and unified `apply_one(…, had_inflight, …)` (`:215`). `BackendError` is de-PG-typed — `Box<dyn Error + Send + Sync>` with a *convenience* `From<compio_postgres::Error>` (`executor.rs:123,157`) — so it already admits a `JsDriverError`. **The seam was built for exactly this.**
2. **`SqlDialect::Mysql` + `MysqlSchemaRenderer` already exist** (`zeroship-schema/src/query.rs:110,414`): the mysql proposal's Phase 1 (render) + Phase 2 (apply-strategy unification) are in. Phase E is the deferred **Phase 3 (live execution)**, realized via a JS driver over `node:net` instead of `compio-mysql`.
3. **A Rust↔V8 embedding exists in the migrate family — but it is the WRONG SHAPE for a live DB driver.** The former in-tree JS authoring adapter depends on `zeroship-runtime` and `eval`s `schema.js` in the V8 sandbox. **Critical correction (verified `eval.rs:141-180`):** that embedding is a *synchronous, zero-I/O* `runtime.with_scope(|scope| …)` that loads a module graph whose top-level code is "synchronous — no top-level await" (its own comment, `:164`), then *defensively* drains microtasks (`perform_microtask_checkpoint`, `:170`) and reads a global back out. It never touches the compio event loop, never awaits a socket, never resolves a Promise that depends on I/O. A live `mysql2` connection over `node:net`, **held open across the whole plan** with many request/response round-trips, is a fundamentally different driving model: it requires pumping the runtime's *async event loop* (compio socket completions → JS `'data'` events → microtask/Promise resolution) until each `conn.execute(...)` Promise settles. **JsDriverBackend is therefore a NEW embedding shape, not a reuse of the `eval.rs` one.** The reusable part is narrow: the `Runtime::builder()` construction, `setup_globals`/`install_*`, and the module-loading machinery. The *driving* part — pumping a live async loop while holding a stateful socket across awaits — is the real Phase E long pole and is specified concretely in §2.3a below.

Net: Phase E collapses from "invent a Rust-hosts-JS-driver bridge" to "construct a `NetPolicy::Trusted` runtime, build a **bespoke async-drive pump** that owns the compio loop non-reentrantly, define one query-marshalling seam, and prove the pump against the prepared-statement path." The construction and the seam are cheap; the pump is the work.

### 2.2 Branch convergence

Both branches fork from the same merge-base `797c8742` (current `main`); a 3-way merge is well-defined. The bulk is disjoint at the crate level, but **both branches extend the same seven V8-core seam files** (`comm -12` of the two name-only diffs):

```
Cargo.lock
crates/runtime/src/core/{dispatch,init,runtime,state}.rs
crates/runtime/src/lib.rs
crates/worker/src/cache.rs
```

All seven are **additive, keep-both** conflicts (no rewrites of shared logic):

- **`runtime.rs` (`RuntimeBuilder`):** node-net-tls adds `net_policy` field + `.net_policy()` (`:573`) + build plumb; migrate adds `runtime_descriptor` field + method + plumb. Keep both fields/methods/args.
- **`cache.rs` (`make_isolate`/`load_app`):** node-net-tls adds the **eviction guards** to `evict_lru` (`!is_isolate_leased()` `:408`, `active_native_socket_count()`/`close_native_sockets_for_eviction()` `:411,432`) and a `.net_policy(NetPolicy::trusted(…))` only under `#[cfg(test)]` (`:482`). **It does NOT add `.net_policy` to the production `load_app` (`:246`) — that absent call is precisely the S1 gap (§1.1); S1 is what adds it.** Migrate changes the `load_app` signature (`deploy_hash`, `runtime_descriptor`) + appends `.runtime_descriptor(...)` + an env-vars insert. The merge is therefore: keep migrate's `load_app` signature change + keep node-net-tls's `evict_lru` guards (disjoint regions — the guards are in `evict_lru`, the signature change is in `load_app`). **The S1 producer's `.net_policy(...)` builder call (§1.3) is a *post-merge S1 edit to `load_app`*, not a merge conflict** — it does not exist on either branch yet.
- **`state.rs`:** node-net-tls adds `net_policy` + egress counters; migrate adds `runtime_descriptor`. Adjacent fields, keep both.
- **`lib.rs`:** node-net-tls re-exports `NetPolicy`; migrate touches 2 export lines. Trivial.
- **`init.rs` / `dispatch.rs`:** migrate adds schema-descriptor globals + dispatch (`+196`/`+113`); node-net-tls adds `process.nextTick` ordering + native-turn pump. Different regions, low-risk — **must be eyeballed, not auto-resolved** (R6).
- **`Cargo.lock`:** regenerate, never hand-merge.

**Mechanism — a dedicated integration branch** (don't force-land either epic):

```
git switch -c integ/migrate-jsdriver main          # off 797c8742
git merge node-net-tls          # smaller, arch-reviewed, contained → land first
                                # resolve 7 keep-both seam files; regen Cargo.lock
cargo test -p zeroship-runtime && cargo test -p zeroship-worker   # gate A (full targets)
git merge feat/db-migration-engine   # the epic; same 7 files pre-reconciled
cargo test -p zeroship-migrate -p zeroship-schema                  # gate B (PG :5440)
```

node-net-tls **first** because it is the smaller, already-reviewed body — the second merge then re-applies the migrate half onto a file that already has the net half (the easy direction). **Verification gate before any Phase E code:** both per-crate suites green on `integ/migrate-jsdriver` — node-net-tls's live-driver `node_*_e2e` (mysql2/ioredis/pg) AND migrate's full PG (`--test-threads=1`, live :5440) + SQLite suites. Full per-crate targets, not `--lib` (per `feedback_verify_full_suite_not_lib`). The merged root `Cargo.toml` keeps migrate's members + path deps; node-net-tls adds no workspace members. The single line that makes the epics need each other: the driver Runtime is now built with `.net_policy(NetPolicy::Trusted{…})`, a method that exists only post-merge.

### 2.3 The JsDriverBackend — one transport seam, everything else existing Rust

`PostgresBackend` (`postgres.rs:21`) wraps `&compio_postgres::Client`; every method delegates to a free fn doing `batch_execute`/`query→Row`. JsDriverBackend mirrors it, but the "connection" is a handle into a **Trusted V8 isolate running mysql2**, and the primitive is a **two-method JSON transport**:

```rust
// apply/backend/mysql_js/transport.rs (new)
pub struct JsDriverConn {
    rt: zeroship_runtime::Runtime,   // the Trusted driver isolate (dedicated; owns its pump)
    // NOTE: no RuntimeLease here — see M3 below. A dedicated standalone Runtime
    // lives in no worker AppCache, so nothing runs evict_lru against it; the
    // lease (which only increments a counter cache.rs reads) would be a no-op.
    // The plan-lifetime `JsDriverConn` binding + RAII Drop + the watchdog ARE
    // the lifecycle controls.
}
impl JsDriverConn {
    async fn exec(&self, sql: &str) -> Result<(), JsDriverError>;                    // DDL / no-resultset
    async fn query_json(&self, sql: &str, binds: &[BindValue]) -> Result<RowSet, JsDriverError>; // mysql2 prepared, positional ?
}
pub struct RowSet { pub rows: Vec<serde_json::Map<String, serde_json::Value>> }
```

`RowSet` is the **entire data boundary**. Everything above it — MySQL render (`MysqlSchemaRenderer`/`DmlRenderer`), journal SQL, parsing into `AppliedEntry`/`SchemaSnapshot`, `decide()`, the recovery state machine — is **pure Rust that already exists**. `MysqlBackend: MigrationBackend` holds a `JsDriverConn` and looks structurally identical to `postgres.rs`; `ddl_is_transactional()` returns `false` — the whole reason the path changes. **But the `exec`/`query_json` signatures hide the real work: how those two `async fn`s drive a live, stateful, connection-held-across-awaits mysql2 socket. That is M2, specified in §2.3a — it is NOT free reuse of the `eval.rs` synchronous embedding.**

### 2.3a — Driver-isolate execution model (the real Phase E long pole, M2)

**The mismatch.** The proven live-driver path (the mysql2 e2e, commit `2817a0c5`, harness `tests/support/node_realworld.rs:77-94`) runs the *entire* connection lifecycle — `createConnection` → N×`query` → `end` — **inside one `fetch` dispatch**. `call_fetch_handler` returns `FetchOutcome::Pending { rx }`; the caller `await`s `rx.recv()` on the compio runtime while `start_pump()` drives node:net completions → `'data'` events → Promise resolution until the fetch Promise settles and the pump posts the result over the oneshot. **This is a one-shot request/response primitive: one dispatch = one whole connection lifecycle.** Phase E cannot use it as-is, because the migrate executor must hold one connection open *across* many Rust-side decisions interleaved with SQL: render fragment-1 (Rust) → exec → read journal → `decide()` (Rust) → render fragment-2 → exec → `GET_LOCK` held the entire time. The connection must persist *between* Rust round-trips; "one fetch = whole lifecycle" does not express that.

**The model (recommended): a long-lived command-loop driver module reusing the proven pump.** The platform-authored driver entry opens the connection **once** into module state and then never returns — it runs a command loop parked on a native await:

```js
// driver-entry.js (platform-authored, lives in the Trusted isolate)
import mysql from "mysql2/promise";
const conn = await mysql.createConnection(__zsDriverDsn());   // DSN injected by Rust, host→node:net
for (;;) {
  const cmd = await __zsNextCommand();          // native Promise: resolves when Rust pushes a command
  try {
    if (cmd.kind === "exec")  { await conn.execute(cmd.sql);                 __zsResolve(cmd.id, {ok:[]}); }
    else                      { const [rows] = await conn.execute(cmd.sql, cmd.binds); __zsResolve(cmd.id, {ok:rows}); }
  } catch (e) { __zsResolve(cmd.id, {err:{code:e.errno, sqlState:e.sqlState, message:e.message}}); }
}
```

Rust side: `JsDriverConn::exec`/`query_json` push a command (id, sql, binds) into a Rust→JS queue, `notify_pump()`, then `await` a per-command oneshot. The pump drives the parked `__zsNextCommand()` Promise to resolve with the command, mysql2 issues the round-trip over the *already-open* socket, `__zsResolve` posts the RowSet back over the oneshot. **The connection lives across commands because the JS loop never unwinds** — `conn` stays in module scope; the open node:net socket stays registered in `RuntimeState` between commands, kept alive by the pump. Each `exec`/`query_json` is structurally `drive_fetch_outcome` **reused N times against one parked connection** instead of once per lifecycle.

**Two new native primitives this requires** (small, Trusted-isolate-only, behind the migrate vector — never on the creator surface): `__zsNextCommand()` (returns a Promise the pump resolves from a Rust-side command mailbox) and `__zsResolve(id, payload)` (posts a result the awaiting Rust oneshot receives). These are the *only* new runtime surface; they are a command-channel analogue of the existing dispatch oneshot, not a new event loop.

**Non-reentrant loop ownership (the critique's explicit demand).** Exactly one command is in flight at a time: `JsDriverConn` serializes (`&mut self` on the command path, or an internal in-flight flag). The pump owns the compio loop; Rust `await`s the oneshot and does **not** re-enter the pump while it runs — identical to `drive_fetch_outcome`'s single outstanding `rx.recv()`. **If the single-thread interleave proves to entangle the migrate executor's own compio loop with the driver pump** (the executor `block_on`s its plan; the driver pump wants the same loop), the fallback is the **dedicated-thread variant**: the driver `Runtime` + its `start_pump` run on their own compio thread; `JsDriverConn::{exec,query_json}` send commands over a cross-thread channel and await a cross-thread oneshot; the executor's loop and the driver's pump never touch the same `RuntimeInner` (`Runtime` is `!Send`, so the isolate stays on its thread — only the command/oneshot messages cross). E0.5 (below) decides single-thread vs dedicated-thread empirically; the *protocol* (command + oneshot) is identical either way.

**This is unsolved embedding work, not reuse.** E1's "compiles against the unchanged trait" gate does **not** exercise the pump; a green compile proves the *seam* but not the *drive*. The plan therefore adds **E0.5 — a driver-isolate execution-model spike** (below) that must pass before any E1 backend code is written.

**Acquiring the connection — `NetPolicy::Trusted` to the DB host:port.** The driver isolate is constructed by the former JS authoring adapter:

```rust
let rt = Runtime::builder()
    .net_policy(NetPolicy::trusted(/*max_sockets*/ 4, /*egress_ceiling*/ HIGH))
    .modules(/* platform-authored driver entry that imports mysql2 */)
    .build();
```

`Trusted` (not `Allowlist`) because the migrate engine is operator/platform code: the DB host comes from the operator's DSN (`ExecutorConfig`, reached only via token-gated `platform()`/`trusted()` constructors, `conn.rs:258,292`). `Trusted` skips host-matching but **still enforces SSRF, the per-isolate socket cap, the process-wide socket CAS cap, and the egress ceiling** (`net_policy.rs:6-15`). The DSN host:port flows into `mysql2.createConnection({host,port,...})` → `net.connect` → `authorize_connect` (`connect.rs:78`) → SSRF resolve (`:372`) → TCP. TLS for managed MySQL rides mysql2's `ssl:{ca}` → `node:tls` (behind the merged `runtime_tls` feature, review A3/T2); the **T1 ca-pin-not-augment** fix matters — the admin channel must pin the operator CA, not accept public roots. `validate_connect_kind` (`connect.rs:154`) blocks `node:net` from `query`/`mutation` handlers but allows `action`/`stream`/trusted (`None ⇒ allowed`); the migrate isolate runs no creator RPC handler (`current_kind() == None`), so the kind rail is satisfied by construction.

**Lifecycle: the dedicated topology makes `RuntimeLease` a no-op — the watchdog + RAII Drop are the real controls (M3).** `apply_one` on MySQL is multi-round-trip and multi-implicit-commit, plus a session-held `GET_LOCK` spanning the whole plan; if the isolate dies mid-sequence the mysql2 connection drops, the session lock vanishes, recovery is forced. An earlier draft proposed holding a `RuntimeLease` (`runtime.rs:228`, `lease_isolate()` `:428`) to keep the isolate un-evictable. **That is wrong for the recommended topology.** `RuntimeLease` is explicitly a *worker-AppCache* concept — its doc (`runtime.rs:222-238`) says "worker LRU eviction must treat the runtime as un-evictable," and it only increments `isolate_lease_count`, which **only `cache.rs` `evict_lru` reads** (`cache.rs:408` filters leased isolates). A **dedicated, standalone** migrate Runtime (the §2.3a recommendation, matching `eval.rs:141`'s `Runtime::builder().build()` with no cache) lives in **no AppCache** — nothing runs `evict_lru` against it — so the lease does literally nothing a plain `let conn = JsDriverConn::open(...)` binding doesn't. **Decision: dedicated topology → drop the lease framing entirely.** The real lifecycle controls are: (1) the plan-lifetime `JsDriverConn` binding whose **RAII `Drop`** tears down the `Runtime` (the isolate + its sockets close on drop; `close_native_sockets_for_eviction` is available if an explicit pre-drop close is wanted), and (2) a **wall-clock watchdog**. The watchdog bounds a hung transaction using the engine's own envelope — `statement_timeout` (60s) + `lock_timeout` (3s) (`conn.rs:48,80`) → set MySQL `max_execution_time` for SELECTs, wrap every command in a `compio::time::timeout`, and arm a top-level deadline = `Σ(per-statement budgets) + GET_LOCK timeout + slack`; on expiry the `JsDriverConn` is dropped and the connection destroyed (trips fragment-recovery on next run — never corruption). Tests assert teardown via `InnerProbe::strong_count() == 0` (`runtime.rs:212-219`) after drop. **The only world where a lease matters is the rejected pool-shared-in-worker-cache topology** — which would run the **Trusted** migrate isolate *inside the end-user worker process*, mixing trust domains; that is a non-starter and is why dedicated is recommended (§2.7-R1).

**The SessionSnapshot seam suffices — confirmed. The typed-error seam needs the RIGHT `ApplyError` arm (M4).** `type SessionSnapshot` is only round-tripped (`snapshot_session`→`restore_session`), never inspected; `MysqlBackend` sets it to `MysqlSessionSnapshot { innodb_lock_wait_timeout, sql_mode }` — no `search_path` (MySQL has none) — **zero trait change**, confirmed (`backend/mod.rs:157,191`). For errors, the critique is right that `Backend(String)` would flatten errno/SQLSTATE — but the fix needs **no enum change** because `ApplyError` **already** carries a downcastable arm: `ApplyError::Db(#[source] BackendError)` (`executor.rs:168`), where `BackendError(Box<dyn Error + Send + Sync>)` (`:123`) exposes `downcast_ref::<E>()` (`:137`). The PG path routes driver errors through `Db(BackendError)` and downcasts SQLSTATE in tests; **MySQL must route `JsDriverError` through the same `ApplyError::Db(BackendError::new(js_err))` arm — NOT `ApplyError::Backend(String)`** (the stringly arm SQLite uses at `sqlite/mod.rs:345`, intended only for already-operator-facing messages). `enum JsDriverError { Transport, Remote { code: u16, sqlstate, message }, Marshal }: Error + Send + Sync` then downcasts via `BackendError::downcast_ref::<JsDriverError>()` **exactly as PG downcasts `compio_postgres::Error`**. The recovery branches that need error *class* — lock-timeout-vs-duplicate-key in `decide()`/recovery, crash-recovery `had_inflight` — must therefore raise `Db(BackendError::new(JsDriverError::Remote{..}))`, never `Backend(String)`, so the class survives the apply path. **Net: zero trait change AND zero `ApplyError`/`RollbackError` enum change — but only by using the `Db`/`BackendError` channel, which the prior draft conflated with the drift channel.** (Caveat: the existing PG impl uses `ApplyError::Backend(String)` in a few backfill spots, `postgres.rs:186,204`; MySQL deliberately does not, to preserve downcastability through recovery.) The transport lives *inside* `MysqlBackend`, below the trait, as `&Client` lives inside `PostgresBackend` — the trait stays untouched.

**The boundary question — "does the JS driver return rows the Rust probe can consume?" — yes, decisively.** `SchemaSnapshot` (`snapshot.rs:253`) is a neutral owned struct already free of `compio_postgres::Row`; PG's `snapshot_schema` already *builds* it from rows. The JS leg substitutes the row *source* (mysql2 JSON over node:net) for the same builder. `decide()` (`existence_probe.rs:316`) is pure `(GuardProbe, &SchemaSnapshot, SqlDialect) → GuardVerdict`. The only new code is the `RowSet → SchemaSnapshot` mapper against MySQL `information_schema` spellings (the `mysql_canonical_type` fold).

### 2.4 MySQL specifics over the transport

- **Render reuse:** consumes the landed Phase-1 `MysqlSchemaRenderer` + `DmlRenderer` MySQL leg (`VARCHAR(n)`/`LONGTEXT`, `TINYINT(1)`, `DATETIME(6)`, `JSON`, `AUTO_INCREMENT`, backtick quoting, positional `?`). No new spelling.
- **`ddl_is_transactional() = false` → two-phase, every migration.** `uses_two_phase_path()` returns `true` for every versioned migration (repeatables forced atomic by the `m.flags.repeatable` short-circuit). Routes MySQL through the *exact machinery PG built for `CREATE INDEX CONCURRENTLY`* (started-marker → confined `<up>` → completed-marker, `executor.rs:2375`). Mirror-image vs SQLite (which *rejects* the non-txn path) — proof the dialect, not the migration flag, now drives the selector.
- **Confinement: MySQL `SET ROLE` ≠ PG `SET ROLE` — redesign around account identity (M5).** PG confinement (`role.rs:14-30`) works because the admin `SET ROLE`s to a **NOLOGIN NOSUPERUSER** migrator role and PG then runs *all* privilege checks as that role — the connecting role's own privileges are **dropped** for the duration. **MySQL 8 `SET ROLE` does NOT have this semantic:** it only *activates roles already granted to the current account*; it does **not** remove the base account's own directly-granted privileges. So if the coordinator authenticates as a broadly-privileged account, `SET ROLE migrator` gives **no confinement** — the DDL still runs with the base account's full rights. **PG-equivalent least-priv on MySQL requires the connecting account itself to be least-priv.** Two viable shapes: **(a, recommended) the executor authenticates the driver connection as a dedicated least-priv MySQL migrator *account*** (`'zs_migrator'@'%'` granted only DDL on the project schema + journal-table DML, no `SUPER`/`PROCESS`/cross-schema), so confinement is the connection identity — no per-fragment role toggling needed; or **(b)** a *separate* migrator-authenticated connection for the confined `<up>` fragments while a privileged coordinator connection writes journal markers (two connections, more moving parts, and a second `GET_LOCK` holder problem). **Recommend (a):** one least-priv `JsDriverConn` for the whole plan; the journal markers are writable because the migrator account is granted DML on the journal table specifically. This means `reset_role_best_effort` is a **no-op on MySQL** (there is no elevated role to drop — the account was never elevated), exactly as it no-ops on SQLite (`sqlite/mod.rs:387`) — *not* a `SET ROLE DEFAULT` body. The MySQL provisioning step (peer of PG's `ensure_migrator_role`, `role.rs:226`) is `CREATE USER … IDENTIFIED BY … ; GRANT <least-priv> …` for the migrator account. `migrator_role` as a *name* (`conn.rs:87`) is not a PG-ism, but the *`SET ROLE`-confines* mechanism is — MySQL confines by account, not by role activation.
- **Lock on one session:** `GET_LOCK` held on the same single least-priv `JsDriverConn` for the whole plan (release-on-connection-drop is what makes the watchdog/forced-abort correct — dropping the conn frees the lock). **Lock-name 64-char cap:** MySQL 8 caps `GET_LOCK` names at 64 chars and errors above; a `project_id`-derived name must be **hashed to fit** (e.g. `zs_mig_` + a truncated hex digest), mirroring how `migrator_role_name` sanitizes (`role.rs:181`).
- **Binds:** positional, non-reusable `?` (`reusable_placeholders()=false`); `BindValue`s marshal as a JSON array; mysql2's prepared `conn.execute(template, binds)` binds server-side — **never** string-interpolated, no-injection guarantee preserved.
- **Crash recovery at fragment granularity:** MySQL has no `IF NOT EXISTS` for index/column, so PG's blind-replay is unavailable. The oracle is the **per-guarded-fragment `decide()` probe**: on `had_inflight`, replay `decide()` per fragment — `SatisfiedNoop`⇒skip, `RunBare`⇒re-run, `FailDrift`⇒fail closed. Consumes `LoweredArtifact.fragments`, not the opaque `m.up`. The MySQL path must `exec` fragments **one-by-one** (each auto-commit observable) where PG runs `batch_execute(&m.up)` as one round-trip. `validate_non_txn` becomes a whole-dialect law: every fragment must be `Probeable | IdempotentReplay`; raw multi-statement `.sql` `up`s rejected at validate-time.
- **Catalog mapper:** `snapshot_schema` issues MySQL `information_schema.{TABLES,COLUMNS,STATISTICS,KEY_COLUMN_USAGE,REFERENTIAL_CONSTRAINTS,VIEWS}` SELECTs via `query_json`, then folds `RowSet → SchemaSnapshot` (read `COLUMN_TYPE` for `tinyint(1)`, strip `int(11)` widths, fold `json`/`datetime`).

### 2.5 The faithful e2e (real MySQL 8 on :3307, no shim)

New `crates/zeroship-migrate/tests/mysql_jsdriver_e2e.rs`, gated on `MYSQL_TEST_URL`/:3307 like the PG suite gates on :5440:

1. **Apply over node:net** — fixture (createTable + index) renders MySQL DDL, applies through the Trusted isolate; assert via follow-up `information_schema` `query_json`.
2. **Advisory lock is real** — two concurrent applies → the second blocks on `GET_LOCK` (or fails on short lock-timeout).
3. **Two-phase, not BEGIN-wrapped** — `started` marker exists before `completed`; DDL committed independently of the journal write (observe table present after `started`, before `completed`, at an injected pause).
4. **Journal row** — `completed` with correct checksum/version; re-run is a net-applied skip.
5. **Crash-recovery (deepest)** — kill the connection (drop the `JsDriverConn` → isolate teardown, connection destroyed) **after `CREATE TABLE` commits, before `CREATE INDEX`**; re-run; assert fragment-1 `SatisfiedNoop`, fragment-2 `RunBare`, then `completed`. The per-fragment oracle, exercised live. **Requires new test infra: a deterministic between-fragment pause seam** — the apply path needs an injected hook (test-only, e.g. a `#[cfg(test)]` callback the executor fires after each fragment auto-commit) so the test can drop the connection at a precise, reproducible point. This is not free; budget it in E2. The PG suite has no equivalent because PG recovers via blind `IF NOT EXISTS` replay; MySQL's fragment-granular recovery is what makes the pause-seam necessary.
6. **Observable mutation + drift-clean redeploy** — `snapshot_schema` round-trips; second deploy is `SatisfiedNoop` (no phantom drift).

Each fixed gap ships a regression test that fails pre-fix.

### 2.6 Phased plan

- **E0 — Convergence (prerequisite, no new code).** §2.2: integration branch, merge node-net-tls then migrate, resolve the 7 keep-both files, regen `Cargo.lock`. Gate: both full per-crate suites green. The only *mechanical* blocker the brief names.
- **E0.5 — Driver-isolate execution-model spike (the M2 gate, MUST pass before E1).** Prove §2.3a end-to-end in isolation: a long-lived command-loop driver module + the two `__zsNextCommand`/`__zsResolve` native primitives + the bespoke pump, opening **one** mysql2 connection over node:net and serving **multiple** `exec`/`query_json` commands across **separate Rust-side `await`s** against the *same* parked connection (e.g. `CREATE TEMP TABLE`, then a later `SELECT` on it — proving the socket and session state persist between commands). Decide single-thread-reuse-of-pump vs dedicated-thread empirically here and record the choice. **Exit criterion: a Rust test that holds a connection open across ≥3 separated commands with non-reentrant loop ownership proven (no nested pump drive).** This is the real research; if it fails, the E1→E2 plan does not proceed as written. E1's compile gate does **not** substitute for this.
- **E1 — JsDriverBackend skeleton (no live MySQL).** In the former JS authoring adapter: wrap the E0.5 spike into `JsDriverConn::{exec,query_json}` + the wall-clock watchdog (no `RuntimeLease` — §2.3/M3); stub `MysqlBackend: MigrationBackend` delegating render + journal SQL, transport pointed at a loopback/echo harness. Gate: `MysqlBackend` compiles against the **unchanged** trait, errors routed through `ApplyError::Db(BackendError::new(JsDriverError))` (§2.3/M4); unit-test `RowSet → SchemaSnapshot`. **Note: a green compile proves the seam, NOT the drive — E0.5 is what proves the drive.**
- **E2 — Live MySQL apply e2e.** Wire to real MySQL 8 :3307 via mysql2; implement `apply_one` two-phase (fragments one-by-one), journal I/O, `snapshot_schema`, `GET_LOCK` (64-char-hashed name), confinement via the **least-priv migrator account** (§2.4/M5, not `SET ROLE`), `reset_role_best_effort` a no-op. Land §2.5 including crash-recovery. **The payoff — live MySQL with zero `compio-mysql`.**
- **E3 — pg-over-node:net, second proof.** A `JsDriverBackend` variant running the `pg` JS driver over node:net against PG :5440 on the *existing* PG fixtures, asserting byte-identical journal/schema vs native `compio_postgres`. Honesty test: the transport seam is dialect-agnostic; stresses the *transport* (PG-via-JS keeps transactional DDL) independently of MySQL's non-txn recovery quirks. **Recommend test-only, not a production backend** (production PG stays on `compio_postgres` — faster, no V8).
- **E4 (optional).** MySQL `online()`/`shadow()` (`ALGORITHM=INPLACE`), PK-keyed paged backfill (no `ctid`), `mysql_fk_definition` normaliser for deferred-FK idempotent redeploy.

### 2.7 Risks / open questions

- **R1 — driver isolate lifecycle (M3).** **Decided: dedicated standalone Runtime.** `RuntimeLease` is a no-op for a Runtime in no AppCache — dropped from the design. Lifecycle = plan-lifetime `JsDriverConn` binding (RAII `Drop` tears down the isolate + sockets) + wall-clock watchdog (statement/lock-timeout envelope + forced abort-on-expiry). Pool-shared-in-worker-cache is rejected: it would co-locate the Trusted migrate isolate with end-user worker code, mixing trust domains.
- **R2 — TLS to managed MySQL — prerequisites ALREADY LANDED.** `runtime_tls` (A3, commit `26be1fa8`) and **T1 ca-pin-not-augment** (`50564122`) are committed on node-net-tls. Phase E builds on them post-E0-convergence; nothing to wait for. **R4 cold-cache makes TLS effectively mandatory for managed MySQL** (see below), so wire the mysql2 `ssl:{ca}` → `node:tls` path in E2, pinning the operator CA (not public roots — that's what T1 enforces).
- **R3 — mysql2's `node:events`/`node:net` surface (review X1).** `node/events` lacks `setMaxListeners`/`prependListener`. The live mysql2 e2e in node-net-tls (commit `2817a0c5`) uses `conn.query` with a binds array (`tests/node_mysql2_e2e.rs:53`), which mysql2 routes through the **text protocol**, not the binary prepared-statement protocol. Phase E uses `conn.execute` (binary prepared). **E0.5 must confirm `conn.execute` specifically** — the existing test does not exercise it.
- **R4 — auth, cold-cache caveat.** MySQL 8 `caching_sha2_password` has TWO paths: the **cached fast path** is a SHA-2 challenge/response needing no extra transport — but it is only reachable *after* a prior successful auth has populated the server-side cache. The **cold-cache path** (fresh server, post-restart, post-`FLUSH PRIVILEGES`, or first-ever connect) requires **either TLS** or **RSA public-key retrieval** (`allowPublicKeyRetrieval`, itself a MITM surface). For managed MySQL this makes **TLS effectively mandatory** and hard-ties E2 to the `runtime_tls`/T1 work (R2). mysql2 implements all of this, not our Rust — still **free** transport-wise, the reason JS-driver-as-transport sidesteps the `compio-mysql` long pole — but the E2 :3307 test must exercise the **cold-cache** connect (first connect after a fresh container / `FLUSH PRIVILEGES`), not just the steady-state cached path, and must run with TLS.
- **R5 — perf.** JSON-marshalling every row across V8 is slower than `compio_postgres::Row` — irrelevant: migrations run out-of-band at deploy, not the request hot path. Do not optimize.
- **R6 — `init.rs`/`dispatch.rs` merge regions.** Low-risk keep-both, but not auto-resolvable; eyeball during E0.

---

## 3. Dependency order — the three tracks are NOT a chain

```
                         current main (797c8742)
                                 |
        ┌────────────────────────┼─────────────────────────────┐
        |                        |                              |
   ┌────▼─────┐         ┌────────▼─────────┐          ┌─────────▼──────────┐
   │   S1     │         │   Phase E (E0…)  │          │  migrate-executor  │
   │ NetPolicy│         │  convergence +   │          │  Trusted path      │
   │ producer │         │  JsDriverBackend │          │  (node:net live)   │
   └────┬─────┘         └────────┬─────────┘          └─────────┬──────────┘
        │                        │                              │
 needs ONLY the           needs BRANCH                  needs NEITHER —
 node-net-tls runtime     CONVERGENCE first             ships on node-net-tls
 primitive + a control-   (E0: merge node-net-tls       alone; constructs its
 plane producer.          then migrate). Independent    own Runtime.net_policy
 INDEPENDENT of Phase E.  of S1 entirely.               (Trusted) directly.
```

- **S1 is independent of Phase E.** S1 only needs the landed `NetPolicy` enforcement (including the **already-committed** S2/T1/AL1/AL2/A3) + the new control-plane producer seam. It touches `core`/`control`/`worker`, not the migrate crates. It can be implemented and merged on its own timeline — **gated only on its own genuinely-open coupled items (S3/S7/S9 alongside; S2 is DONE)**, not on the migrate epic.
- **Phase E needs branch convergence.** E1+ cannot start until E0 (merge node-net-tls + feat/db-migration-engine on `integ/migrate-jsdriver`, 7 keep-both files, both suites green). E0 is the only blocker the brief names, and it is mechanical.
- **The migrate-executor Trusted path needs neither.** It is a distinct construction site that builds its own `Runtime` with `.net_policy(NetPolicy::trusted(...))` outside the worker's `cache::load_app`. `AppNetPolicy` deliberately cannot encode `Trusted`, so the creator-app channel (S1) can never escalate to it — and this Trusted vector rides on node-net-tls alone, not on S1's producer and not on the full Phase E convergence (Phase E is what *consumes* it for live MySQL, but the runtime capability stands on its own). The migrate crate's own `TrustProfile::Trusted` (`appbase-migrate/.../model/policy.rs:52`) is a *separate* trust axis (SQL deny-list), not the runtime `NetPolicy`.

**Recommended sequencing on return:**
1. Confirm S1's open product questions (§ below); implement S1 with **S3 (`net_ingress_bytes`)** alongside (S2/T1/A3 already landed) — S1 is self-contained and unblocks creator-side outbound TCP independently.
2. In parallel (no dependency), run **E0** convergence on `integ/migrate-jsdriver`; gate on both full suites; then E1→E2 for live MySQL, E3 as a test-only PG-via-JS honesty proof.

---

## 4. Open decisions for the operator

**S1 (§1.4, §1.6):**
1. **Auto-grant policy** — auto-grant exact host:port to a curated safe set (SendGrid/Twilio/managed-Postgres), human-review only wildcards/unknowns?
2. **Per-plan gating of the capability** — is raw `node:net` paid-tier-only (free tier = `Denied`, no grants accepted), or all-tiers-subject-to-review? Affects whether grant writes check the plan.
3. **Grant scope granularity** — per-app (proposed, tighter) or per-creator (fewer review actions)?
4. **Frontable-suffix catalog ownership (S7)** — confirm moving `FRONTABLE_WILDCARD_SUFFIXES` (`net_policy.rs:219`) to an operator-editable catalog (runtime const stays as fail-closed backstop).
5. **Egress ceiling / `max_sockets` sizing per plan** — the *only* remaining egress question. **S2 is DONE** (per-dispatch fuse reset, `caps.rs:20`+`runtime.rs:1584`, commit `50564122`); the ceiling is now a per-dispatch blast-radius backstop, so just confirm the per-plan ceiling/socket-cap values. **S3 (`net_ingress_bytes`) must ship with S1** — grants make ingress abuse reachable.
6. **Wildcard policy** — exact host:port only by default, wildcards operator-discretion-with-justification or disallowed in v1?

**Phase E (§2.7):**
7. **Driver isolate-drive execution model (M2, §2.3a) — the real gate.** Approve **E0.5 as a hard prerequisite spike** (long-lived command-loop module + `__zsNextCommand`/`__zsResolve` primitives + bespoke pump) before committing to E1→E2. Single-thread-pump-reuse vs dedicated-thread is decided empirically in E0.5; the command/oneshot protocol is identical either way. **This is unsolved embedding work, not reuse of the synchronous `eval.rs` path.**
8. **Driver isolate topology (R1/M3) — recommend dedicated, decision needed.** Dedicated standalone Runtime → `RuntimeLease` is dropped (no-op outside a worker AppCache); lifecycle = RAII `Drop` + wall-clock watchdog. Confirm we are NOT co-locating the Trusted migrate isolate in the worker process.
9. **MySQL confinement model (M5) — approve account-identity, not `SET ROLE`.** MySQL `SET ROLE` does not drop base-account privileges; confine via a dedicated least-priv `zs_migrator` *account* (recommended) vs a two-connection split. Confirm the provisioning step (peer of PG `ensure_migrator_role`).
10. **Error channel (M4)** — confirm MySQL routes `JsDriverError` through `ApplyError::Db(BackendError::new(..))` (downcastable, no enum change), not `ApplyError::Backend(String)`.
11. **Is E3 (pg-over-node:js) test-only or a shippable backend?** — recommend test-only; production PG stays on `compio_postgres`.
12. **TLS/auth posture for :3307 test server (R2/R4)** — T1/A3 already landed; confirm the E2 test exercises the **cold-cache** `caching_sha2_password` connect **with TLS** (cold path needs TLS or `allowPublicKeyRetrieval`), not just steady-state.
13. **mysql2 prepared-path coverage (R3)** — confirm `conn.execute` (binary prepared) works against the merged `node:events`/`node:net` surface; the existing live test (`node_mysql2_e2e.rs:53`) covers only the `query`/text path. E0.5 must close this.
14. **Crash-recovery test infra (§2.5-5)** — approve building the test-only between-fragment pause hook in the apply path (not free; required for the deepest recovery test).
