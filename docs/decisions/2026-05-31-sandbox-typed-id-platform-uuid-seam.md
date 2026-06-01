# Sandbox typed-id ↔ platform UUID identity seam (no FK bridge)

## Context

The unified `zeroship` schema now holds two identity worlds in the same
database:

- **Platform world (UUID).** `zeroship.users(id UUID)` and
  `zeroship.apps(id UUID)` are the durable auth/control entities
  (`0002_auth.sql`, `0004_control.sql`). Every platform table that points
  at a user or an app does so with a `UUID … REFERENCES zeroship.users(id)
  ON DELETE CASCADE` (or `…apps(id)`) FK — see `0002`, `0005`, `0006`,
  `0007`. Referential integrity + cascade is the platform's deletion model.

- **Sandbox world (typed-id TEXT).** `zeroship.sandboxes`,
  `zeroship.shares`, `zeroship.sandbox_events`, and
  `zeroship.deleted_sandboxes` (`0011_sandbox_initial.sql`) key on
  typed-id base62 strings: `user_id TEXT CHECK (~ '^usr_[0-9A-Za-z]{20,40}$')`,
  `sandbox_id sbx_…`, `project_id prj_…`, `host_id hst_…`, `token_id tok_…`.
  These are `crates/core/src/typed_id.rs` ids (UUIDv7 + base62 + entity
  prefix), produced and validated by the sandbox controller — **not** the
  raw UUIDs stored in `zeroship.users.id` / `zeroship.apps.id`.

The sandbox `user_id` and the platform `zeroship.users.id` denote the same
human, but they are different *encodings* in different *namespaces*. There
is intentionally **no foreign key** from `zeroship.sandboxes.user_id`
(or any sandbox table) back into `zeroship.users` / `zeroship.apps`. This
ADR records why that decoupling is deliberate, not an oversight.

## Decision

**Keep the two identity spaces decoupled. No FK bridges the sandbox
typed-id world to the platform UUID world.**

Concretely:

1. Sandbox tables validate identity *shape* with `CHECK (… ~ '^usr_…')`
   regexes, not referential integrity. A sandbox row can exist whose
   `user_id` has no matching `zeroship.users` row, and that is allowed by
   construction.

2. The sandbox controller (`crates/sandbox`) is the sole writer/owner of
   the sandbox tables, reachable through its own pooled roles
   (`sandbox_admin` / `sandbox_app` / `sandbox_audit` / `sandbox_gdpr`,
   `0011` § 13.2). It never joins to `zeroship.users` and holds no grant on
   it.

3. The `prj_` project id is *derived deterministically* from the platform
   app/thread id, not stored as an FK — see
   `docs/decisions/2026-05-26-builder-sandbox-project-typed-id.md`. The
   builder re-derives the same `prj_` for the same project across restarts,
   so `(user_id, project_id)` dedup works without a mapping table or a
   cross-namespace FK.

### GDPR erasure is role-scoped DELETE, not FK cascade

Because there is no FK, deleting a `zeroship.users` row does **not**
cascade into the sandbox tables. User erasure is instead driven explicitly
by the sandbox controller's GDPR path:

- `DELETE /admin/users/{user_id}` →
  `crates/sandbox/src/admin_handlers.rs::delete_user`, which runs **one
  transaction on the `sandbox_gdpr` role pool** (`pool_gdpr()`).
- The TX issues scoped, typed-id-keyed deletes:
  `DELETE FROM zeroship.sandbox_events WHERE user_id = $1::TEXT`, then
  `zeroship.shares` (by the user's sandbox ids), then a tombstone
  `INSERT … zeroship.deleted_sandboxes`, then
  `DELETE FROM zeroship.sandboxes WHERE user_id = $1::TEXT`, and finally an
  in-TX `gdpr.delete_user` audit row into `zeroship.sandbox_events`.
- `sandbox_gdpr` is granted *exactly* `SELECT, DELETE` on the four sandbox
  tables plus `INSERT` on `deleted_sandboxes` / `sandbox_events` (`0011`
  § 13.2) — the minimal surface to perform and audit erasure. It cannot
  touch any platform table.

So the platform-side "delete the user" action and the sandbox-side erasure
are **two coordinated operations against two namespaces**, not a single FK
cascade. The seam is honored by an application-level call into the sandbox
GDPR endpoint, scoped by the `sandbox_gdpr` role.

## Rationale

- **Service ownership / least privilege.** The sandbox controller owns its
  tables behind its own role split. A cross-schema FK would force the
  platform's deletion path to either hold write authority over sandbox
  rows or depend on the sandbox schema's existence — both violate the
  "sandbox controller is the sole writer" boundary and the role isolation
  in `0011` § 13.2.

- **Independent lifecycles & deployability.** Sandbox state is operational
  (sandboxes start/stop/get reaped; events are partitioned and aged out by
  the `ensure_event_partitions` sweep). Platform identity is durable. An FK
  with `ON DELETE RESTRICT` would let live sandbox rows block a user
  delete; `ON DELETE CASCADE` would silently destroy auditable sandbox
  history (including the `gdpr.delete_user` records that exist *for* Art. 30
  accountability). Neither is desirable. Erasure must be an explicit,
  audited action, which the role-scoped DELETE path gives us.

- **Encoding mismatch is real, not incidental.** The sandbox world speaks
  typed-id base62 TEXT end-to-end (HTTP boundary validation, dedup key,
  sealed-record filenames, audit payloads). Storing the raw platform UUID
  instead — purely to satisfy an FK — would split the sandbox's own
  identity representation and require a decode at every boundary. Keeping
  `usr_…` TEXT throughout keeps the controller stateless w.r.t. the
  platform schema.

- **Auditability over referential integrity.** GDPR here wants a *record
  that erasure happened* (`gdpr.delete_user` event, `deleted_sandboxes`
  tombstone), survivable independent of whether the platform user row still
  exists. An FK cascade produces silent deletion with no trace; the scoped
  DELETE path produces erasure *plus* an audit trail in the same TX.

## Consequences

- A sandbox `user_id` is *not* guaranteed to resolve to a live
  `zeroship.users` row. Consumers must not assume a JOIN-able relationship;
  treat the sandbox `user_id` as an opaque scoped key.

- User erasure is a **two-step protocol**: the platform deletes its UUID
  rows (its own FK cascade), and *separately* calls the sandbox
  `DELETE /admin/users/{user_id}` so the `sandbox_gdpr` role erases the
  typed-id rows. Orchestration that forgets the second step leaves sandbox
  rows behind — there is no FK to catch it. This is the price of the
  decoupling and is accepted deliberately.

- Cross-namespace correlation (e.g. operator export) is done by passing the
  `usr_…` id into the sandbox admin surface, never by SQL JOIN across the
  UUID/typed-id boundary.

## Notes

- The inline seam comment lives at `0011_sandbox_initial.sql` lines
  ~132–140 (on `zeroship.sandbox_events`); this ADR is the long-form
  rationale that comment points readers to.
- Related: `docs/decisions/2026-05-26-builder-sandbox-project-typed-id.md`
  (deterministic `prj_` derivation), `2026-05-05-sandbox-admin-shared-bearer.md`
  (admin bearer / `admin_id = "operator"` placeholder on the audit row).
- typed-id definition: `crates/core/src/typed_id.rs`.
