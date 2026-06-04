# Whole-codebase security audit (2026-06-03)

Autonomous, self-paced (`/loop`) security audit of the zeroship codebase, module
by module, hardest-first. Each module is reviewed by focused opus reviewers
(adversarial, file:line, honest — no manufactured findings); the orchestrator
verifies top findings against source and records them here.

**This is a review pass** — findings only, no fixes applied while the operator is
offline (anything CRITICAL + trivially-safe is flagged for action on return).

> ## ⚠️ CRITICAL findings — action before launch
> (No live exposure today — pre-launch, no production apps — but these are the must-fix-before-launch items.)
>
> 1. **RT-1 — V8 isolate escape (fastcall null signature).** Every `#[v8_class]`
>    `fastcall` method (e.g. `Headers.has`, reachable by all untrusted app JS) is
>    callable on a foreign/forged receiver, which the brand-check-free fast shim
>    reinterprets as `*const Self` → type confusion → arbitrary memory read. **The
>    worst class of finding — defeats one-isolate-per-app.** Fix: add a
>    `v8::Signature` to the FunctionTemplate + an `internal_field_count()` guard in
>    the shim + a foreign-receiver regression test. (Verified + empirically reproduced; fix verified. NOTE: the getter fastcall path needs the same fix.)
> 2. **RT-5 — sync CPU loop wedges the shared worker thread** when `cpu_limit` is
>    `None` (`unlimited`/`enterprise` plans) → permanent co-tenant starvation.
> 3. **RT-6 — off-heap memory escapes the V8 heap cap** (`Buffer.allocUnsafe`,
>    fetch bodies) → OOM-kills the whole worker process + every co-tenant isolate.
> 4. **RT-7 — LRU eviction leaks the isolate** (pump strong-`Rc` cycle; `Drop`
>    never runs) → unbounded resource leak under app churn.
>
> RT-5/6/7 mean **mutually-untrusted apps are not yet safe on a shared worker thread**.
>
> *(Sandbox micro-VM backend, if/when deployed for untrusted workloads:)*
> **SB-A1 — guest VM has unfiltered L3 egress** to cloud metadata + the internal VPC (no
> egress firewall; `ip_forward=1`) and **SB-A2 — the Nomad API is unauthenticated** — chaining
> to full sandbox-fleet control-plane + cross-tenant DB takeover from inside one guest. These are
> **GCP provisioning-script (infra) fixes**, not Rust-code bugs (the Rust job-spec path is
> injection-safe). The VM path is currently *less* contained than the V8 `fetch` SSRF guard.
>
> 5. **CT-B1 — the 15% platform fee is not enforced server-side (revenue-model integrity).**
>    The Stripe checkout session is built in creator-controlled code (`@zeroship/payments`
>    runs in the app); `applicationFeePercent` defaults to 15 but is caller-settable, and
>    the webhook's `record_payout` records `application_fee_amount` + a self-attested
>    `creator_id` verbatim — only checking `fee≤gross`, no minimum-fee floor, no binding of
>    `creator_id` to the connected account (verified `stripe_store.rs:226`). A creator sets
>    `applicationFeePercent: 0` → keeps 100%. **The platform must own checkout-session
>    creation (or enforce a server-side fee floor + account↔creator binding).**

## Threat model (platform-wide)

- App code is **untrusted JS**, one V8 isolate per app; many tenants share the
  gateway, one Postgres/Redis, the worker pool. The internet hits the gateway.
- The **native layers are the security kernel**: tenant isolation, auth/identity
  integrity, injection prevention, resource bounds, sandbox containment.
- Pre-launch (no back-compat constraint), so fixes can break shapes freely.

## Prior deep reviews (this session — not repeated here)

- **Auth pipeline** — `docs/reviews/2026-06-02-auth-pipeline-security-review.md`
  (71-agent) + red-team rounds; fixed + merged.
- **plugin-db** — `docs/reviews/2026-06-02-plugin-db-security-review.md` (5-lane);
  17/18 findings fixed (DB-5 capability-ordering remains).

## Executive summary — audit complete (9 modules, ~40 reviewer-lanes)

**Bottom line:** the **identity, isolation, and injection fundamentals are largely sound** — the
gateway's `ZeroShip-User` signing is strong, the V8 node-compat sandbox is excellent (no
host fs/net/process), `fetch` has a real IP-pinned SSRF guard, per-app tenant isolation (DB
schema, KV hash-tags, control-plane IDOR, env-secret AAD) is consistently enforced, and SQL/
command injection is closed everywhere. The platform is **not breakable by injection or by
ordinary cross-tenant IDOR.** The serious findings cluster in **three themes**:

1. **Shared-thread containment is half-built (runtime).** A V8 isolate escape (RT-1, fastcall
   null signature — *verified*) plus three resource holes (RT-5 sync-loop wedge, RT-6 off-heap
   OOM, RT-7 eviction leak) mean **mutually-untrusted apps are not yet safe on a shared worker
   thread**. RT-5 is *creator-reachable* via CT-A1 (self-escalate to the `unlimited` plan that
   sets `cpu_limit=None`).
2. **The money model is unenforced (control).** CT-B1 (*verified*): the 15% platform fee and
   revenue attribution originate in creator-controlled checkout code with no server-side floor
   or account binding — a creator keeps 100%.
3. **The micro-VM backend's network containment is broken (sandbox infra).** SB-A1/A2: a guest
   reaches cloud metadata + an unauthenticated Nomad API → fleet takeover. GCP-script fixes, and
   only if the nomad-ch backend is used for untrusted workloads.

**No live exposure today** — pre-launch, no production apps/tenants. These are the
must-fix-before-launch list. All findings are in `docs/reviews/2026-06-03-codebase-security-audit.md`
per module; the top ones below were re-verified against source by the orchestrator.

### CRITICAL (7)
| ID | Module | What | Verified |
| --- | --- | --- | --- |
| RT-1 | runtime | V8 isolate escape — fastcall methods built with no `v8::Signature` → foreign-receiver type confusion → arbitrary memory read (`Headers.has` reachable by all app JS) | ✅ src |
| RT-5 | runtime | sync `while(true){}` wedges the shared worker thread when `cpu_limit=None` (unlimited/enterprise plans) | reviewer |
| RT-6 | runtime | off-heap memory (`Buffer.allocUnsafe`, fetch bodies) escapes the V8 heap cap → whole-worker OOM | reviewer |
| RT-7 | runtime | LRU eviction leaks the isolate (pump strong-`Rc` cycle) → resource leak under churn | reviewer |
| CT-B1 | control | 15% platform fee not enforced server-side → creator keeps 100% | ✅ src |
| SB-A1 | sandbox(infra) | guest VM unfiltered L3 egress to cloud metadata + internal VPC (no egress firewall) | reviewer |
| SB-A2 | sandbox(infra) | Nomad API unauthenticated + VPC-reachable → sandbox-fleet control-plane takeover | reviewer |

### HIGH (10)
GW-1 (no apex allowlist in Host→app, *verified*) · GW-2 (`/apps/{name}` decouples tenant from origin, *verified*) · GW-3 (XFF rate-limit spoof under `trust_proxy`) · GW-4 (unbounded rate-limiter maps) · ST-1 (incomplete storage path validator, *verified*; critical once S3 lands) · ST-2 (no object-size cap → co-tenant OOM) · CT-A1 (plan self-escalation → chains to RT-5) · DR-1 (redis `set_slot` OOB panic) · DR-2 (redis SSRF-via-cluster-redirect) · SB-A3/A4 (PG `10/8`-trust + creds in metadata; VMM/kernel/rootfs not hash-verified).

### MAJOR (notable)
GW-5 (streaming idle-timeout DoS) · RT-2 (fail-open `ZEROSHIP_DEV` SSRF bypass) · RT-3 (no fetch timeout / `wall_timeout` unenforced) · KV-1 (no per-app Redis quota) · CT-C1 (no reserved env-name denylist — `WORKER_KEY`/`DATABASE_URL`/`NODE_OPTIONS`) · CT-B2 (metering spoofable + non-idempotent) · CT-B3 (`insecure_dev` disables webhook+internal auth) · W-1 (`ZeroShip-User` not bound to `app_id` — cross-app identity replay) · W-2 (redeploy drops in-flight work) · DR-3 (redis pool desync) · DR-6 (no TLS in practice for PG/Redis) · SB-1 (preview-secret RNG fallback).

### Pass-2 addendum (deeper review — gaps, red-team, SDK/RPC, request-traces, PG-pool)
**Red-team STRENGTHENED the top two:** RT-1 was **empirically reproduced** (memory-disclosure + SIGABRT PoC) **and its one-line fix verified** — highest confidence (also: the *getter* fastcall path needs the same fix); CT-B1 fully re-traced. **New HIGH:** **P2-C1** (PG-pool autocommit leaks `SET ROLE`+timeouts across tenants on setup-error/cancellation — no RAII guard, no `DISCARD ALL`; *verified*; HIGH not CRITICAL because schema-qualification still bounds it to permission-denied, not data-read) · **P2-B1/2/3** (dispatcher prototype-chain lookup; no input-size cap → JSON DoS; manifest-vs-runtime discovery divergence → app-layer authz is 100% gateway) · **P2-B4** (build-time `new Function()` eval of creator source = **builder-host RCE** on shared build infra; *verified*). **New MAJOR:** **P2-A1** (`randomFill` ArrayBuffer-detach TOCTOU → guest-triggerable worker-abort DoS) · **P2-A2** (`is_auth_host` unanchored prefix → `auth.zeroship.ai.evil.com` proxies into internal Hydra) · **P2-D1** (identity HMAC not app-bound — elevates W-1). **Reassuring negatives:** request-tracing confirmed cross-component seams TIGHT (RPC/fetch share the auth gate, per-request ctx is request-id-scoped+cleared, `__Host-`cookie app-binding holds, worker ignores `X-App-Id`); module-loader spoofing REFUTED; WebCrypto/fetch-body have no detach-TOCTOU; the tx (non-autocommit) DB path is airtight. **Not separately deep-reviewed** (lower residual value / already covered): the `crates/auth` UI-handler internals + Cedar policy files (the **auth pipeline** was deeply reviewed 2026-06-02), the CLI, and the client-side SDKs (`@zeroship/db`/`control`/`ui` — server-enforced trust boundary).

### Cross-cutting themes
- **Resource quantity, not quality.** Injection/isolation are closed; the recurring gap is *bounds* — unbounded maps (GW-4), no per-app quotas (KV-1, ST-2, CT-A2), off-heap memory (RT-6), missing timeouts (GW-5, RT-3, DR-4) — all noisy-neighbor/DoS on shared infra.
- **Fail-open env-gate footguns.** `ZEROSHIP_DEV` (RT-2), `insecure_dev` (CT-B3), `get_app_id`→"default" (ST-5), the preview-secret RNG fallback (SB-1) — security-critical controls keyed on env presence/fallbacks rather than fail-closed explicit values.
- **Host-trust assumptions at boundaries.** Host→app (GW-1/2, CT-A3/A4), `app_id` not in the worker MAC (W-1), no TLS to PG/Redis (DR-6), VM L3 egress (SB-A1) — several boundaries trust the layer in front more than they should (defense-in-depth gaps, mostly pre-launch-acceptable but worth closing).

### Recommended fix order (before launch)
1. **RT-1** (isolate escape — one-line `.signature()` + guard + test; the worst class).
2. **CT-B1** (enforce the platform fee server-side — the business model).
3. **RT-5/6/7 + CT-A1** (shared-thread containment + plan validation) — gate before multi-tenant.
4. **SB-A1/A2** (sandbox infra) — only if the micro-VM backend serves untrusted workloads.
5. The HIGH host-trust + storage + redis-driver items; then the MAJOR quota/timeout/fail-open sweep.

> **Note on the one previously-known item:** plugin-db **DB-5** (capability handle reachable at
> module top-level) and runtime **RT-4** (`__zs_env` permanent global) are the *same class* —
> a capability reachable during user-module evaluation; own-app scope. Worth a unified fix.

---

## Module order + status

| # | Module | Why | Status |
| --- | --- | --- | --- |
| 1 | **gateway** (`crates/gateway`) | internet-facing front door | ✅ 4 HIGH (host-trust ×2, rate-limit ×2), signing strong, no SSRF/traversal |
| 2 | **runtime** (`crates/runtime`) | V8 sandbox, fetch/SSRF, WS, crypto, capability boundary | ✅ **4 CRITICAL** (isolate escape + 3 DoS), 2 MAJOR SSRF; node-compat + SSRF posture strong |
| 3 | **plugin-kv + plugin-storage** | sibling native primitives (plugin-db lens) | ✅ kv STRONG (2 MAJOR quota/scan); storage 2 HIGH (path-validator, size cap), S3 N/A |
| 4 | **control** (`crates/control`) | deploy, billing/Stripe, env, route registry | ✅ **1 CRITICAL** (fee unenforced), 1 HIGH (plan self-escalate), 3 MAJOR; IDOR/bundle-ingest solid |
| 5 | **worker** (`crates/worker`) | bundle loading, isolate lifecycle/eviction | ✅ 2 MAJOR (identity not app-bound, redeploy no-cleanup); bundle-integrity/cache-keying solid |
| 6 | **bundle** (`crates/bundle`) | .zship artifact: unpack/path-traversal, blob store | ✅ unpack/blob hardened (covered by control lane A) |
| 7 | **compio-postgres / compio-redis** | bespoke wire-protocol drivers | ✅ redis 2 HIGH (slot panic, SSRF redirect); pg close; no-TLS MEDIUM |
| 8 | **sandbox** (`crates/sandbox`) | Nomad + Cloud Hypervisor VM isolation | ✅ **2 CRITICAL** (infra: guest L3 egress, unauth Nomad), Rust code solid |
| 9 | **core** (`crates/core`) | typed_id, signing utils, wire types | ✅ STRONG (no findings above MINOR) |

Legend: ⏳ queued · 🔄 reviewing · ✅ findings recorded — **ALL 9 MODULES COMPLETE**

---

## 1. Gateway — findings ✅

4 lanes: routing/dispatch (A), proxy/SSRF/assets (B), enforce/rate-limit/RLS/idempotency (C),
token-signing integrity (D). Auth endpoints excluded (covered by the auth-pipeline review).

**Headline:** the identity-signing crypto (D) is **strong** and there is **no SSRF, no asset
path-traversal, no request smuggling, no cross-tenant cache bleed, no ReDoS** — the trusted-
registry upstream selection, content-hash asset/cache keying, and HMAC `ZeroShip-User` mint/
verify are all sound. The real exposure is **host-trust + rate-limit robustness**.

| ID | Sev | Lane | Finding | Status |
| --- | --- | --- | --- | --- |
| GW-1 | HIGH | A | **No apex allowlist in Host→app resolution** — `extract_app_name` (`dispatch.rs:76`) takes the first Host label with zero check that the parent is `zeroship.ai`. `Host: myapp.evil.example` → tenant `myapp`. Host-derived trust (OIDC `redirect_uri` `dispatch.rs:1601`, DPoP `htu` `auth.rs:681`, CSRF origin `auth.rs:454`) is seeded by attacker input; phishing-grade host spoofing. (HIGH not CRIT: cookie `app`-claim + `__Host-` + Hydra redirect allowlist blunt direct session theft.) | **verified** (src read) |
| GW-2 | HIGH | A | **`/apps/{app_name}/…` path route decouples tenant from origin** — `main.rs:723` + `dispatch.rs:45` path-name wins unconditionally, no Host constraint. `myapp.zeroship.ai/apps/otherapp/…` dispatches `otherapp` on `myapp`'s origin. Origin/UI confusion; CSRF `expected_origin` no longer matches the mutated app. Bind to an internal listener or apply host-coherence checks. | **verified** (src read) |
| GW-3 | HIGH | C | **X-Forwarded-For spoof under `trust_proxy=true`** — ntex `connection_info().remote()` takes the leftmost (client-supplied) XFF token (`dispatch.rs:157`). Rotate XFF → unlimited per-IP buckets → full per-IP rate-limit evasion. Gated behind `trust_proxy` (default false), but that's the prod-behind-LB case. Parse XFF right-to-left by trusted-hop count. | reviewer-evidenced (ntex behavior) |
| GW-4 | HIGH | C | **Unbounded rate-limiter maps** — `enforce.rs:73,151,249` registries are plain `HashMap`s, no eviction/cap/TTL (grep-confirmed). Per-rule `Ip`/`Session` keys embed attacker-controlled strings → key-cardinality memory-exhaustion DoS, cross-tenant (one shared map). Idempotency has `MAX_LIVE_KEYS_PER_APP`; the limiter has no equivalent. Bound with LRU/TTL eviction. | reviewer-evidenced (grep) |
| GW-5 | MAJOR | B | **No idle-timeout on the chunked streaming read loop** — `proxy.rs:321,648` read upstream chunks in a bare loop; only header + buffered-body reads are timeout-wrapped. A slow/stalled upstream (or hung app SSE) pins the detached task + socket indefinitely → slowloris/connection-pinning. Wrap each streaming `read` in an idle deadline. | reviewer-evidenced |
| GW-6 | MEDIUM | B | `content-length` not stripped on the worker→client chunked path (`proxy.rs:305`) — CL+TE framing desync (smuggling-adjacent); the transparent `forward_http` path strips it correctly. | reviewer-evidenced |
| GW-7 | MEDIUM | C | Global per-app rate-limit + concurrency guard run **after** full auth resolution (DB round-trips); unauthenticated flood forces auth/DB work before any global limiter. Add a cheap pre-auth IP/global bucket at the top. | reviewer-evidenced |
| GW-8 | MEDIUM | C | **Static/Redirect/Rewrite actions skip the global rate-limit** entirely (`dispatch.rs:664`; only WorkerRpc/WorkerSsr reach `handle_dispatch`). Asset/redirect hammering is unmetered. Move the global limiter up to `execute_resource_tree`. | reviewer-evidenced |
| GW-9 | MEDIUM | C | Idempotency **at-least-once on worker crash** — a crash between lock-acquire and response-store lets a retry re-acquire the lock after `LOCK_TTL_SECS=30` and re-execute the (e.g. billing.charge) mutation. Persist an in-flight marker. | reviewer-evidenced |
| GW-10 | MEDIUM | A | Inbound `X-Forwarded-*`/`Forwarded`/`X-Real-IP` are **not** stripped before forwarding to the worker (`dispatch.rs:1153`); app JS reading them gets attacker-controlled source IPs. No authoritative client-IP header is injected either. Strip them + inject `x-zs-client-ip`. | reviewer-evidenced |
| GW-11 | MEDIUM | A | Host/subdomain match is **case-sensitive** (`dispatch.rs:76`) while DNS is not, and app names are stored case-preserved (`registry.rs:148`); `MyApp` vs `myapp` could be distinct tenants reachable by Host case. (The auth-host check *does* lowercase — the inconsistency is the tell.) Lowercase the label + enforce lowercase names. | reviewer-evidenced |
| GW-12 | LOW | B | Unbounded buffered-body read (no `MAX_BUFFERED_BODY`, `proxy.rs:484`) + chunk-size parse has no upper bound and malformed→`Incomplete` spins (`proxy.rs:544`). Workers semi-trusted, so hardening. | reviewer-evidenced |
| GW-13 | LOW | D | `oidc_rp.rs:1156` `serde_json::to_string(user).unwrap_or_default()` would HMAC-sign an **empty** identity on a serialize failure (not currently reachable) — a fail-*open* default on the identity mint. Propagate the error. | reviewer-evidenced |
| GW-14 | LOW | A/B | `route_auth_host` forwards the full inbound header set to Hydra/auth without the reserved-header scrub (`dispatch.rs:348`); `is_auth_host` prefix match is broad (`auth.zeroship.evil` classifies as auth host, `dispatch.rs:345`). Upstream-trust-dependent. | reviewer-evidenced |
| — | LOW | D | `ZeroShip-User` header carries no revocation bind beyond `iat`≤60s — a captured header is replayable ≤60s post-revocation against the same gateway-generated v4 request_id (near-impossible). By design; noted. | reviewer-evidenced |

**Solid (verified):** signing/verify (HMAC over unambiguous encoding, full-field MAC, request-id binding enforced on the worker, constant-time compare, fail-closed parse, HMAC-vs-EdDSA domain separation, ≥32B key floor); RLS (`set_config` parameterized + transaction-local + FORCE RLS + fails closed); idempotency per-`(app,wire,key)` namespacing; no SSRF (trusted-registry upstreams); no asset traversal (content-hash keys); no smuggling (validated HeaderValues, CL recomputed, hop-by-hop stripped on forward); authenticated route-registry sync with passthrough fallback; worker-dispatch reserved-header scrub correct + tested.

**Top fixes for return:** GW-1/GW-2 (host-trust — apex allowlist + constrain `/apps`), GW-3/GW-4 (rate-limit IP source + unbounded maps), GW-5 (streaming idle-timeout).

---

## 2. Runtime — findings ✅

4 lanes: V8 sandbox/isolate-integrity (1), fetch/SSRF (2), resource-limits/DoS (3),
crypto/node-compat (4). **The highest-stakes module — 4 CRITICAL + 2 MAJOR.**

| ID | Sev | Lane | Finding | Status |
| --- | --- | --- | --- | --- |
| RT-1 | **CRITICAL** | 1 | **V8 isolate escape via fastcall null signature.** `#[v8_class]` builds fastcall templates with no `v8::Signature` (`runtime-macros/.../emit/install.rs:368`, no `.signature()`); the fast shim does `get_aligned_pointer_from_internal_field(1,0)` → `&*(raw as *const Self)` with no brand check / no internal-field-count guard (`fastcall/mod.rs:455`). A foreign/forged receiver (`m.call({})`, `Object.create(C.prototype)`, another native instance) reaches the fast path under JIT and is reinterpreted as `*const Self` → type confusion → arbitrary-relative memory read (and write for any `&mut self` fastcall). `Headers.has` is fastcall + reachable by all app JS. Reviewer reproduced SIGABRT. Fix: `.signature(v8::Signature::new(scope, ctor_tmpl))` + `internal_field_count()>=2` guard + foreign-receiver regression test. | **verified** (src) |
| RT-5 | **CRITICAL** | 3 | **Sync CPU loop wedges the shared worker thread when `cpu_limit` is `None`.** The POSIX CPU timer — the only interrupt for synchronous JS — is armed only `if cpu_limit.is_some()` (`core/runtime.rs:1322`); the wall-timeout fires only on the async `Pending` arm a sync loop never reaches. `unlimited`/`enterprise` plans set `cpu_limit_ms: None` (`control/registry.rs:531`). `while(true){}` (or a microtask flood) permanently starves every co-tenant isolate on that thread. Fix: always arm a watchdog (`None` = platform ceiling, not "off"); isolate truly-trusted apps onto dedicated threads. | reviewer-evidenced |
| RT-6 | **CRITICAL** | 3 | **Off-heap memory escapes the V8 heap cap → whole-worker OOM.** No `adjust_amount_of_external_allocated_memory` anywhere; backing stores (`new_backing_store_from_vec`, `Buffer.alloc(Unsafe)`) and retained fetch bodies allocate Rust `Vec`s invisible to `heap_limit`/`near_heap_limit`. `while(true) a.push(Buffer.allocUnsafe(64MB))` grows RSS until the kernel OOM-kills the worker + all co-tenants. Fix: account external bytes against the heap budget (or a per-isolate external cap). | reviewer-evidenced |
| RT-7 | **CRITICAL** | 3 | **LRU eviction leaks the isolate + native resources.** The pump is spawned holding a **strong** `Rc<RefCell<RuntimeInner>>` (`core/runtime.rs:919`); `evict_lru` (`worker/cache.rs:275`) only drops the cache handle, so the self-sustaining pump cycle keeps `RuntimeInner` alive — `Drop` never runs, leaking the isolate, DB connections, sockets, file handles. App churn → unbounded leak → OOM despite eviction. (Idle-GC ticker correctly uses `Weak` — the pump must too.) Fix: pump holds `Weak`+`upgrade()` per loop; eviction signals pump shutdown. | reviewer-evidenced |
| RT-2 | **MAJOR** | 2 | **Fail-open `ZEROSHIP_DEV` SSRF bypass.** `transport/ssrf.rs:89/144/162` + `client.rs:26` gate the dev bypass on `env::var("ZEROSHIP_DEV").is_ok()` — true for ANY value (`0`, ``, `false`). Unlike `ZEROSHIP_DEV_INSECURE`, the worker/gateway never scrub it. If the var leaks into a prod worker, the entire SSRF guard is disabled → `fetch('http://169.254.169.254/...')` (cloud creds) + `fetch('http://localhost:9090/...')` (control plane) work for every tenant. Fix: gate on `== "1"` checked once at boot; hard-clear it unless `--dev-insecure`. | reviewer-evidenced |
| RT-3 | **MAJOR** | 2 | **No fetch timeout; `wall_timeout` stored but never enforced.** cyper client built with no connect/read/total timeout (`client.rs:22`); `wall_timeout` (`runtime.rs:616`) is never read in the pump (only CPU is). A `fetch()` on a hung/blackholed socket consumes ~0 CPU so no limit fires; 64 (`MAX_PENDING_FETCHES`) hung fetches + connect-timing oracle = slow-DoS + internal/arbitrary-host port scanner. Fix: explicit cyper timeouts + enforce `wall_timeout` by flipping the request cancel flag. | reviewer-evidenced |
| RT-4 | MEDIUM | 1 | **`__zs_env` is a permanent global** (`core/init.rs:1784`) returning the full `env.{db,auth,kv,storage}` to user-module top-level — never deleted (unlike `__zsDbPlatform`). Same class as plugin-db DB-5, generalized to every namespace; own-app scope (capability-timing / over-broad authority, not cross-app). Fix: delete after bootstrap captures it, or gate to require an active request context. | reviewer-evidenced |
| RT-8 | MEDIUM | 3 | **CPU-watchdog map never unregisters + keyed by reusable raw pointer.** `CpuTimerSystem::unregister` is `#[allow(dead_code)]`/never called; the watchdog map is keyed by `addr_of!(self.isolate)` (`runtime.rs:1327`). Unbounded growth; once RT-7 is fixed, a reused address → a stale timer could `terminate_execution()` an unrelated live app. Fix: unregister in `Drop`, key by app `Uuid`. | reviewer-evidenced |
| RT-9 | MEDIUM | 3 | Pump-CPU budget has a **10s grace window** (`record_pump_cpu` `runtime.rs:2506`) — pump-side work (timer/microtask chains, `setInterval(fn,0)` via the `ready_timers` fast-path) can pin the thread ~100% for 10s before the first check; the only pump-side backstop. Tighten window + add a burst check. | reviewer-evidenced |
| RT-10 | LOW | 1/2/4 | `__zs_env` aside: outbound fetch has **no forbidden-header filter** (`Host`/`CL`/`TE`/`Cookie` forwarded — `headers.rs:64` deferred guards) → intermediary/vhost confusion (CRLF still blocked); `data:` URL has no decoded-size cap; `setInterval(fn,0)` spin; `randomInt` over-rejects (no bias); SHA-1 exposed (legit). | reviewer-evidenced |

**Solid (verified):** **node-compat sandbox is excellent** — no `fs`/`net`/`dgram`/`child_process` bindings, `process.env` allowlist-gated (no host-secret leak; `std::env::vars()` leak already removed), real `aws_lc_rs` CSPRNG, WebCrypto extractable/usage enforced, node-crypto cipher allowlist (no ECB, keyless disabled, IV lengths, constant-time `timingSafeEqual`), `os` stubbed, module resolver closed. **SSRF posture strong** — IP-pinned connect (no rebind TOCTOU, verified vs cyper), per-hop redirect re-validation, scheme allowlist, IP-encoding canonicalized, metadata/CGNAT/loopback/RFC1918 blocked, CRLF-safe. **Slow-path native recovery is brand-checked** (`recover_box.rs`: brand → External → re-entry guard; `get_internal_field` bounds-checks, so `Object.create(proto)` throws not OOB). **Per-request auth identity correctly request-id-scoped** (no cross-request/isolate bleed). Heap cap, frame/buffer caps, admission controls (timers/ops/fetches), WebSocket reassembly bounds all real.

**Top fixes for return:** RT-1 (isolate escape — one-line signature + guard + test), RT-5/6/7 (shared-thread containment), RT-2 (SSRF fail-open env gate).

---

## 3. plugin-kv + plugin-storage — findings ✅

Sibling native primitives, reviewed with the plugin-db lens.

### plugin-kv — **STRONG** (no CRITICAL; better than the plugin-db baseline)
`app_id` un-spoofable (server-stamped, never a JS arg); keys `{app_id}:`-namespaced
with `{`/`}`/control chars banned → **hash-tag isolation correct under cluster routing**;
commands are RESP arrays (**no injection**); SCAN patterns glob-escaped; TTL overflow
closed + tested; DSN credentials redacted from errors; `list` cursor opaque + tenant-scoped.

| ID | Sev | Finding |
| --- | --- | --- |
| KV-1 | MAJOR | **No per-app key-count / byte quota** — per-item caps exist (key 512B, value 256KiB) but nothing bounds total keys/bytes. On shared prod Redis, one tenant can exhaust `maxmemory` → cross-tenant key eviction or `OOM`-failed writes for every colocated tenant (all of an app's keys share a hash-tag = one shard). Needs a real per-app quota (control-plane `AppRuntimeLimits` + atomic counter). |
| KV-2 | MAJOR | **`kv.list("")` unbounded blocking scan** — empty prefix walks up to 10k entries **synchronously on the event-loop thread** (redb path; module doc admits no offload). Self-inflicted/abusive latency DoS on the worker. Offload the scan; lower `LIST_MAX_LIMIT`. |
| KV-3 | MINOR | incr overflow vs non-numeric conflated on the Redis path (substring-sniff); `from_utf8_lossy` silent corruption on `get`; `now_ms` swallows clock error; URL parsed per-op. |

### plugin-storage — path-validator gaps + zero resource limits (S3 backend not yet implemented)
`app_id` un-spoofable + prefix-rooted (`<root>/<app_id>/<bucket>/…`); `..` segments rejected;
`list` prefix can't traverse (starts_with, not path-join); key segments pushed individually.

| ID | Sev | Finding | Status |
| --- | --- | --- | --- |
| ST-1 | HIGH | **Incomplete central path validator** — `validate_object_coords` (`backend/mod.rs:94`) rejects `/` and `..` but NOT NUL, backslash, newline/control chars, or dotfile segments. The stated "one validator makes every backend safe" boundary is leaky. **Not an app-JS cross-tenant escape on LocalFs today** (`..` caught, `app_id` a trusted UUID, interior-NUL paths fail at the fs syscall) — but a latent CRITICAL the moment the S3 backend lands (different key semantics) or user uploads are served. Switch to a strict allowlist; apply to `app_id`/`bucket` too. | **verified** (src) |
| ST-2 | HIGH | **No object-size limit** — `put` base64-decodes the full input then `to_vec()`s again (2× in RAM, `callbacks.rs:101`/`local.rs:65`), no cap → app-JS-reachable **co-tenant OOM** on the shared worker thread (compounds RT-6 off-heap-memory). Cap base64 length before decode + per-app storage quota. | reviewer-evidenced |
| ST-3 | MEDIUM | `get` triple-buffers the whole object (read→base64→JSON), no range/streaming → memory spike; `list` is a full recursive walk with **no pagination/cap** (`local.rs:131`) → DoS. | reviewer-evidenced |
| ST-4 | MEDIUM | **`list` re-implements a weaker, divergent validator** (`local.rs:121`) instead of the central one — the exact drift the centralization was meant to prevent; inherits ST-1's gaps. Add `validate_list_coords` as the single source. | reviewer-evidenced |
| ST-5 | MEDIUM | **`get_app_id` falls back to `"default"`** when `APP_ID` is absent (`callbacks.rs:51`) — a fail-*open* shared tenant. Not reachable on the real worker path (APP_ID always stamped) but the kernel should hard-fail, not invent a shared tenant. | reviewer-evidenced |
| ST-6 | LOW | `walk` follows symlinks (`metadata()` not `symlink_metadata`, `local.rs:155`) — not app-creatable today; `content_type` silently discarded (security-relevant once uploads are served); storage-root vs bundle-store disjointness not asserted (N4); back-compat alias contradicts pre-launch stance. | reviewer-evidenced |

**Note:** the **S3/R2 backend does not exist yet** (the `s3` feature gates nothing) — the S3-specific threats (leading-`/`, `..`, presigned URLs) are N/A and must be re-reviewed when it lands, with ST-1 fixed *first* so S3 inherits a correct validator.

**Top fixes for return:** ST-1 (strict path allowlist before S3), ST-2/KV-1 (resource quotas), KV-2/ST-3 (unbounded blocking scans).

---

## 4. Control plane — findings ✅

3 lanes: deploy/CRUD-authz (A), billing/Stripe (B), env/secrets (C). The Cedar authz +
`app_members` ownership were covered by the auth-pipeline review; these lanes confirm the
**ownership/IDOR model is consistently sound** and focus on input-validation + the money path.

| ID | Sev | Lane | Finding | Status |
| --- | --- | --- | --- | --- |
| CT-B1 | **CRITICAL** | B | **15% platform fee not enforced server-side.** Checkout session built in creator code (`@zeroship/payments`); `applicationFeePercent` caller-settable; `record_payout` records `application_fee_amount` + self-attested `creator_id` verbatim, only checking `fee≤gross` — no fee floor, no `creator_id`↔connected-account binding (`stripe_store.rs:226`). Creator sets `applicationFeePercent:0` → keeps 100%. Platform must own session creation or enforce a server-side floor + account binding. | **verified** (src) |
| CT-A1 | HIGH | A | **Creator self-escalates to `unlimited` runtime/billing tier via unvalidated `plan_id`.** `set_plan`/`create_app` (`api.rs:563,85`) pass `body.plan_id` to a raw `UPDATE` with no allowlist; the owner Cedar policy grants `billing:write` on their own app, and `runtime_limits_for_plan("unlimited")` returns `cpu/wall/heap = None` (`registry.rs:519`). A free-tier creator removes their own guardrails — **chains into RT-5** (cpu_limit None → wedge the shared worker thread). Validate `plan_id` against a known set; gate tier raises behind admin/verified-billing, derive limits server-side. | reviewer-evidenced |
| CT-C1 | MAJOR | C | **No reserved/platform env-name denylist** — `valid_key` accepts any `[A-Z][A-Z0-9_]{0,63}`; a creator can set `DATABASE_URL`/`WORKER_KEY`/`CONTROL_KEY`/`NODE_OPTIONS`/`PATH` and `merged_env_for_worker` injects them into their isolate (`env_store.rs:67,469`). Vars are always in `process.env`; severity = whatever the runtime/node-compat trusts. Add a reserved-name denylist at write time. | reviewer-evidenced |
| CT-B2 | MAJOR | B | **Metering spoofable per-tenant + non-idempotent.** `report_usage` (`internal.rs:152`) keys counters by `app_id` from the request body, authed only by the shared control-key — any control-key holder reports usage for ANY app; `record_usage` is a blind `value+=delta` (no event-id/dedupe → replay double-counts). **Feeds no billing path today** (metering stub), so currently a stats-integrity/DoS issue; money-critical once usage drives invoicing. Scope to the worker's own apps + add idempotency. | reviewer-evidenced |
| CT-B3 | MAJOR | B | **`insecure_dev` disables BOTH webhook signature verification AND `/internal/*` auth** (`stripe_handlers.rs:404`, `internal.rs:17`) — a single boolean between dev convenience and full compromise (forged `invoice.paid`; unauth decrypted-secret read). Hard-refuse to start `insecure_dev` on a non-loopback bind. | reviewer-evidenced |
| CT-A2 | MEDIUM | A | **Deploy endpoint has no rate-limit / concurrency cap** (unlike env handlers) — each call streams ≤256MB to tmp + CPU-heavy zstd decode; N concurrent deploys = N×256MB tmp + parallel decompression → control-plane disk/CPU DoS by one creator. Add `admin_rate_limit` + in-flight semaphore + tmp-byte cap. | reviewer-evidenced |
| CT-A3 | MEDIUM | A | **Case-variant app names collide** — `create_app` accepts mixed case + the DB `UNIQUE` is case-sensitive, but DNS/Host is case-insensitive and the gateway indexes the raw-case label (cross-ref GW-11). `Foo` and `foo` are distinct apps owned by different tenants reachable by Host casing → cross-tenant routing capture. Store/compare lowercase (`citext`/`UNIQUE(lower(name))`). | reviewer-evidenced |
| CT-A4 | MEDIUM | A | **No reserved-name denylist on `create_app`** — creators can claim `console`/`api`/`www`/`admin`/`internal`/`static` → `{name}.zeroship.ai` (only `auth.` is special-cased). Future platform hosts silently squattable + phishing (cross-ref GW-1 host-trust). | reviewer-evidenced |
| CT-C2 | MINOR | C | **Single platform-wide secret key** — `derive_key = SHA256("…v1"‖master)`, same key for every app's secrets (AAD scopes the row, but a master/`primary_key` compromise decrypts the whole fleet). Per-app HKDF (`info=app_id`) already roadmapped — prioritize. | reviewer-evidenced |
| CT-B4 / CT-misc | MINOR | B/A/C | currency defaulted to `usd` + cross-currency aggregation in `total_earnings` (wrong figures); Connect `callback` (billing-staff only) doesn't verify the `acct_` is owned by the creator before linking payouts; orphan blobs on deploy-vs-delete race; unbounded per-app env-var count; rate-limit fail-open on missing source IP; dead `merged_env` plaintext path. | reviewer-evidenced |

**Solid (verified):** **per-app IDOR consistently closed** — every CRUD/deploy/env endpoint gates `authz.require(action, App{id})` keyed on the authenticated principal via DB-backed Cedar memberships; `list_apps` ownership-scoped; no mass-assignment (owner bound from `principal_id`, columns hardcoded) — *except* the `plan_id` field (CT-A1). **Bundle ingest hardened** — tar entries must be exactly `manifest.json` or `blobs/<64-hex-sha256>` (no `../`), decompression `take`-capped + per-blob/count/manifest caps, blob hash verified during write (no cross-app/platform blob overwrite, content-addressed). **Webhook crypto solid** — HMAC `t=,v1=` parse, ±300s replay tolerance, constant-time compare, rotation, CPU-amplification cap, size caps, ledger idempotency (`event_id` UNIQUE + `payload_hash` tamper check). **Env/secrets solid** — write-only secret API, AAD binds `(app_id,key_name)`, random nonces, control-key-gated worker delivery (constant-time), generic error bodies, key zeroization. Internal sync lane constant-time auth; no master-key→creator-endpoint bypass.

**Top fixes for return:** CT-B1 (enforce the platform fee server-side), CT-A1 (validate `plan_id` — also closes a path to RT-5), CT-C1 (reserved env-name denylist), CT-B3 (`insecure_dev` blast radius).

---

## 5. Worker + bundle — findings ✅

| ID | Sev | Finding | Status |
| --- | --- | --- | --- |
| W-1 | MAJOR | **`ZeroShip-User` identity header not bound to `app_id`.** Signed payload is `{user_json}.{request_id}.{issued_at}` (`core/auth/mod.rs:263`); the worker validates it independently of the dispatch path app_id (`handler.rs:48,163`) and never cross-checks. A header minted for app A can be replayed to `/dispatch/{app_B}` within the 60s window with the same `request_id` → app A's authenticated user injected into app B's isolate, defeating pairwise-subject isolation. Defense-in-depth at the gateway↔worker boundary (needs `worker_key` to reach `/dispatch`, so gated on a breach/misroute there) — **refines the gateway "signing strong" verdict.** Fix: add `app_id` to the MAC (gateway signer + worker verifier, one patch). | reviewer-evidenced |
| W-2 | MAJOR | **Redeploy isolate swap drops in-flight work with no abort fan-out.** LRU eviction fires every in-flight `AbortController` (`cache.rs:287`); the redeploy path (`cache.rs:165`, `sync.rs:258`) just drops the old `Runtime` — pending `fetch()`/DB queries/SSE streams abandoned, JS abort handlers + tx rollbacks never run. Redeploy is the common case. (Sibling to RT-7's pump leak.) Fix: factor the eviction abort fan-out + call it before every `isolates.remove`. | reviewer-evidenced |
| W-3 | MINOR | Secrets/env never zeroized on eviction/rotation — old `Arc<CachedEnv>` plaintext (DB passwords, API keys) lives in worker heap until last reader drops, no scrub; recoverable via core dump/heap scan. Wrap in `Zeroizing`/`secrecy`. | reviewer-evidenced |
| W-4/W-5 | MINOR | Reconcile jitter seed is a stack pointer (ASLR leak + doesn't de-correlate threads as claimed); request body modeled as `String` via `from_utf8_lossy` (`dispatch.rs:1219`) → **binary bodies corrupted before the app sees them**, breaking app-side signature verification / content hashing. Model the body as bytes/base64. | reviewer-evidenced |

**Solid (verified):** **bundle-load integrity enforced** — `BlobStore::get_blob` re-computes `sha256` and rejects mismatch before instantiation (no poisoned-bundle run); **cache keying is the trusted path-UUID** (no `X-App-Id` path exists in the worker); concurrent same-app requests are V8-serialized (`!Send` `Rc<RefCell>`, no `.await` mid-call); control→worker channel constant-time `worker_key`-authenticated + non-loopback startup guard fails closed; redeploy commits env *before* the V8 swap (no new-code/old-env window).

### 6. Bundle (`crates/bundle`) — **covered by control lane A (✅)**
`.zship` unpack is hardened: tar entries must be exactly `manifest.json` or `blobs/<64-hex-sha256>` (no `../` traversal), decompression `take`-capped + per-blob/count/manifest size caps, blob hash verified during streaming write (content-addressed → no cross-app/platform blob overwrite). No additional findings.

## 7. compio-postgres / compio-redis drivers — findings ✅

Both delegate byte-level wire parsing to mature crates (`postgres-protocol`, `redis-protocol` v6) — so length-field/null handling is covered. Findings are in the **bespoke framing/pool/cluster glue**.

| ID | Sev | Driver | Finding | Status |
| --- | --- | --- | --- | --- |
| DR-1 | HIGH | redis | **Server-triggerable panic** — `set_slot` does `slots[slot as usize]` unchecked (`cluster.rs:489`); `slots.len()=16384` but `parse_redirect` accepts the full `u16`. A `-MOVED 60000 ip:port` reply → OOB index → **panic crashing the shared worker thread**. (`parse_cluster_slots` bounds-checks; the redirect path doesn't.) Gated on backend compromise/MITM. Fix: reject `slot>=16384`. | reviewer-evidenced |
| DR-2 | HIGH | redis | **SSRF-via-cluster-redirect** — MOVED/ASK `host:port` is connected to verbatim with no allowlist (`cluster.rs:472,537`), replaying the authenticated command + re-attaching the Redis password to an attacker-named IP:port. (IP-literal only, no DNS — so internal IPs, not arbitrary DNS.) Gated on backend compromise/MITM. Fix: restrict redirects to nodes learned from `CLUSTER SLOTS`. | reviewer-evidenced |
| DR-3 | MEDIUM | redis | **Pool returns errored connections un-poisoned** (`pool.rs:160`) — no `send_recv` error path calls `poison()`; a timed-out/mid-frame connection with leftover `rx` bytes, on next checkout, decodes them as a *different* command's response → **cross-request response desync** (tenant-isolation concern in multi-tenant KV). Fix: poison on any I/O/timeout/protocol error. | reviewer-evidenced |
| DR-4 | MEDIUM | redis | No max-frame cap on the read buffer (`client.rs:328`) — `$2147483647` dribble grows `rx` toward 2GB (PG has a 64MB cap; redis doesn't). Fix: mirror the PG ceiling. | reviewer-evidenced |
| DR-5 | MEDIUM | pg | Panic on a DataRow with fewer fields than RowDescription (`row.rs:196` indexes `ranges` validated only against the column list) — inherited from tokio-postgres, platform-backend-gated. Bounds-check `idx` vs `ranges.len()`. | reviewer-evidenced |
| DR-6 | MEDIUM | both | **No TLS in practice** — every PG consumer passes `NoTls` (`plugin-db/lib.rs:467,694`); redis has no TLS path at all → all DB/Redis traffic is plaintext. MITM reads/alters everything + enables DR-1/DR-2's MITM variants. Safe ONLY if PG/Redis are strictly private-network/loopback — make that explicit + enforced. (No `accept_invalid_certs` footgun — but only because verify code is never reached.) | reviewer-evidenced |
| DR-7 | LOW | pg | No portable per-query read timeout (only Linux `tcp_user_timeout` if configured) → a never-finishing response parks a pool slot (slow-loris). PG prepared-stmt/`search_path` reuse across multi-tenant pool checkout was NOT audited this pass — flagged for a dedicated look. | reviewer-evidenced |

**Solid (verified):** PG framing has an explicit 64MB cap checked *before* buffering (no `with_capacity(attacker_len)` OOM), `checked_add`+`get()` offset math, SCRAM delegated to `postgres-protocol` with channel-binding-downgrade rejected + `sslmode=prefer+direct` refused, no plaintext-password logging. Both parser cores are mature crates (injection-safe binding already confirmed in plugin-db/kv lanes). **Verdict: compio-postgres close to production-ready; compio-redis not (bespoke cluster layer needs the DR-1..4 fixes).**

---

## 8. Sandbox (Nomad + Cloud Hypervisor micro-VM backend) — findings ✅

The nomad-ch micro-VM backend is an **alternative** isolation model to the primary V8-per-thread
worker. The **Rust code is solid** — no spec/shell injection (job spec built with `serde_json::json!`,
not string templating; app-influenced fields hard-validated as base62 typed-UUIDs *before* any
host-path join → path traversal blocked), driver binary SHA-256-pinned, agent RPC Ed25519-signed
(per-sandbox key, private half never leaves the controller), secrets written 0400/root/no-env. The
breaks are in the **GCP provisioning scripts** (deployment config) + missing artifact pinning. The
out-of-tree `nomad-driver-ch` Go plugin (CH privilege/seccomp/jailer model) is **not in this repo** —
unauditable here.

| ID | Sev | Finding | Status |
| --- | --- | --- | --- |
| SB-A1 | **CRITICAL** (infra) | **Guest VM has unfiltered L3 egress to the host internal network + cloud metadata.** Host confinement is just `ip_forward=1` + a per-VM `/30` tap — **zero iptables/nftables egress rules** (`gcp-worker-startup.sh:261`). A workload in the VM reaches `169.254.169.254` (GCP metadata → instance SA token, `sandbox-token`/`admin-token`/`pg-password` attributes) and the VPC (Nomad :4646, Postgres :5432, controller, other agents). **The VM path is *less* contained than the V8 `fetch` SSRF guard it was meant to harden.** Fix: default-DROP FORWARD from `10.99/16`; allow only public-internet egress (drop 169.254/16, RFC1918, host IPs). | reviewer-evidenced |
| SB-A2 | **CRITICAL** (infra) | **Nomad API unauthenticated (no ACLs), bound `0.0.0.0:4646`**, firewall-open to the VPC (`gcp-worker-startup.sh:334`, `provision-gcp-cluster.sh:144`). Chained with SB-A1, a guest → full control-plane takeover of the sandbox fleet: submit arbitrary `ch` jobs (read other tenants' `home.img`, hijack a tap index, arbitrary kernel/resources), `alloc exec`/stop other allocs, enumerate tenants via job `Meta`. Bypasses ALL the Rust-side validation. Fix: enable Nomad ACLs (default-deny anon), bind to the private IP, controller uses a scoped token. | reviewer-evidenced |
| SB-A3 | HIGH (infra) | Postgres `pg_hba` trusts the entire `10.0.0.0/8` with `md5` password, and `pg-password` is in guest-reachable VM metadata → a guest escape reads it and gets full DB access (every tenant's data). Scope `pg_hba` to the controller IP, use `scram-sha-256`, source the password host-only. | reviewer-evidenced |
| SB-A4 | HIGH | `cloud-hypervisor`, `vmlinuz`, and the rootfs image are **NOT hash-verified before boot** (`gcp-worker-startup.sh:153`) — only the driver binary is SHA-pinned. The kernel + rootfs *define* guest isolation; a poisoned bucket artifact boots silently. Extend the FATAL-on-mismatch SHA gate to all three. | reviewer-evidenced |
| SB-1 | MAJOR | **Preview-share HMAC secret has an insecure `/dev/urandom`-failure fallback** to `time^pid` material (`registry.rs:126`) that silently keys real preview-share tokens → brute-forceable → forged `rw` share links. (The Ed25519 paths correctly hard-fail on RNG failure; this one doesn't.) Make it `Result`/fail-closed. | reviewer-evidenced |
| SB-A6 | MEDIUM | `user.blacklist = ""` clears Nomad's root-task guard → CH runs as **root** with no visible jailer/seccomp (the out-of-tree driver's posture is unverifiable) — a CH VM-escape CVE lands as host root with no second containment layer. Run CH unprivileged under a jailer-equivalent. | reviewer-evidenced |
| SB-misc | LOW/MED | admin bearer has no length floor (vs ≥32B elsewhere); `clock_resync` skew-bypass replay relies on the agent challenge-LRU (residual); `chmod 0666 /dev/kvm`; tenant ids in clear Nomad `Meta` (enumeration oracle with SB-A2); stale `nomad-vm-wrapper.sh` test reference (the script is deleted — no injection surface, good). | reviewer-evidenced |

**Solid (verified):** Rust job-spec construction injection-safe + typed-ID-validated before path joins (no traversal/arg/spec injection); driver binary SHA-pinned FATAL-on-mismatch; per-sandbox Ed25519 agent auth (private key controller-only, `verify_strict`, domain tags, nonce-after-verify); `SANDBOX_TOKEN` ≥32B floor + redaction + constant-time; admin Full/RO distinct-token boot guard; `nomad_addr` loopback-only; secrets 0400/root/no-env; per-VM `/30` tap (L2 cross-tenant isolation holds — the break is L3).

## 9. Core (`crates/core`) — findings ✅ — **STRONG, no findings above MINOR**
`typed_id` is overflow-safe (checked base62 decode, no panics on attacker input) and **never used as an auth secret** (session/preview/agent auth all use proper HMAC/Ed25519); `config/secrets.rs` enforces uniform ≥32B strength floors, rejects dev sentinels + plaintext-literal secrets outside `--dev-insecure`, `is_loopback_url` is literal-only (no DNS rebind), fails closed (`exit(1)`) on resolution failure; wire types don't leak (`AppRecord.api_key` `skip_serializing`, `RouteEntry` carries `api_key_hash` not the secret); observability emits no secrets/identities. The `ZeroShip-User` mint/verify (gateway lane D) is strong. **MINOR:** `typed_id` prefix validation is opt-in (`parse()` vs `parse_with_prefix()`) — a future caller using bare `parse()` could reintroduce prefix-confusion; the security boundary already uses the hardened form.

---

# Pass 2 — deeper review (gaps · red-team · SDK/RPC · request-traces)

Pass 1 covered the 9 crates module-by-module. Pass 2 targets what pass 1 deferred or didn't reach,
and adversarially re-verifies the pass-1 CRITICALs.

| Lane | Scope | Status |
| --- | --- | --- |
| P2-A | **Red-team** + runtime gaps | ✅ RT-1 reproduced+fix-verified, CT-B1 confirmed; 2 new MAJOR; module-loader refuted |
| P2-B | **SDK / dispatcher / RPC** | ✅ 4 HIGH (proto-lookup, no-input-cap, discovery-divergence, build-RCE) |
| P2-C | **PG pool cross-tenant state** | ✅ **HIGH (P2-C1)** — see below |
| P2-D | **Request-tracing** | ✅ MAJOR (identity not app-bound); most seams TIGHT |

### Pass-2 findings (lanes B/C/D recorded; A red-team pending)

| ID | Sev | Lane | Finding | Status |
| --- | --- | --- | --- | --- |
| P2-C1 | HIGH | C | **PG pool leaks per-app session state (`SET ROLE` + timeouts) across tenants on the autocommit path.** `apply_autocommit_role` error returns via `?` with no reset/close (exec.rs:183), and `reset_autocommit_role` is a plain `.await` *after* the query with **no RAII drop-guard** — so a cancelled future (CPU-kill/request-cancel, which the runtime does) skips it (exec.rs:188); the pool has **no `DISCARD ALL` checkin barrier** (only a dirty-flag set by tx-rollback). The connection returns to the pool with `SET ROLE app_A` active. **Calibrated HIGH, not CRITICAL** (reviewer said CRITICAL): plugin-db's *primary* isolation is schema-qualification (app_B-stamped, role-independent), so a leaked `app_A` role on app_B's connection yields **permission-denied/DoS, not cross-tenant data reads** — but the per-app-role *defense-in-depth layer is defeated* and the next tenant inherits app_A's timeouts. Fix: `SET LOCAL`-in-explicit-tx for autocommit (closes setup-error + cancellation + desync at once) + a `DISCARD ALL` checkin barrier. (tx path is airtight — off-pool dedicated client + `SET LOCAL`.) | **verified** (src) |
| P2-D1 | MAJOR | D | **`ZeroShip-User` HMAC is request-bound but NOT app-bound, and `worker_key` is global.** A `(request_id, ZeroShip-User)` pair minted for app A verifies identically on `/dispatch/{app_B}` (core/auth/mod.rs:263 — no `app_id` in the MAC; worker derives app_id from the path independently, handler.rs:163). Cross-app identity isolation rests **entirely on gateway control flow** always pairing the right header with the right path — not on the token. Confirms + cross-component-elevates **W-1**. No client-reachable exploit today (gateway pairs correctly), but any gateway refactor / second `worker_key` holder / multi-app-per-dispatch makes it silent cross-tenant `env.auth` confusion. Fix: bind `app_id` into the HMAC + worker verifies its path app_id against it. | reviewer-evidenced |
| P2-B1 | HIGH | B | **Dispatcher prototype-chain method lookup.** `__zsDispatch` does `rpcDict[name]` with attacker-controlled `name` and no `hasOwnProperty`/`Object.create(null)` guard (dispatcher.ts:88; same in worker fast-path init.rs:794) — `name="constructor"`/`"toString"`/`"valueOf"` resolve to inherited functions and get invoked with client input. **Prod gateway 404s un-manifested ids before the worker** (fail-closed), but **dev has no gateway** so it's directly reachable; latent privilege-confusion. Fix: `Object.create(null)` + `hasOwnProperty` guard + a `constructor`/`__proto__` wireId regression test. | reviewer-evidenced |
| P2-B2 | HIGH | B | **No input size/depth cap on the dispatch path** — `request.text()`→`JSON.parse`→Zod with no byte/nesting limit before or in the dispatcher; gateway `max_input_bytes` defaults `None` (compiled.rs:335). Default-open JSON-parse CPU/heap DoS in the app isolate per RPC call. Fix: a default gateway ceiling + a depth/size guard pre-`JSON.parse`. | reviewer-evidenced |
| P2-B3 | HIGH | B | **Manifest-discovery ≠ runtime-registration.** Runtime `_zsRpc` is a **namespace-walk** (every callable export); the manifest (the gateway's only authz source) is **wrapper-marked exports only**. Fail-closed today (gateway 404s un-manifested ids) but the two paths disagreeing on "what is a procedure" is a latent auth-bypass class, and **app-layer authz does not exist** — it's 100% gateway manifest enforcement, so any non-gateway path (dev / future direct-worker) has **zero authz**. Fix: derive `_zsRpc` from discovered procedures (one shared wireId function), so registered≡authorized by construction. | reviewer-evidenced |
| P2-B4 | HIGH (builder) / MED (gen) | B | **Build-time `new Function()` eval of app source** (manifest.ts:526) — `defineApp(...)` arg is `eval`'d in the build process with full Node privileges. On the **builder service** (compiles untrusted creator/AI source on shared infra — the product), `defineApp((()=>{require('child_process').execSync(...)})())` is **build-host RCE / tenant-escape**. Fix: AST-parse the literal (the transform already has a parser), never `new Function`. | **verified** (src) |
| P2-B5 | MEDIUM | B | Output Zod validation default-OFF in prod (dispatcher.ts:129) → a handler that over-fetches (returns a full DB row incl `password_hash`) has no boundary redaction; the declared `output` schema is cosmetic at runtime. Enforce output parse when a schema is declared, or document loudly. | reviewer-evidenced |
| P2-B6 / P2-D2 / P2-B7 | LOW | B/D | 5xx error sanitization keyed on a runtime-readable env var (`AUTH_INSECURE_DEV`) the app could shadow → raw error (SQL/paths/secrets) leak on misconfig; `serve.rs` dev path doesn't scrub reserved client headers (dev-only, identity still safe via separate resolution); dev-auth dev-only guarantee is tree-shaking-dependent (no `sideEffects:false`) — **verified absent from built bundles today** but no CI grep-assert. | reviewer-evidenced |

### Pass-2 lane A — red-team verification + runtime gaps

**Pass-1 CRITICALs independently verified:**
- **RT-1 — CONFIRMED CRITICAL, empirically reproduced + fix verified.** A second reviewer wrote an in-tree adversarial test (`%OptimizeFunctionOnNextCall` + foreign receiver): a foreign *native* receiver returned **another class's memory** as an integer (silent type-confusion disclosure, no throw); a plain `{}` receiver **SIGABRT**'d. Applying `.signature(v8::Signature::new(scope, ctor_tmpl))` made both → `TypeError: Illegal invocation` while legit receivers keep the fast path — **fix confirmed correct + sufficient**. ⚠️ The **getter** fastcall path (`install.rs:404`) needs the identical fix, not just methods.
- **CT-B1 — CONFIRMED CRITICAL.** Full trace: checkout session built entirely in creator worker code, POSTed directly to Stripe with the creator's key/account; `applicationFeePercent` validated only `[0,100]`; no server-side session creation, no fee floor, `creator_id` self-attested, `record_payout` records verbatim. `fee=0` passes.
- **RT-6 → recalibrated MAJOR** (confirmed real — zero `adjust_amount_of_external_allocated_memory`; resource-evasion/OOM-DoS, not corruption). RT-2 → MAJOR, GW-1 → MAJOR (both confirmed real, pass-1 over-rated). *(I keep RT-6 in the shared-thread-containment cluster for fix-priority — whole-worker OOM denies all co-tenants, same fix-gate as RT-5/7.)*

**New pass-2 runtime findings:**
| ID | Sev | Finding |
| --- | --- | --- |
| P2-A1 | MAJOR | **`randomFillSync`/`randomFill` ArrayBuffer-coercion TOCTOU** (`node/crypto/random.rs:142,234`) — `byte_length()` cached *before* `uint32_value()` coerces offset/size (runs user JS); a `valueOf` that `buffer.transfer()`s (resizable AB + `transfer` are on by default in V8 147) detaches after the stale length check → OOB index → Rust safe-slice **panic** across the `extern "C"` boundary = abort-class **guest-triggerable worker DoS at will**. (Panic backstops silent OOB → MAJOR.) Fix: re-read `byte_length()` after coercion / coerce without invoking user JS. |
| P2-A2 | MAJOR | **`is_auth_host` unanchored prefix** (`dispatch.rs:345`, sharper GW-1 sibling) — `host_lc.starts_with("auth.zeroship.")` matches `Host: auth.zeroship.ai.evil.com` → request proxied into the **internal Hydra/Auth-UI upstream**. Auth-host confusion / route-into-internal-infra. Anchor to an exact host/suffix allowlist. |

**Cleared (negatives):** module-loader specifier spoofing **REFUTED** (closed-world resolution: bundle's flat `sources` + fixed native allowlist, no fetch/FS/compile-on-demand; re-importing `./__user__.js`/`zeroship` grants no caps — caps are `env.*` globals/args, not module exports). WebCrypto (`read_buffer_source`) + fetch-body (`chunk_to_bytes`) buffer reads have **no detach TOCTOU** (synchronous length-read+copy, no JS reentry). The dispatcher prototype-confusion (P2-B1) was independently re-confirmed by this lane.

**Seams confirmed TIGHT (request-trace negative results — reassuring):** RPC (`/__zeroship/v1/<id>`) and fetch traverse the **same** `execute_resource_tree` auth/CSRF/rate-limit gate (no RPC auth-skip); per-request user/ctx is **request-id-keyed, set per turn, cleared on every terminal path** (no cross-request bleed on a pooled isolate; holds for RPC + fetch); `__Host-` cookie + `app==oauth_client_id` claim check + sector `pws_` subject (no confused-deputy — app B can't get app A's user); worker **ignores `X-App-Id`** (app_id from path only); `/dispatch` + `/logs` both worker-auth-gated, only `/health`+`/metrics` open (no identity); DPoP/Bearer both bind `client_id`→route with per-app revocation. **Dispatcher solid:** gateway fail-closed on unknown wireIds, plain `export function` helpers excluded from registration, secure-by-default forces explicit `publicly_accessible` for `auth:"anon"`, `__zsDbPlatform` deleted before any handler runs (top-level reachability is the separate DB-5/RT-4 finding).

---

# Pass 3 — remaining surfaces (authz policies · DB-layer RLS · federated auth · CLI)

Passes 1-2 covered the 9 crates + SDK/RPC + request-traces + red-team. Pass 3 covers the surfaces
explicitly noted as not-yet-deep-reviewed.

| Lane | Scope | Status |
| --- | --- | --- |
| P3-A | **Cedar authz** | ✅ SOUND (no CRIT/HIGH); 1 MED app-flag fail-open; CT-A1 = plan-validation not authz |
| P3-B | **DB-layer authz** | ✅ SOUND (forced RLS+WITH CHECK); 2 MAJOR (dpop_jti grant, oauth no-RLS-backstop) |
| P3-C | **Federated auth flows** | ✅ SOUND, no account-takeover; 2 MED (device rate-limit, linker 23505) |
| P3-D | **CLI + control client** | ✅ HIGH (serve 0.0.0.0+env-flood) + control-client origin MED |

### Pass-3 findings — **headline: the authorization + auth layers are SOUND**

The three highest-stakes remaining surfaces (Cedar authz, SQL-layer RLS/grants, federated auth) came back with **no new CRITICAL/HIGH** — materially raising confidence in the platform's core trust model.

| ID | Sev | Lane | Finding | Status |
| --- | --- | --- | --- | --- |
| P3-D1 | HIGH | D | **`zeroship serve` binds `0.0.0.0` (no `--host` flag, no authz) + floods the app's `process.env` with the dev's entire shell env** (`std::env::vars()`, main.rs:178) → `OPENAI_API_KEY`/`STRIPE_KEY`/`DATABASE_URL` reachable by anyone on the LAN. Dev-only, but a real secret-exposure footgun. Fix: default-bind `127.0.0.1`; env allowlist. | reviewer-evidenced |
| P3-B1 | MAJOR | B | **`zeroship_gateway` has no grant on `dpop_jti`** but INSERT/SELECT/DELETEs it for DPoP replay protection (dpop.rs:927) → `permission denied` → DPoP auth fails or replay-detection silently degrades. Add the grant in a new changeset. | reviewer-evidenced |
| P3-B2 | MAJOR | B | **`oauth_clients`/`oauth_grants` have no RLS backstop** — per-app OAuth secrets/grants isolated app-layer-only (RLS would be inert today since only BYPASSRLS roles touch them — but no SQL net beneath an app-scoping bug, unlike the 4 forced-RLS tenant tables). | reviewer-evidenced |
| P3-C1 | MEDIUM | C | **`/device` user-code POST has no app-level rate-limit** (every other sensitive POST does) — only Hydra's throttle guards online guessing of low-entropy RFC-8628 user codes; a hit can bind a victim's pending device authz. Add a per-IP/session bucket. | reviewer-evidenced |
| P3-A1 | MEDIUM | A | **Cedar app-flag lookup fail-OPEN** — `load_app_flags` (eval.rs:218) unqualified `apps` table + missing row → `AppFlags::default()={suspended:false}` → `suspended`/`audit_locked` forbids silently stop applying. Bounded (live apps always load flags) but the one control that should fail-closed fails open. Qualify the table + default missing→deny. | reviewer-evidenced |
| P3-C2 / P3-D2 | MEDIUM | C/D | Auth `linker` doesn't map a `(provider,subject)` `23505` race to a policy outcome (opaque 500 vs "already linked"; not exploitable — step-1 read prevents the takeover, write has no defense-in-depth); `@zeroship/control` attaches the bearer to whatever `baseUrl`/`path` resolves to with no origin binding → master-key exfil if `baseUrl` influenced. | reviewer-evidenced |
| P3-misc | LOW | A/B/C/D | `pitr_targets` `GRANT SELECT…TO PUBLIC` → cross-tenant PITR-*metadata* read; `0014` swallows `WHEN OTHERS` as race-success; sandbox tables app-layer-only; CLI `--token` in argv (`ps`-visible), no TLS enforcement on non-loopback control URL; magic 6-digit code's 5-attempt cap is load-bearing; `oauth_stash`/`PendingLink` share an HMAC key (non-confusable by shape today). | reviewer-evidenced |

**Solid / verified-clean (pass 3):** Cedar — default-deny, DB-backed memberships (absence=deny), TOKEN⊂USER two-call fail-closed + regression-tested, PAT mint-time subset check, platform-role escalation admin-gated, injection-defended. **CT-A1 clarified: the authz is correct** (owner changing their own app's plan is by design; the Stripe payout surface requires platform `billing`/`admin` via `Resource::Any`) — CT-A1 is a *plan-validation/semantics* issue, not an authz over-permit. SQL — forced RLS + `WITH CHECK` on all 4 tenant tables, non-BYPASSRLS gateway + per-app roles, pinned-`search_path` `SECURITY DEFINER`s, `BYPASSRLS` confined to 2 roles, Hydra least-priv (0027), no `GRANT…TO PUBLIC` on `zeroship.*`. Federated auth — OAuth state+PKCE+nonce binding, full `id_token` validation (no alg-confusion), verified-email-only linking (domain-re-registration takeover defended), consent no-escalation, single-use atomic tokens, signup/forgot enumeration-safe, `/me` last-method orphan-guard. **No account-takeover, no cross-tenant authz/RLS hole found.**

---

## Audit conclusion (3 passes, ~52 reviewer-lanes — comprehensive)

Reviewed end-to-end three times over: pass 1 (9 crates module-by-module), pass 2 (SDK/RPC dispatcher, PG-pool, request-traces, red-team verification), pass 3 (Cedar authz, SQL RLS/grants, federated auth, CLI). **The verdict is consistent and strengthening:** the security *fundamentals* — identity signing, the authorization model (Cedar + forced-RLS), tenant isolation, injection prevention, the V8 node-compat sandbox, the auth flows — are **well-built and largely sound**, repeatedly confirmed across independent reviewers and request-traces. The must-fix items are concentrated and well-characterized:
- **Runtime shared-thread containment** (RT-1 isolate escape [reproduced+fix-verified], RT-5/6/7 DoS) — top priority.
- **The money model** (CT-B1 — server-side fee enforcement).
- **The sandbox micro-VM network infra** (SB-A1/2 — if that backend is used for untrusted workloads).
- **The pass-2 cluster** (PG-pool role/state leak, SDK dispatcher hardening, build-host RCE).
- A long tail of resource-bounds / fail-open-env-gate / host-trust hardening.

Further passes would be diminishing returns (re-review of covered ground). **The audit is complete.**

---

# Pass 4 — new review axes (supply-chain · realtime · background-jobs · secrets-hygiene)

Passes 1-3 covered the code module-by-module + SDK/RPC + traces + red-team + authz/RLS/auth-flows.
Pass 4 covers axes orthogonal to the per-module review (not re-review).

| Lane | Scope | Status |
| --- | --- | --- |
| P4-A | **Dependency / supply-chain** | ✅ mostly clean; 1 MED (rustls-webpki), SC1 HIGH (sandbox TCB unpinned) |
| P4-B | **Realtime auth/isolation** | ✅ subscription path SOLID; 2 HIGH + 1 MAJOR on the `serve` WS/SSE path |
| P4-C | **Background jobs / crons** | ✅ scoping/least-priv solid; 3 HIGH (audit-trigger, no-liveness, control_key plaintext) |
| P4-D | **Committed secrets / config hygiene** | ✅ CLEAN — no real committed secret |

### Pass-4 findings

| ID | Sev | Lane | Finding | Status |
| --- | --- | --- | --- | --- |
| P4-C-F4 | HIGH | C | **`control_key` shipped over plaintext HTTP** — the gateway↔control route-sync (`sync.rs:159`, hand-rolled `TcpStream`) sends `Authorization: Bearer {control_key}` in cleartext every ~5s; that key authorizes pulling **every app's decrypted env/secrets** via `/internal/*`. A passive observer on the gateway↔control segment captures the master internal credential. (Concretizes DR-6 no-TLS.) Require TLS / private-mesh-only for the internal API. | reviewer-evidenced |
| P4-C-F1 | HIGH | C | **Audit append-only tamper-trigger disabled session-wide during the retention sweep** — auth's sweep does bare `SET zeroship.audit_retention='on'` (not `SET LOCAL`, no tx) on the **shared, pipelined** auth connection (`audit_retention.rs:110`, *verified*); for the hourly sweep window any concurrently-pipelined query runs with the forensic-integrity trigger OFF. (HIGH not CRIT: tampering needs a concurrent `audit_events` DELETE/UPDATE path, which doesn't exist today — defense-in-depth erosion.) Fix: dedicated connection + `SET LOCAL`-in-tx (the control peer already does this). | **verified** (src) |
| P4-C-F2 | HIGH | C | **Security crons have no liveness/fail-closed signal** — JWK rotation, token/revocation sweeps loop with `if let Err(e){error!}` + `.detach()`, no supervisor, no last-success metric. A stuck/panicked rotation/revocation job silently stops (stale signing keys never retired; `token_revocations` grows unbounded — auth-hot-path latency/DoS) with only an indistinguishable `error!` line. Export per-cron last-success + alert. | reviewer-evidenced |
| P4-B-1 | HIGH | B | **Unbounded WS frame allocation on `zeroship serve`** — `read_ws_frame` (`serve.rs:819`) honors a client `u64` payload length → `vec![0u8; n]` with no cap → trivial OOM/abort (a 16-byte header triggers a ~280TB alloc). The bounded `FrameReader(max_frame,max_message)` is a *separate* impl used only by the JS client. (Single-node serve path; multi-node 501s WS.) Cap before allocate, close 1009. | reviewer-evidenced |
| P4-B-2 | HIGH | B | **WS per-connection identity not bound** — the WS-event pump dispatches `onmessage`/`onclose` without setting `executing_request_id`/per-request user (and clears it, `runtime.rs:2226`), so `env.auth.getUser()` inside a WS handler returns `null` **or a stale leftover from a prior request** (identity-bleed window on a pooled isolate). Auth-correctness footgun on the channel creators are told to use. Bind the connection's user at upgrade + re-establish it per WS turn. | reviewer-evidenced |
| P4-A-R1 | MEDIUM | A | **`rustls-webpki 0.103.11`** — 3 advisories (CRL-parse panic DoS + name-constraint bypass for URI/wildcard certs), reachable via `cyper` (outbound HTTPS). One-line fix: `cargo update -p rustls-webpki` → ≥0.103.13. | reviewer-evidenced |
| P4-A-SC1 | HIGH(s-c) | A | **Sandbox VM-isolation TCB fetched without checksums** (extends SB-A4) — `gcp-worker-startup.sh:153` `gs_pull`s `cloud-hypervisor`, `vmlinuz`, the rootfs image, AND the `zeroship-sandbox` controller binary with **no SHA verification** (only `nomad-driver-ch` is pinned). GCS-bucket-write = code-exec on every sandbox host. Extend the existing SHA-pin gate to all of them. | reviewer-evidenced |
| P4-B-3 / RT-3 | MAJOR | B | SSE/streaming + native-WS pumps (`serve.rs:575`/`:1119`) have no wall/idle-timeout or `CancelFlag` — a never-ending stream pins the connection+worker (same class as RT-3/GW-5). | reviewer-evidenced |
| P4-C-F3 | MEDIUM | C | `db.replication.dropAbandoned({inactiveSeconds:0})` — tenant-controlled, reaps the app's own freshly-created slots → self-DoS (resync storm). Floor to ≥60s or make control-plane-only. | reviewer-evidenced |
| P4-misc | LOW/latent | B/C/A | Latent **CSWSH** on the WS upgrade (no server-side Origin check; SameSite=Lax cookies; subscriptions bypass `csrf_origins`) — only bites once the gateway proxies WS; DPoP JTI sweep is opportunistic + caller-supplied TTL; `migration_sweeper`/replication-watchdog reapers are dead-code stubs (orphan migration rows + abandoned slots never GC'd → slow disk-fill); `react-router<7.15` DoS (builder app only); Docker bases use mutable tags (`rust:latest`); `auth-clients.example.toml` carries an inert dev secret. **Non-vuln note:** `tokio 1.51.1` (full) is in the tree via `crates/platform` — a deviation from the "zero tokio" invariant. | reviewer-evidenced |

**Solid / verified-clean (pass 4):** **No committed secrets** (every secret-like value is a `--dev-insecure`-gated compose default behind fail-closed ≥32B startup guards, an empty template, a secret-manager `urn:`/`arn:` ref, or a test fixture); `.gitignore` solid; no secret-logging; GCP provisioning auto-generates from `/dev/urandom`. **Subscription tenant-scoping + CDC-masking SOLID** (app_id-stamped, broker-keyed, not JS-widenable; CDC carries ciphertext/mask-siblings — no plaintext leak, regression-tested). **Background-job cross-tenant scoping + DB-role least-priv solid** (per-app `slot LIKE` scoping, app_id-override ignored, platform-role not BYPASSRLS, control-key constant-time, the control audit-sweep correctly isolated). **Deps mostly clean** (ring/aws-lc-rs/jsonwebtoken/argon2/tar/zstd/postgres-protocol no current advisory; no `h2`; lockfiles committed; no git/tarball deps; no network `build.rs`).

---

## FINAL audit conclusion (4 passes, ~56 reviewer-lanes — every axis covered)

Pass 1 (9 crates module-by-module) · pass 2 (SDK/RPC, PG-pool, request-traces, red-team) · pass 3 (Cedar authz, SQL RLS/grants, federated auth, CLI) · pass 4 (supply-chain, realtime, background-jobs, secrets). **The review is exhausted — every component, integration seam, and orthogonal axis (deps, realtime, jobs, secrets) has been covered, the top findings re-verified against source (RT-1 reproduced + fix-verified), and most surfaces re-confirmed by independent reviewers.**

**Consistent verdict:** the platform's security *fundamentals are well-built* — identity signing, the authz model (Cedar + forced-RLS, no over-permit), tenant isolation (DB schema + KV hash-tags + subscription scoping), injection prevention, the V8 node-compat sandbox, the auth/OAuth flows (no account-takeover), config/secret hygiene. The serious findings are **concentrated, well-characterized, and have no live exposure** (pre-launch). Total: **7 CRITICAL · ~14 HIGH · ~22 MAJOR** + a long MINOR tail — all documented per-finding with file:line + fix.

**Recommended remediation order (the must-fix-before-launch spine):**
1. **RT-1** V8 isolate escape (reproduced; one-line `.signature()` fix verified — *and the getter path*).
2. **CT-B1** enforce the 15% fee server-side.
3. **RT-5/6/7 + CT-A1** shared-thread containment + plan validation.
4. **P2-C1** PG-pool session-state leak + **P2-B1/2/3** SDK dispatcher hardening + **P2-B4** build-host RCE.
5. **P4-C-F4/F1/F2** (control_key TLS, audit-trigger scoping, cron liveness) + **P4-B-1/2** (WS frame cap, WS identity).
6. **SB-A1/2/A4 + P4-A-SC1** sandbox infra (if that backend serves untrusted workloads).
7. The HIGH host-trust/storage/redis + the MAJOR resource-bounds / fail-open-env-gate / no-TLS sweep; then the one-line dep bumps (rustls-webpki, react-router).

**Further review passes = re-review (diminishing returns). The right next step is remediation, not more review.**

---

## Remediation log & execution plan (offline pilot, 2026-06-03)

Operator is offline and asked for a **review**. So the remediation rule for this pass is: **apply only fixes that are both (a) contained — a clear, single correct behavior, no design choice — and (b) trivially reversible (commit-only, never pushed).** Everything that requires a design decision, breaks a wire/SDK contract, or touches money flow is **flagged for the operator's return, not silently rewritten.** Commit-only throughout; nothing pushed.

### Applied this pass (commit-only, NOT pushed)

| ID | Sev | Fix | Commit | Verification |
| --- | --- | --- | --- | --- |
| **RT-1** | CRITICAL | `v8::Signature::new(scope, ctor_tmpl)` added to **both** the `#[v8_method(fastcall)]` and `#[v8_getter(fastcall)]` install sites (`runtime-macros/.../emit/install.rs`); foreign/forged receivers now deopt to the brand-checked slow path. | `39913e47` | TDD: pre-fix the new `v8_fastcall_smoke` foreign-receiver tests **SIGABRT** (reproduced isolate escape); post-fix **11/11 fastcall + 233 runtime-lib + all fetch/headers + WPT-headers/fetch** green. Only production fastcall is `Headers.has`; non-fastcall codegen byte-identical. |
| **P4-A-R1** | MED | `cargo update -p rustls-webpki` → 0.103.13 (transitive; no manifest change). | `fd79d65f` | `zeroship-gateway` (rustls TLS consumer) builds clean post-bump. |

### Flagged for operator return — **NEEDS DESIGN REVIEW** (do NOT apply blind)

- **CT-B1 (15% fee) — money-flow redesign, not a one-liner.** Re-traced this pass: `sdks/payments/src/checkout.ts:114` issues a Connect **direct charge** (`stripe-account: creatorAccountId`), where `subscription_data[application_fee_percent]` is the *only* revenue path to the platform — and it executes **inside creator-controlled worker code** with `applicationFeePercent` caller-overridable (`checkout.ts:80`, default 15 but settable to 0), and the creator can bypass the SDK and craft the Stripe POST directly. The webhook (`stripe_handlers.rs:444`) merely *records* whatever fee Stripe reports. ⇒ the fee is **100% creator-controlled today.** Correct fix = move checkout-session creation **server-side** (control plane stamps `application_fee_percent` from a platform-held creator→fee mapping, using the platform key; creator code receives only a session URL and never the fee knob). This breaks the `@zeroship/payments` SDK contract and defines new revenue-model surface → **operator design call required.** (Pre-launch: no live money, so no live loss.)
- **CT-A1 (plan self-escalation)** — requires a server-side plans/limits model that doesn't exist yet; design-coupled to billing. Defer.
- **P4-C-F4 (control_key over plaintext)** — the fix is *transport/deploy* (require TLS / private mesh on the internal API), not a code one-liner; deploy-topology decision. Defer.
- **SB-A1/A2/A4 + P4-A-SC1 (sandbox infra)** — GCP startup scripts + checksum-pinning of the VM TCB; ops/infra change, not Rust. Defer to the sandbox-backend owner.
- **P2-B4 (build-host RCE via `new Function` of creator source)** — builder-host hardening; needs a sandboxing-strategy decision. Defer.

### APPLIED 2026-06-04 — **CONTAINED & SAFE fixes** (operator authorized "just do it"; workflow-orchestrated, TDD, commit-only NOT pushed, each independently verified)

All seven applied via a sequential-fix → parallel-verify workflow (14 agents); every commit independently re-reviewed (commit stages only intended files, regression test genuinely fails-without-fix, fix contained, nothing pushed). Stacked HEAD re-tested green: runtime `--lib` 239 pass + the 2 new integration tests, gateway `is_auth_host` 2 pass, PG-gated crates compile + self-skip.

| ID | Sev | Fix | Commit | Regression test |
| --- | --- | --- | --- | --- |
| **P2-A2** | MAJOR | `is_auth_host` (`gateway/.../dispatch.rs`) was `== "auth.zeroship.ai" \|\| starts_with("auth.zeroship.")` — the prefix arm matched `auth.zeroship.ai.evil.com`. Replaced with an exact `AUTH_HOSTS.contains(host_lc)` allow-list (prod + `.localhost` dev host). | `6ab2b4b0` | `is_auth_host_rejects_unanchored_lookalikes` (+exact-accept); RED pre-fix, GREEN post — pure unit. |
| **P4-B-1** | HIGH | native WS frame reader (`serve.rs`) did `vec![0u8; n]` from a client `u64` → ~280TB OOM. Cap at existing `DEFAULT_MAX_FRAME_SIZE` (1 MiB) **before** allocate; synthesize a 0x8/1009 Close (handled by both call sites unchanged). | `b4976caa` | `oversized_frame_rejected_with_1009_no_alloc` (+at-cap/normal still parse); RED pre-fix. |
| **P4-B-3** | MAJOR | SSE/chunked stream pump (`serve.rs`) had no idle bound. Added `STREAM_IDLE_TIMEOUT` (300s, **idle**-based — each chunk re-arms, so legit long LLM/SSE streams run forever; only wedged ones close). | `57c61d8f` | `idle_stream_times_out` (+active-not-cut/closed-resolves); RED pre-fix. **Carve-out below.** |
| **P4-B-2** | HIGH | WS turns set no per-request auth ctx → `env.auth.getUser()` in a WS handler returned a **stale** prior-request user. Bind the connection user at `mint_pair` (upgrade) + re-establish per WS turn via `current_user` (the single getUser/requireUser chokepoint). | `ddd2d264` | `ws_handler_getuser_is_connection_user_not_stale_leftover` — verifier reverted-fix → RED shows actual `usr_BBB` bleed. |
| **RT-7** | MAJOR | detached pump held a **strong** `Rc<RefCell<RuntimeInner>>` → eviction never dropped the isolate. Downgraded the pump back-ref to `Weak` (upgrade-or-exit at use). | `9cd7afc0` | `eviction_drops_isolate_after_pump_started` (Drop-flag probe); RED pre-fix. |
| **P2-C1** | HIGH | autocommit funnel ran session-level `SET ROLE`+timeouts; RESET skipped on **cancellation** → leaked role/timeout to the pooled conn. Switched to `SET LOCAL` inside an explicit `compio_postgres::Transaction` (rollback-on-drop + pool dirty-barrier auto-reset). | `f37c07a6` | `autocommit_cancelled_query_does_not_leak_role_or_timeout_to_pool` — **PG-gated, not run here** (`ranHere:false`); self-skips; verified by code + compile. |
| **P4-C-F1** | HIGH | audit-retention sweep did a bare session `SET zeroship.audit_retention='on'` on the **shared pipelined** conn (disarmed the tamper trigger session-wide). Now opens its **own** dedicated per-tick connection + `SET LOCAL`-in-tx (mirrors control's `registry.conn()`). | `5992dcec` | `sweep_guc_does_not_leak_past_its_transaction` — **PG-gated, not run here**; verified by code + compile (3 non-PG asserts pass). |

**One carve-out (P4-B-3, agent-flagged, correctly deferred):** only the **SSE/chunked** pump got the idle timeout. The **native-WS pump** (`read_ws_frame`/`native_ws_pump`) was *deliberately left unbounded* because a naive read deadline would wrongly kill an idle-but-alive WebSocket — that path does no server-initiated ping keepalive. A correct WS idle bound needs ping/pong keepalive or a configurable knob = a separate **design** change. → moved to the design-review list. (Single-node `serve`/bench path; multi-node 501s WS.)

### Still flagged — NEEDS DESIGN REVIEW (unchanged + the new WS-pump carve-out)

CT-B1 (money-flow redesign — *in active discussion*), CT-A1 (plan model), P4-C-F4 (internal-API TLS), sandbox-TCB checksums (SB-A1/2/A4 + P4-A-SC1), P2-B4 (build-host sandbox), **WS-pump idle bound** (P4-B-3 remainder, needs ping/pong). Plus the MINOR tail: `dropAbandoned` ≥60s floor, `react-router` bump (builder app), pin Docker base tags.

**Pilot stance (2026-06-04):** top CRITICAL **RT-1** + the rustls-webpki CVE + **all 7 contained HIGH/MAJOR fixes are applied, verified, and reversible (commit-only, 41 ahead of origin/main, none pushed).** Remaining work is design-gated and awaits operator decisions (CT-B1 first).
