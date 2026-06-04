# Whole-codebase security audit (2026-06-03)

Autonomous, self-paced (`/loop`) security audit of the zeroship codebase, module
by module, hardest-first. Each module is reviewed by focused opus reviewers
(adversarial, file:line, honest — no manufactured findings); the orchestrator
verifies top findings against source and records them here.

**This is a review pass** — findings only, no fixes applied while the operator is
offline (anything CRITICAL + trivially-safe is flagged for action on return).

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
| 2 | **runtime** (`crates/runtime`) | V8 sandbox, fetch/SSRF, WS, crypto, capability boundary | 🔄 reviewing |
| 3 | **plugin-kv + plugin-storage** | sibling native primitives (plugin-db lens) | ⏳ queued |
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

## 2. Runtime — findings 🔄

_(V8 sandbox / fetch-SSRF / resource-limits / crypto-node-compat reviewers running)_
