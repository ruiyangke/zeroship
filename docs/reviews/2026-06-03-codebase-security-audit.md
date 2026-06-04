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
| 1 | **gateway** (`crates/gateway`) | internet-facing front door | 🔄 reviewing (routing, proxy/SSRF, enforce/rate-limit, signing) |
| 2 | **runtime** (`crates/runtime`) | V8 sandbox, fetch/SSRF, WS, crypto, capability boundary | ⏳ queued |
| 3 | **plugin-kv + plugin-storage** | sibling native primitives (plugin-db lens) | ⏳ queued |
| 4 | **control** (`crates/control`) | deploy, billing/Stripe, env, route registry | ⏳ queued |
| 5 | **worker** (`crates/worker`) | bundle loading, isolate lifecycle/eviction | ⏳ queued |
| 6 | **bundle** (`crates/bundle`) | .zship artifact: unpack/path-traversal, blob store | ⏳ queued |
| 7 | **compio-postgres / compio-redis** | bespoke wire-protocol drivers | ⏳ queued |
| 8 | **sandbox** (`crates/sandbox`) | Nomad + Cloud Hypervisor VM isolation | ⏳ queued |
| 9 | **core** (`crates/core`) | typed_id, signing utils, wire types | ⏳ queued |

Legend: ⏳ queued · 🔄 reviewing · ✅ findings recorded

---

## 1. Gateway — findings

_(reviewers running; findings recorded on completion)_
