# zeroship-sandbox

Pluggable sandbox-session controller for the zeroship platform. Each editor sandbox is one runtime — a Docker container, a Kubernetes Pod with libkrun, or a Nomad job running Cloud Hypervisor — driven through the [`Backend`](src/backend/mod.rs) abstraction.

The controller's responsibility statement: **own the sandbox lifecycle (create / stop / restore), own the in-memory + pg + sealed-record state machine, and forward signed creator-authed traffic to the in-VM agent.** The agent (`crates/sandbox-agent/`) runs file/exec ops; the controller does NOT.

## Crate layout

```
crates/sandbox/src/
├── auth.rs               # creator-side bearer auth (SANDBOX_TOKEN)
├── backend/              # docker / k8s / nomad-ch backends
├── config.rs             # SandboxConfig::from_env (~30 env vars)
├── db.rs                 # pg-backed non-secret state + migrations
├── files.rs              # file-tree / read-file / write-file plumbing
├── handlers.rs           # creator-facing HTTP handlers
├── admin_handlers.rs     # operator-facing /admin/* HTTP handlers
├── lib.rs                # AppState wiring, HA loops (heartbeat / takeover)
├── main.rs               # ntex bind + route table
├── metrics.rs            # prometheus-style counters
├── persist.rs            # XChaCha20-Poly1305 sealed records (signing key store)
├── preview.rs            # /preview/{port}/{path} HTTP forwarder
├── preview_share*.rs     # /share token mint/list/revoke
├── preview_ws.rs         # WebSocket-Upgrade forwarder
├── registry.rs           # in-memory SandboxRegistry
└── restore.rs            # restart-restore loop (pg-driven)
```

Migrations live alongside in [`migrations/`](migrations/); each is forward-only and idempotent.

## Phase-3 admin API

Operator-facing surface for cross-tenant queries + GDPR data-export / data-delete. See [`docs/proposals/sandbox-pg-state.md`](../../docs/proposals/sandbox-pg-state.md) § 13 for the design.

| Method | Path | Purpose |
| --- | --- | --- |
| GET | `/admin/sandboxes` | List all sandboxes (filters: `user_id`, `host_id`, `status`; pagination via `limit`/`offset`) |
| GET | `/admin/sandboxes/{id}` | Single sandbox detail (pg row + `in_memory` + agent-version placeholder) |
| GET | `/admin/users/{user_id}/sandboxes` | Per-user shortcut |
| GET | `/admin/users/{user_id}/shares` | Per-user share-token metadata |
| GET | `/admin/users/{user_id}/export` | GDPR data-export (one REPEATABLE READ TX; events capped at 10000) |
| DELETE | `/admin/users/{user_id}` | GDPR cascade delete + sealed-record unlink |
| GET | `/admin/hosts` | Controller fleet status |

### Auth (Phase 3 narrow)

A separate bearer-from-file token gates the admin API. Set:

```
SANDBOX_ADMIN_TOKEN_PATH=/etc/zeroship/admin-bearer  # mode 0o400
```

When unset → every endpoint 503s with `{"error":"admin api disabled"}`. Disable-by-default; operators opt in explicitly.

Wrong/missing bearer → 401. Correct bearer + pg disabled → 503 with `{"error":"pg integration disabled"}`.

The full § 13.8 shape (short-lived JWT + per-endpoint scopes + 2FA step-up + per-admin rate-limit + anomaly alarms) is Phase 5 / production hardening. Phase 3's bearer-from-file is intentionally narrow — the JWT verifier lives in `crates/control/`, and coupling the sandbox crate to it before that contract is finalized is premature. The audit-event actor is hard-coded as `"operator"` for v1; Phase 5 replaces it with the JWT's `admin_id` claim. See [`docs/decisions/2026-05-05-sandbox-admin-shared-bearer.md`](../../docs/decisions/2026-05-05-sandbox-admin-shared-bearer.md) for the trade-off.

#### Token rotation

The admin bearer is read **once at boot**. Rotating the token requires a **rolling restart of every controller replica** — there is no signal-based or file-watch-based reload. Update the secret entry that materializes `SANDBOX_ADMIN_TOKEN_PATH`, then roll each replica (`SIGTERM` → drain → reboot reads the new file). The boot-cache shape exists because per-request file reads were a DoS amplifier (Round-3 CRITICAL #3); the trade-off is non-zero-downtime rotation.

Migration plan (Phase 5):
1. Replace `admin_check` (bearer match) with a JWT verifier + scope check.
2. Replace `audit_admin_action`'s `"operator"` with the JWT's `admin_id`.
3. Add `WWW-Authenticate: Step-Up max_age=300` on 403 from destructive endpoints when `step_up` is missing/stale.

### GDPR delete operator workflow

The cascade DELETE does NOT touch the live runtime. Workflow:

1. Stop all of the user's sandboxes first (via creator-side `DELETE /sandboxes/{id}` or via a script reading from `GET /admin/users/{user_id}/sandboxes`).
2. Confirm `GET /admin/users/{user_id}/sandboxes` returns 0 active sandboxes.
3. Issue `DELETE /admin/users/{user_id}`. Response includes:
   - `sandboxes_tombstoned`
   - `shares_deleted`
   - `events_deleted` (count BEFORE the audit row is inserted)
   - `sealed_files_unlinked`
4. The audit row `events.kind = 'gdpr.delete_user'` is written inside the same TX as the cascade.

Idempotent for unknown / already-cleaned users (returns 200 with all-zero counts).

## 4-role pg auth model

Migration `0004_role_split_phase3.sql` tightens 0001's permissive grants per [`docs/proposals/sandbox-pg-state.md`](../../docs/proposals/sandbox-pg-state.md) § 13.2:

| Role | Capabilities |
| --- | --- |
| `sandbox_admin` | DDL on the `sandbox` schema (migrations only) |
| `sandbox_app` | `SELECT/INSERT/UPDATE/DELETE` on non-events tables; `SELECT/INSERT` on `events` (NO DELETE — the controller cannot tamper with its own audit log) |
| `sandbox_audit` | `INSERT`-only on `events` (audit pipe; will eventually run as a separate process) |
| `sandbox_gdpr` | `SELECT + DELETE` on cascade tables; `INSERT` on `events` + `deleted_sandboxes` (single-TX cascade audit) |

The controller selects per-role DSN via three env vars; each defaults to `SANDBOX_DATABASE_URL` for dev convenience. Production sets all three:

| Env var | Default | Purpose |
| --- | --- | --- |
| `SANDBOX_DATABASE_URL` | (none — pg disabled when unset) | Primary `sandbox_app` DSN |
| `SANDBOX_DATABASE_URL_AUDIT` | `SANDBOX_DATABASE_URL` | `sandbox_audit` DSN |
| `SANDBOX_DATABASE_URL_GDPR` | `SANDBOX_DATABASE_URL` | `sandbox_gdpr` DSN |

`Database::pool_app()` / `pool_audit()` / `pool_gdpr()` return per-role pools; the `_gdpr` pool is opened on demand inside `delete_user` and dropped at end-of-request, never cached.

### Verifying the role isolation in CI

The pg-gated tests in `tests/sandbox_pg_e2e.rs` promote each role to `LOGIN` against the local CI Postgres and exercise the capability matrix:

```
PG_TEST_URL='postgres://postgres:zeroship@localhost:5440/zeroship' \
    cargo test -p zeroship-sandbox --test sandbox_pg_e2e role_ -- \
    --ignored --test-threads=1
```

Six tests cover the 42501 (insufficient_privilege) responses for forbidden ops + the success case for permitted ops.

## Tests

```bash
# unit tests (~177)
cargo test -p zeroship-sandbox --lib

# integration tests (no pg, ~51 active)
cargo test -p zeroship-sandbox --tests

# pg-gated tests (~46; needs Postgres)
docker compose up -d postgres
PG_TEST_URL='postgres://postgres:zeroship@localhost:5440/zeroship' \
    cargo test -p zeroship-sandbox --tests -- --ignored --test-threads=1
```

## See also

- [`docs/proposals/sandbox-pg-state.md`](../../docs/proposals/sandbox-pg-state.md) — full pg-state design (Draft v9)
- [`docs/runbooks/sandbox-nomad-ch.md`](../../docs/runbooks/sandbox-nomad-ch.md) — operator runbook for nomad-ch backend
- [`docs/architecture/runtime.md`](../../docs/architecture/runtime.md) — V8 runtime design (the agent's runtime context)
