# sandbox TODO

## P1 (next-after-Phase-3)

### Read-only `sandbox_admin_ro` role for the read endpoints

Round-4 admin-API review IMPORTANT #7 (deferred). Today every `/admin/*` GET
runs as the `sandbox_app` role (full SELECT/INSERT/UPDATE/DELETE on the
non-events tables). The destructive `DELETE /admin/users/{user_id}` runs as
`sandbox_gdpr`, which is correctly scoped, but the read endpoints would
benefit from defense-in-depth: a fifth role with only `SELECT` on the
admin-visible tables and zero write grants. Operator-tooling bugs (a stray
UPDATE in a future detail handler) then fail at the pg layer, not at the
controller.

Out-of-scope for Phase 3 because it requires (a) a new migration to create
the role + grants, (b) a new env var (`SANDBOX_DATABASE_URL_ADMIN_RO`)
falling back to `SANDBOX_DATABASE_URL`, (c) a per-handler routing decision
(`open_app_pool` → `open_admin_ro_pool`). Schema work + a wider blast radius
than fits Phase 3's freeze.

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
