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

## P2

(Backlog from prior rounds. Phase-5 production-hardening adds: per-operator
JWT — see `docs/decisions/2026-05-05-sandbox-admin-shared-bearer.md`.)
