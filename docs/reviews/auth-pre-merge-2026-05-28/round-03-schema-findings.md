# Round 3 — Schema & migrations: findings

Total: 10 findings (0 critical, 5 high, 4 medium, 1 low).

## CRITICAL

None.

## HIGH

### H1. User deletion does not reliably clean auth session/token state
**File:** `crates/auth/src/store/migrations.rs:41-64`, `crates/auth/src/store/migrations.rs:139-180`

**Severity rationale:** `auth.sessions.user_id` references `auth.users(id)` without `ON DELETE CASCADE`, so deleting a user is blocked by their IdP sessions instead of cleaning them up. `auth.gateway_sessions.user_id` and `auth.console_sessions.user_id` are `TEXT` with no FK, so they cannot cascade and will orphan unless every delete path manually removes them. Password reset tokens are stored in `auth.magic_links` by `email` only, so there is no user FK to clean reset/login links when an account is deleted.

**Reproducer:**
```sql
INSERT INTO auth.users (id, email, name) VALUES
  ('00000000-0000-0000-0000-000000000001', 'delete@example.com', 'Delete Me');
INSERT INTO auth.sessions (user_id, auth_method, amr, idle_expires_at, abs_expires_at)
VALUES ('00000000-0000-0000-0000-000000000001', 'password', ARRAY['pwd'], NOW() + INTERVAL '1 hour', NOW() + INTERVAL '1 day');
DELETE FROM auth.users WHERE id = '00000000-0000-0000-0000-000000000001';
-- Fails due auth.sessions FK. gateway_sessions/console_sessions/magic_links have no FK to enforce cleanup.
```

**Suggested fix:** Make `auth.sessions.user_id` `REFERENCES auth.users(id) ON DELETE CASCADE`. Store gateway/console session `user_id` as UUID FKs, or add a deliberate database-enforced cleanup model. Bind password reset/login token rows to `user_id` where possible, or split reset tokens into a user-keyed table with `ON DELETE CASCADE`.

### H2. User/owner-scoped hot paths are missing supporting indexes
**File:** `crates/auth/src/store/migrations.rs:29-51`, `crates/auth/src/store/migrations.rs:94-101`, `crates/auth/src/store/migrations.rs:139-153`, `crates/auth/src/store/migrations.rs:215-229`, `crates/control/src/token_handlers.rs:276-287`

**Severity rationale:** Several tables that will exceed 1k rows are queried by `user_id` or `owner_id`, but the migrations only create primary-key, partial, or differently ordered indexes. PostgreSQL does not create indexes for FK columns. These paths become sequential scans during normal account operations and incident flows.

**Reproducer:**
```sql
-- No auth.identities(user_id, linked_at) index for:
SELECT id, user_id, provider, subject, email_at_link::text
FROM auth.identities WHERE user_id = $1 ORDER BY linked_at;

-- No auth.sessions(user_id) index for password reset session revocation:
DELETE FROM auth.sessions WHERE user_id = $1;

-- auth_gateway_sessions_app_idx is (app_id, user_id), so this user-only path cannot use it:
UPDATE auth.gateway_sessions SET revoked_at = NOW()
WHERE user_id = $1 AND revoked_at IS NULL;

-- No auth.email_verifications(user_id) active index for:
UPDATE auth.email_verifications SET consumed_at = NOW()
WHERE user_id = $1 AND consumed_at IS NULL;

-- permission_tokens_owner_active_idx is partial on revoked_at IS NULL,
-- so it cannot support the all-token list path:
SELECT id, name, created_at, expires_at, last_used_at, revoked_at
FROM control.permission_tokens
WHERE owner_id = $1 AND kind = 'pat'
ORDER BY created_at DESC, id DESC;
```

**Suggested fix:** Add indexes matching the predicates, for example `auth.identities(user_id, linked_at)`, `auth.sessions(user_id)`, `auth.gateway_sessions(user_id) WHERE revoked_at IS NULL`, `auth.email_verifications(user_id) WHERE consumed_at IS NULL`, and `control.permission_tokens(owner_id, kind, created_at DESC, id DESC)`.

### H3. One-active-token invariants are application-only and race under concurrent issue
**File:** `crates/auth/src/store/migrations.rs:54-68`, `crates/auth/src/store/migrations.rs:94-101`

**Severity rationale:** Magic login, password reset, and email verification issuance all assume "consume old active rows, then insert new row." There is no partial unique constraint enforcing one active row. Two concurrent requests can both run the `UPDATE` before either `INSERT`, leaving multiple unconsumed valid tokens for the same email/purpose or user.

**Reproducer:**
```text
T1: UPDATE auth.magic_links SET consumed_at = NOW()
    WHERE email = 'a@example.com'::citext AND purpose = 'reset' AND consumed_at IS NULL; -- 0 rows
T2: UPDATE auth.magic_links SET consumed_at = NOW()
    WHERE email = 'a@example.com'::citext AND purpose = 'reset' AND consumed_at IS NULL; -- 0 rows
T1: INSERT INTO auth.magic_links (... email='a@example.com', purpose='reset', consumed_at=NULL);
T2: INSERT INTO auth.magic_links (... email='a@example.com', purpose='reset', consumed_at=NULL);
-- Both reset tokens are valid.
```

**Suggested fix:** Add DB constraints for the invariants and make issuance one statement or a transaction that handles conflicts. Examples: `CREATE UNIQUE INDEX ... ON auth.magic_links(email, purpose) WHERE consumed_at IS NULL` and `CREATE UNIQUE INDEX ... ON auth.email_verifications(user_id) WHERE consumed_at IS NULL`.

### H4. Stripe relink can create multiple open history rows
**File:** `crates/control/src/stripe_store.rs:107-135`, `crates/control/src/stripe_store.rs:391-413`, `crates/control/src/registry.rs:239-252`

**Severity rationale:** `link_account` checks the current live binding outside the transaction, then the transaction closes any open history row, inserts a new open history row, and upserts the live row. Concurrent relinks for the same creator can both insert an `unlinked_at IS NULL` history row because the schema has no partial unique constraint for one open history span per creator.

**Reproducer:**
```text
T1: SELECT current account for creator C -> acct_old
T2: SELECT current account for creator C -> acct_old
T1: BEGIN; UPDATE creator_account_history SET unlinked_at = NOW() WHERE creator_id=C AND unlinked_at IS NULL; INSERT history acct_A; ...
T2: BEGIN; same UPDATE now affects 0 rows; INSERT history acct_B; ...
-- creator_account_history now has two open rows for C.
```

**Suggested fix:** Add `CREATE UNIQUE INDEX ... ON creator_account_history(creator_id) WHERE unlinked_at IS NULL`, then serialize relinks by locking the live `creator_accounts` row (`SELECT ... FOR UPDATE`) or by using one upsert/CTE that closes history and opens the new row under the same locked key.

### H5. Builder OAuth bootstrap has a multi-node check-then-insert race
**File:** `crates/control/src/bootstrap_builder.rs:103-119`, `crates/control/src/bootstrap_builder.rs:131-168`

**Severity rationale:** On concurrent control-plane startup, two nodes can both observe the builder OAuth client as absent. Hydra creation treats HTTP 409 as success, but the local DB insert is a plain `INSERT`. The loser hits the `control.oauth_clients` primary-key violation and reports bootstrap failure even though the desired state now exists.

**Reproducer:**
```text
Node A: SELECT 1 FROM control.oauth_clients WHERE client_id='zeroship-builder' -> none
Node B: SELECT 1 FROM control.oauth_clients WHERE client_id='zeroship-builder' -> none
Node A: creates Hydra client, INSERT succeeds
Node B: Hydra returns 409 accepted as Ok, INSERT fails duplicate key
```

**Suggested fix:** Make the local insert idempotent with `INSERT ... ON CONFLICT (client_id) DO NOTHING` and verify the existing row matches the expected trusted-builder shape. If the Hydra and DB bootstrap must be strictly serialized, take a Postgres advisory lock around the bootstrap sequence.

## MEDIUM

### M1. Control-plane tables are created in the default schema instead of `control`
**File:** `crates/control/src/registry.rs:97-330`, `crates/control/src/env_store.rs:158-178`, `crates/control/src/stripe_store.rs:109-184`

**Severity rationale:** `auth` migrations create `control.*` tables for OAuth/authz, but `registry.rs` creates core control-plane tables as unqualified `apps`, `usage`, `app_vars`, `app_secrets`, `creator_accounts`, `payouts`, and `app_audit`. Every query is also unqualified. That makes correctness depend on `search_path`, splits control data across schemas, and leaves room for table-name collisions or accidental reads from `public`.

**Reproducer:**
```sql
SET search_path = scratch, public;
CREATE TABLE scratch.apps (LIKE public.apps INCLUDING ALL);
-- Registry queries such as SELECT manifest_json FROM apps WHERE id = $1 now target scratch.apps.
```

**Suggested fix:** Move these tables to `control` pre-launch and qualify all SQL as `control.apps`, `control.app_vars`, `control.creator_accounts`, etc. Avoid relying on connection-level `search_path`.

### M2. Payout ledger invariants are not enforced by CHECK constraints
**File:** `crates/control/src/registry.rs:266-278`, `crates/control/src/stripe_store.rs:249-262`

**Severity rationale:** The Rust call path validates non-negative amounts and `platform_fee <= gross_amount`, but the ledger table accepts impossible financial rows from any other DB writer, test helper, or future code path. Financial invariants should live in the database too.

**Reproducer:**
```sql
INSERT INTO creator_accounts (creator_id, stripe_account_id)
VALUES ('00000000-0000-0000-0000-000000000002', 'acct_123456789012');
INSERT INTO payouts
  (creator_id, event_id, event_type, gross_amount, platform_fee, net_amount, currency, occurred_at)
VALUES
  ('00000000-0000-0000-0000-000000000002', 'evt_bad', 'charge.succeeded', 100, 500, -400, 'usd', NOW());
-- Accepted by schema; impossible ledger row.
```

**Suggested fix:** Add checks such as `gross_amount >= 0`, `platform_fee >= 0`, `net_amount >= 0`, `platform_fee <= gross_amount`, `net_amount = gross_amount - platform_fee`, and a constrained currency shape.

### M3. Creator/app ownership relationships are not database-enforced
**File:** `crates/control/src/registry.rs:216-245`, `crates/auth/src/store/migrations.rs:206-214`

**Severity rationale:** `creator_accounts.creator_id` is documented as today's auth user ID but has no FK to `auth.users`. `creator_account_history.creator_id` has no FK to either `creator_accounts` or `auth.users`. `control.app_members.app_id` is `TEXT`, while `apps.id` is UUID and lives outside the `control` schema, so app deletion cannot cascade memberships. These gaps allow orphaned creator accounts, history rows, and app memberships.

**Reproducer:**
```sql
INSERT INTO auth.users (id, email, name)
VALUES ('00000000-0000-0000-0000-000000000001', 'member@example.com', 'Member');
INSERT INTO creator_account_history (creator_id, stripe_account_id)
VALUES ('00000000-0000-0000-0000-00000000dead', 'acct_123456789012');
INSERT INTO control.app_members (app_id, user_id, role)
VALUES ('not-an-app', '00000000-0000-0000-0000-000000000001', 'owner');
-- Both child rows are accepted; no app/creator parent is required.
```

**Suggested fix:** Decide the intended deletion semantics and encode them. If payout history must block account deletion, use explicit `ON DELETE RESTRICT` FKs. If app memberships are app-owned, make `app_id` the same type as the app primary key and reference `control.apps(id) ON DELETE CASCADE`.

### M4. Token sweep predicates are not indexed for expired/consumed cleanup
**File:** `crates/auth/src/store/migrations.rs:54-101`, `crates/auth/src/cron/token_sweep.rs:70-87`, `crates/auth/src/cron/token_sweep.rs:92-115`

**Severity rationale:** The sweeper deletes old rows from `auth.magic_links`, `auth.magic_completions`, and `auth.email_verifications` by `expires_at` and `consumed_at`. `magic_completions` has a partial `expires_at` index for unconsumed rows only; `magic_links` and `email_verifications` have no expiry/consumed cleanup indexes at all. These tables are write-heavy and will grow quickly under normal login/reset traffic.

**Reproducer:**
```sql
EXPLAIN DELETE FROM auth.email_verifications
WHERE expires_at < NOW() - INTERVAL '7 days'
   OR consumed_at < NOW() - INTERVAL '7 days';
-- Sequential scan once the table grows.
```

**Suggested fix:** Add cleanup indexes that match the sweep predicates, for example partial indexes on `(expires_at)` for unconsumed rows and `(consumed_at)` where `consumed_at IS NOT NULL`. Consider separate statements instead of `OR` so each index is reliably usable.

## LOW

### L1. Migration idempotency and post-launch ALTER safety need tightening
**File:** `crates/auth/src/store/migrations.rs:11-17`, `crates/auth/src/store/migrations.rs:251`, `crates/control/src/registry.rs:114-130`, `crates/control/src/registry.rs:226-286`

**Severity rationale:** The migrations use `gen_random_uuid()` but create `uuid-ossp`, not `pgcrypto`; that is version-dependent and surprising. `ALTER TABLE control.oauth_clients ALTER COLUMN created_by DROP NOT NULL` is not guarded with `IF EXISTS` or a catalog check. Several `ALTER TABLE ... ADD COLUMN ... NOT NULL DEFAULT ...` statements are fine pre-launch but would take stronger locks on populated tables post-launch.

**Reproducer:**
```sql
-- On a Postgres install where gen_random_uuid() is not built in and pgcrypto is absent:
CREATE TABLE auth.users (id UUID PRIMARY KEY DEFAULT gen_random_uuid());
-- ERROR: function gen_random_uuid() does not exist
```

**Suggested fix:** Create `pgcrypto` explicitly or switch defaults to `uuid_generate_v4()` if `uuid-ossp` is the intended dependency. Guard non-`IF EXISTS` ALTERs with catalog checks. Before launch, collapse the table shapes so the migration list does not carry backfill-style ALTERs that are only needed for pre-launch dev databases.

## Areas reviewed and clean

- `auth.users`: email is `CITEXT UNIQUE NOT NULL`; primary-key and email lookups are indexed.
- `auth.email_suppressions`, `auth.rate_limits`, `auth.dpop_jti`, and `auth.cron_state`: queried by primary key with appropriate indexes for current call sites.
- `control.oauth_grants`: primary key and secondary client/user indexes match the grant list, load, touch, and revoke queries.
- `control.permission_tokens`: PAT validation and revoke paths are primary-key driven; the owner list index gap is covered in H2.
- `usage`, `app_vars`, `app_secrets`, and `app_env_expose`: current predicates are covered by composite primary keys.
- `app_audit` and `payouts`: current read paths have covering app/creator time indexes, aside from the ledger CHECK constraints noted above.

## Status

CLOSED:
- H1 — `329d5aa8` audit R3.H1: cascade auth session user deletion
- H2 — `3ff078f6` audit R3.H2: add auth hot path indexes
- H3 — `165d84d9` audit R3.H3: enforce one active auth token
- H4 — `2ba89d45` audit R3.H4: serialize stripe relink history
- H5 — `dd9c3a50` audit R3.H5: make builder oauth bootstrap idempotent
- M1 — `5a2ca2cf` audit R3.M1: qualify control schema tables
- M2 — `bfc1f0f6` audit R3.M2: enforce payout ledger checks
- M4 — `a1a9f04c` audit R3.M4: index auth token sweeps
- L1 — `6eebd137` audit R3.L1: tighten auth migration idempotency

DEFERRED:
- M3 — Creator/app ownership FKs. `control.app_members` is currently created by auth migrations, while its intended parent `control.apps` is created by control registry migrations. Adding the FK in auth would require duplicating or moving `control.apps` DDL across crate ownership boundaries. Defer until the authorization/control migration ownership is consolidated.
