# sandbox TODO

## P1 (next-after-Phase-3)

### ~~Read-only `sandbox_admin_ro` role for the read endpoints~~ — CLOSED 2026-05-25 (T1)

T1 fixer (2026-05-25) implemented the controller-side half of this entry:
HTTP-layer `AdminRole::{Full, ReadOnly}` role gate at every `/admin/*`
handler, `SANDBOX_ADMIN_RO_TOKEN_PATH` env var mirroring the existing
admin-token path, `AppState::with_admin_ro_token` builder + boot-time
two-distinct-tokens guard, 9 e2e tests + 11 lib unit tests pinning the
authorization matrix.

Routes classified READ (accept `AdminRole::ReadOnly`): `GET /admin/sandboxes`,
`GET /admin/sandboxes/{id}`, `GET /admin/users/{user_id}/sandboxes`,
`GET /admin/users/{user_id}/shares`, `GET /admin/hosts`,
`GET /admin/sandboxes/{id}/wake/{wake_id}`. Routes classified WRITE
(`AdminRole::Full` required, RO bearer → 403 `insufficient_role`):
`POST /admin/sandboxes/{id}/snapshot`, `POST /admin/sandboxes/{id}/wake`,
`POST /admin/sandboxes/{id}/cold-boot`, `DELETE /admin/users/{user_id}`,
`GET /admin/users/{user_id}/export` (the export endpoint is `Full` despite
being a GET — GDPR-cascade aggregation has write-class blast radius).

What's still **deferred** to Phase 5 (the database-layer half): a fifth
`sandbox_admin_ro` pg role with `SELECT`-only grants on the admin-visible
tables, plus a `Database::pool_admin_ro()` helper that the read endpoints
route through. Today every endpoint still runs as `sandbox_app`; if a future
read handler accidentally executes a `DELETE`, the controller-side gate
won't catch it. Defense-in-depth at the pg-role layer remains valuable, but
the HTTP-layer role gate is the higher-impact half (it stops every WRITE
endpoint from being callable by RO operators, full stop). Schema work
follows in the same Phase-5 sweep as the JWT migration; both touch
admin-side state.

### Rate-limit on heavyweight admin endpoints

Round-4 admin-API review IMPORTANT #8 (deferred). `GET /admin/users/{u}/export`
and `DELETE /admin/users/{u}` are the two heaviest endpoints — REPEATABLE
READ tx + multi-table aggregation, and a multi-statement cascade
respectively. A misbehaving operator script that loops one of these would
saturate the gdpr pool (`max_size = 4`) or the app pool (`max_size = 16`) in
seconds. Phase 5 should add per-endpoint sliding-window rate limits keyed
on the bearer (or, post-JWT, on `admin_id`).

Cosmetic but related: pagination shape, dup SQL between `list_all_sandboxes`
and `list_all_sandboxes_inner`, idempotency-key support for `DELETE`,
503-vs-401 disclosure on the disabled-vs-misconfigured boundary —
collectively MINOR #1/#2/#3/#6/#7 in the round-4 review. Bundle them with
the JWT migration.

### Burst-saturation tuning at high concurrent-stops/create

Round-3 GCP stress (60-on-one-worker after the MHz=500 fix unlocked
density past round-2's 16-cap):
- 29/60 creates ok (the other 31 hit `alloc_running_timeout_secs=60s`
  ceiling; CH boot under c=60 saturation took longer than 60s)
- 29/29 stops ok with the new 120s `host_fence_timeout_secs` default
  (was 30s — fence p99 measured at 100s, just inside the new budget)

Two more knobs worth tuning together for production:
1. **Bump `alloc_running_timeout_secs` 60 → 120 (or 180)** — matches
   the spirit of the fence bump. CH boot p99 under burst is 60s on a
   single n2-standard-32; placement+boot in <60s is unrealistic at
   c=60.
2. **Controller-side stop semaphore** (better long-term shape) — cap
   concurrent host_fence polls at, say, 16 per worker. Avoids the
   teardown stampede that pushed fence p99 to 100s.

Out of scope for the immediate Phase-3 close — the MHz fix is the
primary unlock; these are follow-ons that surface only at the new
density. Capture in case a future 300-VM cluster-wide stress runs into
them.

### Controller pause/snapshot APIs (CH backend)

Idle AI-builder workspaces today hold tap + memory + Nomad alloc forever. CH
supports `VM.Pause`/`VM.Resume`/`VM.Snapshot`/`VM.Restore` over its REST
socket — wiring it gives:
- **Pause/Resume** — freezes vCPU; keeps RAM pinned, tap held. ~ms to
  un-pause. Saves CPU only.
- **Snapshot/Restore** — writes RAM+device-state to disk; releases the
  alloc, tap, and RAM. Restore cold-starts in seconds (depends on workspace
  size). Saves the full footprint at the cost of restore latency.

Out-of-scope for the current branch because it requires:
1. **Backend-trait additions** — `pause`/`resume`/`snapshot`/`restore` with
   per-backend feasibility (NomadCh: yes; Docker: pause-only; K8s: N/A).
2. **Agent-side proxy** — controller talks to wrapper-launched CH HTTP API;
   today the agent doesn't expose `/vm.pause` etc.
3. **Persistence schema** — `sandbox.sandboxes.state` needs `paused`,
   `snapshotted` lifecycle states + transition guards. Interacts with the
   Phase-2 lease-takeover semantics (a paused VM still heartbeats; a
   snapshotted one doesn't, and "no heartbeat" must NOT trigger
   takeover-on-unreachable for a deliberately-snapshotted sandbox).
4. **Auth** — owner-only by default; admin-override flagged.
5. **Idle auto-pause** — separate concern; needs a controller-side
   activity tracker (last-request timestamp) and a sweep loop. Probably its
   own follow-up.

Best-path implementation: **fresh worktree off master with a design
proposal first**, critic-loop, then code. The lease-takeover interaction
needs design before code. Reference: round-1/2 of this branch's review
loops on Phase-1/2 lease-takeover for the model the proposal should
extend.

## P2

(Backlog from prior rounds. Phase-5 production-hardening adds: per-operator
JWT — see `docs/decisions/2026-05-05-sandbox-admin-shared-bearer.md`.)
