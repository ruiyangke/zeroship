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
>    the shim + a foreign-receiver regression test. (Verified against source.)
> 2. **RT-5 — sync CPU loop wedges the shared worker thread** when `cpu_limit` is
>    `None` (`unlimited`/`enterprise` plans) → permanent co-tenant starvation.
> 3. **RT-6 — off-heap memory escapes the V8 heap cap** (`Buffer.allocUnsafe`,
>    fetch bodies) → OOM-kills the whole worker process + every co-tenant isolate.
> 4. **RT-7 — LRU eviction leaks the isolate** (pump strong-`Rc` cycle; `Drop`
>    never runs) → unbounded resource leak under app churn.
>
> RT-5/6/7 mean **mutually-untrusted apps are not yet safe on a shared worker thread**.

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

## Module order + status

| # | Module | Why | Status |
| --- | --- | --- | --- |
| 1 | **gateway** (`crates/gateway`) | internet-facing front door | ✅ 4 HIGH (host-trust ×2, rate-limit ×2), signing strong, no SSRF/traversal |
| 2 | **runtime** (`crates/runtime`) | V8 sandbox, fetch/SSRF, WS, crypto, capability boundary | ✅ **4 CRITICAL** (isolate escape + 3 DoS), 2 MAJOR SSRF; node-compat + SSRF posture strong |
| 3 | **plugin-kv + plugin-storage** | sibling native primitives (plugin-db lens) | 🔄 reviewing |
| 4 | **control** (`crates/control`) | deploy, billing/Stripe, env, route registry | ⏳ queued |
| 5 | **worker** (`crates/worker`) | bundle loading, isolate lifecycle/eviction | ⏳ queued |
| 6 | **bundle** (`crates/bundle`) | .zship artifact: unpack/path-traversal, blob store | ⏳ queued |
| 7 | **compio-postgres / compio-redis** | bespoke wire-protocol drivers | ⏳ queued |
| 8 | **sandbox** (`crates/sandbox`) | Nomad + Cloud Hypervisor VM isolation | ⏳ queued |
| 9 | **core** (`crates/core`) | typed_id, signing utils, wire types | ⏳ queued |

Legend: ⏳ queued · 🔄 reviewing · ✅ findings recorded

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

## 3. plugin-kv + plugin-storage — findings 🔄

_(reviewers running)_
