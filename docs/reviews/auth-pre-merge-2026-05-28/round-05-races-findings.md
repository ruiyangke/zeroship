# Round 05 Race Conditions & Multi-Instance Hazards Findings

Read-only source audit. Source was not modified.

## Critical

### C1. Manual transaction runs on the globally shared auth PG session

- File: `crates/auth/src/ui/reset.rs:188`
- Related: `crates/auth/src/main.rs:94`, `crates/auth/src/server.rs:202`, `crates/auth/src/server.rs:225`
- Problem: `complete_password_reset` issues raw `BEGIN` / `COMMIT` on the same `Arc<compio_postgres::Client>` that is shared with all HTTP handlers and cron tasks. PostgreSQL transaction state is scoped to the physical session, not to the Rust future. Any concurrent handler that sends a query on that client while the reset transaction is open can run inside the reset transaction and be committed or rolled back by the reset flow.
- Reproducer:
  1. Start a password reset and pause after `conn.execute("BEGIN")` at `reset.rs:188`.
  2. On another request handled by the same process, complete `/login`; it calls `sessions::create` via the same `Arc<Client>`.
  3. Resume the reset path and force an error before `COMMIT`, for example in strict audit/session cleanup.
  4. The login response may have been built around a session insert that is later rolled back by the reset request. The reverse is also possible: unrelated auth writes can be committed as part of the reset transaction.
- Fix: never run raw `BEGIN` on the process-wide shared client. Use a dedicated connection for the whole transaction, a transaction API that owns/exclusively borrows the session until commit, or a small pool that checks out one unshared connection per transaction. Apply the same rule to any future auth/control transactional code.

## High

### H1. Old-password login can survive a concurrent password reset

- File: `crates/auth/src/ui/login.rs:241`
- Related: `crates/auth/src/ui/login.rs:327`, `crates/auth/src/ui/reset.rs:215`, `crates/auth/src/ui/reset.rs:229`
- Problem: login reads the current `password_hash`, verifies it off-thread, then later inserts a new session. Password reset updates the hash and deletes existing sessions, but it does not prevent a login that already read the old hash from creating a fresh session after the reset's session deletion. This race exists even after C1 is fixed by moving reset onto a dedicated connection.
- Reproducer:
  1. Request A starts `/login` with the old password and reaches the Argon2 verification after reading the old hash at `login.rs:241`.
  2. Request B redeems a reset token, updates the password at `reset.rs:215`, and deletes current sessions at `reset.rs:229`.
  3. Request A resumes and creates a new session at `login.rs:327` using the old credential.
  4. The reset response says sessions were revoked, but the old password login has a valid post-reset session.
- Fix: add a user session generation / credential version and stamp it into session rows, or lock/check the user row around credential verification and session creation. Reset should bump the generation atomically with the password change; session validation should reject rows with stale generation.

### H2. Concurrent correct cross-device magic completions are counted as wrong attempts and can consume the code before the winner finalizes

- File: `crates/auth/src/ui/magic.rs:1047`
- Related: `crates/auth/src/ui/magic.rs:1067`, `crates/auth/src/ui/magic.rs:1094`, `crates/auth/src/ui/magic.rs:1122`
- Problem: `completions_store::consume_pending` checks in-flight state with a SELECT before the reserving UPDATE. If multiple correct-code requests arrive while no reservation exists, they can all pass the SELECT. One reserves the row; the others fail the correct-code UPDATE after row recheck, then execute the wrong-code UPDATE, incrementing `attempts` even though their code was correct. Enough parallel correct submissions can set `consumed_at` before the first request calls `finalize_consume`.
- Reproducer:
  1. Create an `auth.magic_completions` row with `attempts = 0`, `consumed_pending_at IS NULL`, correct code `123456`.
  2. Send five concurrent `/magic/complete` POSTs with code `123456`.
  3. One request reserves the row. The other four can run the wrong-code branch at `magic.rs:1094`, raising attempts to 5 and setting `consumed_at`.
  4. The winning request may already have accepted Hydra login, but `finalize_consume` at `magic.rs:1122` then updates no rows.
- Fix: do the correct-code `UPDATE ... RETURNING` first, then if it returns no row, check for a current in-flight reservation. The wrong-code UPDATE must exclude active reservations with the same `consumed_pending_at IS NULL OR stale` predicate.

### H3. Consent accept records the local OAuth grant before Hydra accepts the challenge

- File: `crates/auth/src/ui/consent.rs:168`
- Related: `crates/auth/src/ui/consent.rs:190`, `crates/auth/src/ui/consent.rs:75`, `crates/auth/src/ui/consent.rs:86`
- Problem: both manual accept and first-party silent accept upsert `control.oauth_grants` before calling Hydra's consent accept endpoint. If Hydra later rejects the challenge, or if a concurrent deny wins at Hydra, the local DB still records an active grant. Future first-party fast-path checks use that local grant and can silently accept scopes the user just denied.
- Reproducer:
  1. Render one consent challenge in two browser tabs.
  2. Submit `/consent/accept` and `/consent/deny` concurrently.
  3. The accept path writes `control.oauth_grants` at `consent.rs:168`.
  4. If Hydra processes the deny first, `accept_consent` fails at `consent.rs:190`, but the local grant remains.
- Fix: only persist/touch the local grant after Hydra accept succeeds. If the DB write after Hydra success fails, compensate by revoking the Hydra consent session or make the flow retryable with an explicit pending/accepted state.

### H4. OAuth grant issue and revoke are not serialized across local DB and Hydra side effects

- File: `crates/control/src/oauth_grants_handlers.rs:94`
- Related: `crates/control/src/oauth_grants_handlers.rs:118`, `crates/auth/src/ui/consent.rs:271`
- Problem: revoke deletes the local `control.oauth_grants` row, then calls Hydra to revoke consent sessions. Consent accept independently upserts the same row and calls Hydra accept. There is no per `(user_id, client_id)` lock/state machine spanning the local row and the Hydra call. Depending on interleaving, the final local DB state and Hydra consent/session state can disagree.
- Reproducer:
  1. Start `/me/oauth-grants/{client_id}` DELETE and pause after the DB delete at `oauth_grants_handlers.rs:94`.
  2. Complete a new consent accept for the same `(user_id, client_id)`, which upserts the row at `consent.rs:271` and accepts in Hydra.
  3. Resume the revoke request; it revokes Hydra consent sessions at `oauth_grants_handlers.rs:118`.
  4. The DB now shows an active grant while Hydra has just revoked consent sessions. The opposite inconsistency is possible if revoke deletes after accept writes but before accept reaches Hydra.
- Fix: serialize all grant mutation for one `(user_id, client_id)` with an advisory lock or explicit grant-state row, and perform DB/Hydra side effects in one ordered critical section. Prefer idempotent states such as `active`, `revoking`, `revoked` over hard delete.

### H5. Concurrent token issuance violates the one-active-token invariant

- File: `crates/auth/src/identity/magic_link.rs:107`
- Related: `crates/auth/src/identity/magic_link.rs:116`, `crates/auth/src/identity/password_reset.rs:84`, `crates/auth/src/identity/password_reset.rs:105`, `crates/auth/src/identity/verification.rs:69`, `crates/auth/src/identity/verification.rs:84`
- Problem: magic-link, password-reset, and verification issuance all implement "invalidate old rows, then insert new row" as two independent statements without a transaction, advisory lock, or partial unique constraint. Two concurrent issuers for the same email/user can both run the UPDATE before either INSERT commits, leaving two unconsumed tokens valid.
- Reproducer:
  1. Send two concurrent magic-link requests for the same email.
  2. Both execute the superseding UPDATE at `magic_link.rs:107` and see no new row from the other request yet.
  3. Both insert at `magic_link.rs:116`.
  4. Both email links can be redeemed until one is consumed or expires. The same race exists for reset tokens and verification tokens in the related files.
- Fix: enforce the invariant in the database. Options: issue under a per-email/per-user advisory lock; store active token state in a single row and update it atomically; or add a partial unique index for active tokens and use one transaction that supersedes/creates deterministically.

### H6. Builder OAuth client bootstrap is SELECT-then-create across DB, filesystem, and Hydra

- File: `crates/control/src/bootstrap_builder.rs:103`
- Related: `crates/control/src/bootstrap_builder.rs:116`, `crates/control/src/bootstrap_builder.rs:117`, `crates/control/src/bootstrap_builder.rs:118`
- Problem: multi-instance control-plane boot checks `control.oauth_clients`, then creates/reads a local secret file, creates the Hydra client, and inserts the DB row. There is no DB advisory lock and the insert has no `ON CONFLICT`. Two instances can both observe "absent". With separate container filesystems they can also generate different client secrets; one Hydra create wins, the other treats 409 as success but may retain a local secret that Hydra never accepted, then fail the DB insert.
- Reproducer:
  1. Start two control-plane instances with builder bootstrap enabled and separate local `data/` directories.
  2. Both return false from `oauth_client_exists` at `bootstrap_builder.rs:103`.
  3. Instance A creates Hydra client with secret A and inserts DB metadata.
  4. Instance B generates secret B, receives Hydra 409 as success at `create_hydra_client`, then hits duplicate DB insert or boots with an invalid local secret.
- Fix: take a stable PostgreSQL advisory lock before the existence read and hold it through secret creation, Hydra create/update, and DB upsert. On Hydra 409, fetch the existing client and reconcile rather than assuming the local secret is valid. Use `INSERT ... ON CONFLICT DO UPDATE/NOTHING` for the metadata row.

## Medium

### M1. Stale magic-link reservations can finalize or clear a newer reservation

- File: `crates/auth/src/identity/magic_link.rs:201`
- Related: `crates/auth/src/identity/magic_link.rs:156`, `crates/auth/src/identity/magic_link.rs:218`
- Problem: `redeem_pending` lets a reservation be retried after 60 seconds, but `finalize_consume` and `clear_consume_pending` only predicate on `token_hash` and `consumed_pending_at IS NOT NULL`. They do not prove that the caller still owns the current reservation. A slow request whose reservation aged out can later finalize or clear a reservation created by a retrying request.
- Reproducer:
  1. Request A reserves a magic token at `magic_link.rs:156`, then stalls before `finalize_consume`.
  2. After 60 seconds, request B redeems the same token and gets a fresh reservation.
  3. Request A resumes and calls `finalize_consume` at `magic_link.rs:201` or an error path calls `clear_consume_pending` at `magic_link.rs:218`.
  4. A consumes or clears B's reservation.
- Fix: return a reservation nonce or the exact `consumed_pending_at` value from `redeem_pending`, and require `WHERE token_hash = $1 AND consumed_pending_at = $2` on finalize/clear. Apply the same ownership check to `auth.magic_completions` finalization.

### M2. Auth bootstrap client reconciliation can fail under simultaneous first boot

- File: `crates/auth/src/bootstrap/mod.rs:62`
- Related: `crates/auth/src/bootstrap/mod.rs:65`, `crates/auth/src/hydra_client/clients.rs:11`
- Problem: client reconciliation does `get_client`, then `create_client` or `update_client` with no lock and no 409 handling. Two auth instances starting against a fresh Hydra can both read missing; one creates the client and the other treats Hydra's duplicate response as a fatal bootstrap error.
- Reproducer:
  1. Start two auth instances at the same time with identical clients config and an empty Hydra client set.
  2. Both observe `None` at `bootstrap/mod.rs:62`.
  3. Instance A succeeds at `create_client`.
  4. Instance B calls the same `create_client` at `bootstrap/mod.rs:65`; Hydra returns duplicate/409 and the process fails startup.
- Fix: hold an advisory lock around client reconciliation or make create 409-tolerant by refetching and updating the existing client. The signing-key bootstrap already uses this shape; apply the same discipline to client bootstrap.

## Low

### L1. Control-plane rate limits are process-local, so multi-instance deployments multiply burst capacity

- File: `crates/control/src/rate_limit.rs:45`
- Related: `crates/control/src/main.rs:370`, `crates/control/src/main.rs:371`, `crates/control/src/http_util.rs:47`
- Problem: the control-plane admin and webhook rate limiters are in-memory `Mutex<HashMap<...>>` instances. This is atomic inside one process, but not across multiple control-plane instances. A load-balanced deployment gives each source IP one full bucket per instance; restarts also reset buckets.
- Reproducer:
  1. Run three control-plane instances behind a round-robin load balancer.
  2. Send admin or webhook traffic from one IP at three times the intended burst.
  3. Each instance accepts up to its own local bucket at `rate_limit.rs:57`.
- Fix: either move these limits to the DB-backed atomic bucket used by auth, Redis, or another shared store, or make LB-level rate limiting a hard deployment requirement with startup/config validation for production.

## Audited With No Finding

- JWK rotation: `crates/auth/src/cron/jwk_rotation.rs:114` takes a per-set advisory lock before reading `auth.cron_state` and holds it through Hydra retirement/rotation plus the `cron_state` write.
- JWK signing-key bootstrap: `crates/auth/src/bootstrap/keys.rs:29` holds the bootstrap advisory lock before reading Hydra JWKS and through missing-key creation.
- Auth DB-backed rate limiter: `crates/auth/src/store/ratelimit.rs:40` uses one `INSERT ... ON CONFLICT DO UPDATE ... RETURNING` statement; same-bucket attempts serialize on the row.
- Token sweeper and DPoP JTI sweeper: deletes are idempotent; concurrent sweepers can duplicate work but do not double-spend or widen token validity. `PgJtiCache` inserts use `ON CONFLICT DO NOTHING RETURNING`.
- Platform role grant/revoke and PAT delete: the role/PAT row mutations are single SQL statements. Concurrent grant/revoke is last-statement-wins, which is at least database-linearized; the remaining concern is product semantics, not an unprotected read-modify-write window.
- `std::sync::{Mutex,RwLock}`/`OnceLock` search: observed locks are short critical sections and are not held across `.await`. The mutable state concern that rises to a finding is the process-local control rate limiter above.
