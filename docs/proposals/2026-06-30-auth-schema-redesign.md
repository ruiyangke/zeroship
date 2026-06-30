# Auth schema redesign: canonical naming taxonomy

**Status:** proposal, design-only. No migration code in this change.
**Date:** 2026-06-30.
**Scope:** auth, identity, OAuth, session, grant, token, and client tables currently created by `db/migrations/V*.sql` excluding `*.down.sql`.

This is a pre-launch clean redesign. P1 should implement the final names directly; do not add compatibility aliases, migration shims, or old-name fallbacks.

## Schema decision (operator)

Use one platform schema: `zeroship`. Do not split auth, control, or gateway state into separate PostgreSQL schemas. Isolation comes from the existing per-service roles (`zeroship_auth`, `zeroship_control`, `zeroship_gateway`, `zeroship_worker`, `zeroship_app`), explicit per-table/column GRANTs, and RLS on tenant-scoped rows.

The rationale is pragmatic: newly-created tables are already default-deny at the table level because the platform must not use blanket `GRANT ON ALL TABLES` or broad `ALTER DEFAULT PRIVILEGES`. A separate schema would add a `USAGE`-level defense-in-depth layer, but that is not worth the churn, `search_path` complexity, and migration noise given the established single-schema + RLS practice already used for secret-bearing tables such as `zeroship.app_secrets`.

Signing-key custody is independent of that schema decision. The raw OP private signing key is not stored plaintext in Postgres; it lives in `AUTH_SIGNING_KEY_FILE` or a KMS-equivalent secret source, mirroring the gateway's `GATEWAY_SIGNING_KEY_FILE` pattern.

## 1. Naming conventions

1. **The schema is always `zeroship`.** Table and column names express concepts; role grants and RLS express write authority, read authority, and tenant isolation.
2. **Table names are plural snake_case nouns.** Keep current table names when they already communicate the concept. Rename only when a name is misleading, duplicated, singular, stale, or tied to Hydra/GoTrue implementation detail.
3. **The canonical platform-user FK is `user_id`.** Every database column that references the platform user row is named `user_id` and FKs to `zeroship.users(id)`. The audit found no separate database-backed operator/principal population: current `principal_id`, `global_user_id`, and control-plane bearer `subject` paths all resolve to `zeroship.users(id)`.
4. **Keep protocol subject names only for projected or external subjects.** `pairwise_sub`, `provider_subject`, `token_subject`, `jti`, `kid`, `amr`, `acr`, and `auth_time` are allowed because they are protocol claims or projected identifiers, not platform-user FKs.
5. **FK columns use `_id`.** Actor columns include the target noun: `created_by_user_id`, `granted_by_user_id`. Hashed secrets use `_hash`; encrypted secrets use `_enc`.
6. **Timestamps use `_at` or established expiry wording.** Use `created_at`, `updated_at`, `revoked_at`, `expires_at`, `idle_expires_at`, `absolute_expires_at`. Rename ambiguous non-`_at` current names such as `locked_until` and `deletion_scheduled_for`.
7. **OAuth grant names are disambiguated by meaning.** `oauth_grants` is the end-user OAuth consent grant. `principal_grants` is deploy/control authorization. `device_authorizations` is the RFC 8628-style pending device authorization. `app_net_grants` is operator-granted network egress.
8. **Drop provider implementation framing.** `oauth_clients` is no longer a Hydra mirror after the OP build. It is the authoritative client registry written by control and enforced by the platform AS.

## 2. Isolation model: one schema, per-role grants/RLS

Current ground truth: `V0001__extensions_schemas.sql` creates one `zeroship` schema and all hand-authored platform tables currently land there. `V0027__oauth_hydra_schema.sql` creates the vendor-owned `oauth_hydra` schema/role only; Hydra's own tables are not hand-authored in this repo and are retired by the OP replacement.

Final layout: every table in this redesign lives under `zeroship.*`. Service ownership is still explicit, but it is encoded as GRANT policy rather than schema placement:

- `zeroship_auth` owns OP/IdP and identity authority rows: global users, upstream identity links, auth factors, IdP sessions, OAuth grants, token state, signing-key metadata, and per-app pairwise identity projections.
- `zeroship_control` owns creator/platform control rows: app CRUD references, authoritative OAuth client registry writes, declared scope registry, deploy/PAT authorization, device authorization approval, and operator network grants.
- `zeroship_gateway` owns edge browser session rows and reload-recovery anchors. It does not get direct grants on OP bearer-token stores or signing-key metadata.
- `zeroship_worker` and `zeroship_app` do not get direct grants to OP/control/gateway tables. They access platform state through native primitives and service APIs.

Grant matrix for the redesigned auth/OAuth/session tables:

| Table(s) | `zeroship_auth` | `zeroship_control` | `zeroship_gateway` | `zeroship_worker` | `zeroship_app` | RLS/notes |
| --- | --- | --- | --- | --- | --- | --- |
| `zeroship.users` | read/write | read needed profile/account fields | no direct grant | no grant | no grant | User/account data stays service-mediated. Add RLS only for any future direct self-service role. |
| `zeroship.federated_identities`, `zeroship.idp_sessions`, `zeroship.magic_links`, `zeroship.magic_completions`, `zeroship.email_verifications`, `zeroship.email_suppressions`, `zeroship.totp_credentials`, `zeroship.totp_backup_codes` | read/write | no grant by default; narrow read only for explicit admin tooling | no grant | no grant | no grant | Credential, proof, and login-flow state is auth-private. |
| `zeroship.app_user_identities` | read/write | read for admin/debug only | narrow read/write only if gateway owns pairwise-session projection in implementation | no grant | no grant | Tenant scoped by `client_id`/app ownership; RLS mirrors `zeroship.app_secrets` style when exposed outside the auth service. |
| `zeroship.oauth_clients` | read; read client-secret verifier column only if auth verifies confidential clients directly | read/write; owns registration | no grant | no grant | no grant | Client-secret material/verifier columns are explicitly not granted to gateway, worker, or app. Use column-level grants or a redacted view for non-secret metadata if a service later needs it. |
| `zeroship.app_oauth_clients`, `zeroship.app_scope_defs` | read | read/write | no direct grant by default | no grant | no grant | Tenant scoped by `app_id`; use RLS for any non-control direct reads. |
| `zeroship.oauth_grants` | read/write | read for support/admin and client lifecycle cleanup only | no grant | no grant | no grant | Secret-bearing consent/token authority table. Tenant scoped by `(user_id, client_id)`; RLS required before any non-auth write. |
| `zeroship.oauth_authorization_codes` | read/write | no grant | no grant | no grant | no grant | Secret-bearing single-use code store; auth-private. |
| `zeroship.oauth_refresh_tokens` | read/write | no grant except lifecycle purge jobs run as auth or a tightly scoped maintenance role | no grant | no grant | no grant | Secret-bearing refresh-token family store; auth-private. |
| `zeroship.signing_keys` | read/write metadata | read public metadata only if control needs operator visibility | no grant | no grant | no grant | Contains public JWK and rotation status only. Raw private key custody is file/KMS, not a DB grant problem. |
| `zeroship.token_revocations`, `zeroship.dpop_jtis`, `zeroship.rate_limits`, `zeroship.cron_state` | read/write | no grant by default | no grant by default | no grant | no grant | Token-security and auth housekeeping state. Prefer service API checks over direct cross-service reads. |
| `zeroship.audit_events` | insert/read | read for audit/admin | no grant | no grant | no grant | Append-only trigger stays. RLS or redacted views before exposing tenant-filtered audit data. |
| `zeroship.permission_tokens`, `zeroship.principal_grants`, `zeroship.device_authorizations`, `zeroship.app_net_grants` | narrow read/write only for OAuth/device handoff paths | read/write | no grant | no grant | no grant | Control-plane authorization state; RLS by `user_id` or `app_id` where tenant-scoped. |
| `zeroship.sessions`, `zeroship.app_session_anchors` | narrow revocation/read as needed | no grant by default | read/write | no grant | no grant | Gateway-owned browser session credentials; tenant scoped by `app_id`; RLS required if auth/control get direct cleanup writes. |

The secret-bearing OP tables are intentionally not granted to `zeroship_worker`, `zeroship_app`, or `zeroship_gateway`: `zeroship.oauth_refresh_tokens`, `zeroship.oauth_authorization_codes`, `zeroship.signing_keys`, `zeroship.oauth_grants`, and the client-secret/verifier column(s) on `zeroship.oauth_clients`. `zeroship_auth` gets the OP tables; `zeroship_control` gets only the reads or writes its control-plane lifecycle requires.

### Signing-key custody requirement

`crates/auth` loads the raw OP private signing key from `AUTH_SIGNING_KEY_FILE` at boot. The file contains PEM/PKCS#8 key material and should be provisioned like `GATEWAY_SIGNING_KEY_FILE`, or replaced by a KMS-backed equivalent with the same runtime custody boundary.

`zeroship.signing_keys` is a registry, not the crown-jewel key store. It stores only `kid`, `alg`, public JWK, status, and rotation timestamps. If a future multi-node design requires persisted private-key material for rotation coordination, the persisted value must be encrypted at rest by a key from the file/KMS custody path; it must never be plaintext and must never be readable by any database role as usable private key material.

## 3. The redesigned tables

Each entry lists the audited creating migration, current purpose from comments, final table, final key columns, and final PK/FKs. Column types/nullability/defaults are grounded in the current SQL unless marked as a P1 OP addition.

### `zeroship.users`

Old: `zeroship.users` -> `zeroship.users`.
Creating/altering migrations: `V0002__auth.sql`, `V0030__auth_failed_login_count.sql`, `V0034__auth_account_deletion.sql`.
Purpose: global platform user pool; all auth/control identity joins target this row.

Final columns: `id UUID NOT NULL DEFAULT gen_random_uuid()`, `email CITEXT NOT NULL UNIQUE`, `email_verified_at TIMESTAMPTZ NULL`, `name TEXT NOT NULL`, `avatar_url TEXT NULL`, `password_hash TEXT NULL`, `credential_version BIGINT NOT NULL DEFAULT 0`, `lock_expires_at TIMESTAMPTZ NULL` (old `locked_until`), `disabled_at TIMESTAMPTZ NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `last_login_at TIMESTAMPTZ NULL`, `failed_login_count INTEGER NOT NULL DEFAULT 0`, `deletion_requested_at TIMESTAMPTZ NULL`, `deletion_scheduled_at TIMESTAMPTZ NULL` (old `deletion_scheduled_for`), `anonymized_at TIMESTAMPTZ NULL`.

PK/FKs: PK `id`. No FKs.

### `zeroship.federated_identities`

Old: `zeroship.federated_identities` + `zeroship.identity_links` -> `zeroship.federated_identities`.
Creating migrations: `V0002__auth.sql`, `V0060__identity_links_principal_grants.sql`.
Purpose: one external provider-local subject bound to one platform user. This merges the duplicate control-plane identity bridge into the existing auth identity-link table.

Final columns: `id UUID NOT NULL DEFAULT gen_random_uuid()`, `user_id UUID NOT NULL`, `provider TEXT NOT NULL`, `provider_subject TEXT NOT NULL` (old `federated_identities.subject`; same as `identity_links.provider_subject`), `email_at_link CITEXT NULL` (old `identity_links.email` merged here), `raw_profile JSONB NULL`, `linked_at TIMESTAMPTZ NOT NULL DEFAULT NOW()` (old `identity_links.created_at`).

PK/FKs: PK `id`; FK `user_id -> zeroship.users(id) ON DELETE CASCADE`; unique `(provider, provider_subject)`.

### `zeroship.idp_sessions`

Old: `zeroship.idp_sessions` -> `zeroship.idp_sessions`.
Creating migration: `V0002__auth.sql`.
Purpose: auth service IdP login session for the `__Host-zsidp_session` cookie.

Final columns: `id UUID NOT NULL DEFAULT gen_random_uuid()`, `user_id UUID NOT NULL`, `auth_method TEXT NOT NULL`, `amr TEXT[] NOT NULL`, `acr TEXT NULL`, `auth_time TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `credential_version BIGINT NOT NULL DEFAULT 0`, `idle_expires_at TIMESTAMPTZ NOT NULL`, `absolute_expires_at TIMESTAMPTZ NOT NULL` (old `abs_expires_at`), `revoked_at TIMESTAMPTZ NULL`.

PK/FKs: PK `id`; FK `user_id -> zeroship.users(id) ON DELETE CASCADE`.

### `zeroship.magic_links`

Old: `zeroship.magic_links` -> `zeroship.magic_links`.
Creating/altering migrations: `V0002__auth.sql`, `V0029__auth_magic_links_user_id.sql`.
Purpose: magic-link login tokens and password-reset tokens.

Final columns: `token_hash BYTEA NOT NULL`, `email CITEXT NOT NULL`, `user_id UUID NULL`, `csrf_nonce TEXT NOT NULL`, `purpose TEXT NOT NULL`, `request_ip INET NULL`, `request_user_agent TEXT NULL` (old `request_ua`), `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `expires_at TIMESTAMPTZ NOT NULL`, `consumed_pending_at TIMESTAMPTZ NULL`, `consumed_at TIMESTAMPTZ NULL`.

PK/FKs: PK `token_hash`; FK `user_id -> zeroship.users(id) ON DELETE CASCADE`. Keep email index and user index under new names.

### `zeroship.magic_completions`

Old: `zeroship.magic_completions` -> `zeroship.magic_completions`.
Creating migration: `V0002__auth.sql`.
Purpose: cross-device magic-link completion handshakes.

Final columns: `csrf_nonce TEXT NOT NULL`, `code TEXT NOT NULL`, `email CITEXT NOT NULL`, `login_challenge TEXT NOT NULL`, `attempts SMALLINT NOT NULL DEFAULT 0`, `expires_at TIMESTAMPTZ NOT NULL`, `consumed_pending_at TIMESTAMPTZ NULL`, `consumed_at TIMESTAMPTZ NULL`.

PK/FKs: PK `csrf_nonce`; no FKs.

### `zeroship.email_verifications`

Old: `zeroship.email_verifications` -> `zeroship.email_verifications`.
Creating migration: `V0002__auth.sql`.
Purpose: email-verification tokens.

Final columns: `token_hash BYTEA NOT NULL`, `user_id UUID NOT NULL`, `email CITEXT NOT NULL`, `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `expires_at TIMESTAMPTZ NOT NULL`, `consumed_at TIMESTAMPTZ NULL`.

PK/FKs: PK `token_hash`; FK `user_id -> zeroship.users(id) ON DELETE CASCADE`.

### `zeroship.email_suppressions`

Old: `zeroship.email_suppressions` -> `zeroship.email_suppressions`.
Creating migration: `V0002__auth.sql`.
Purpose: auth mailer bounce/complaint suppression list.

Final columns: `email CITEXT NOT NULL`, `reason TEXT NOT NULL`, `suppressed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `provider_message TEXT NULL` (old `provider_msg`).

PK/FKs: PK `email`; no FKs.

### `zeroship.totp_credentials`

Old: `zeroship.totp_credentials` -> `zeroship.totp_credentials`.
Creating migration: `V0035__auth_totp_2fa.sql`.
Purpose: per-user TOTP shared secret, encrypted at rest.

Final columns: `user_id UUID NOT NULL`, `encrypted_secret BYTEA NOT NULL`, `confirmed_at TIMESTAMPTZ NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`.

PK/FKs: PK/FK `user_id -> zeroship.users(id) ON DELETE CASCADE`.

### `zeroship.totp_backup_codes`

Old: `zeroship.totp_backup_codes` -> `zeroship.totp_backup_codes`.
Creating migration: `V0035__auth_totp_2fa.sql`.
Purpose: single-use TOTP recovery codes.

Final columns: `id BIGINT GENERATED ALWAYS AS IDENTITY`, `user_id UUID NOT NULL`, `code_hash TEXT NOT NULL`, `used_at TIMESTAMPTZ NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`.

PK/FKs: PK `id`; FK `user_id -> zeroship.users(id) ON DELETE CASCADE`.

### `zeroship.app_user_identities`

Old: `zeroship.app_user_identities` -> `zeroship.app_user_identities`.
Creating migration: `V0009__auth_app_user_identities.sql`.
Purpose: per-app pairwise and relay identity mapping for `(client_id, user_id)`.

Final columns: `client_id TEXT NOT NULL` (old `app_client_id`), `user_id UUID NOT NULL` (old `global_user_id`), `pairwise_sub TEXT NOT NULL`, `relay_email TEXT NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `revoked_at TIMESTAMPTZ NULL`.

PK/FKs: PK `(client_id, user_id)`; FK `client_id -> zeroship.oauth_clients(client_id) ON DELETE CASCADE`; FK `user_id -> zeroship.users(id) ON DELETE CASCADE`; index `pairwise_sub`; active unique index on `relay_email` where non-null and `revoked_at IS NULL`.

### `zeroship.oauth_grants`

Old: `zeroship.oauth_grants` -> `zeroship.oauth_grants`.
Creating migration: `V0004__control.sql`.
Purpose: end-user OAuth consent grant: the granted scope set for a `(user_id, client_id)` pair. This is the single table for the P0-proposed consent state; do not create `zeroship.consents`.

Final columns: `user_id UUID NOT NULL`, `client_id TEXT NOT NULL`, `granted_scopes TEXT[] NOT NULL`, `granted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `last_used_at TIMESTAMPTZ NULL`, `remember_expires_at TIMESTAMPTZ NULL` (P1 OP addition for remembered-consent reprompt skip).

PK/FKs: PK `(user_id, client_id)`; FK `user_id -> zeroship.users(id) ON DELETE CASCADE`; FK `client_id -> zeroship.oauth_clients(client_id) ON DELETE CASCADE`.

### `zeroship.oauth_authorization_codes`

Old: new P1 OP table; no hand-authored current table. Hydra currently owns this in its vendor schema.
Purpose: single-use OAuth authorization code store with PKCE, nonce, redirect URI, and subject binding.

Final key columns: `code_hash BYTEA NOT NULL`, `client_id TEXT NOT NULL`, `user_id UUID NOT NULL`, `redirect_uri TEXT NOT NULL`, `pkce_challenge TEXT NOT NULL`, `pkce_method TEXT NOT NULL`, `nonce TEXT NULL`, `requested_scopes TEXT[] NOT NULL`, `granted_scopes TEXT[] NOT NULL`, `auth_time TIMESTAMPTZ NOT NULL`, `amr TEXT[] NOT NULL DEFAULT '{}'`, `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `expires_at TIMESTAMPTZ NOT NULL`, `consumed_at TIMESTAMPTZ NULL`.

PK/FKs: PK `code_hash`; FK `client_id -> zeroship.oauth_clients(client_id) ON DELETE CASCADE`; FK `user_id -> zeroship.users(id) ON DELETE CASCADE`.

### `zeroship.oauth_refresh_tokens`

Old: new P1 OP table; no hand-authored current table. Hydra currently owns refresh families.
Purpose: platform-owned refresh-token rotation family and reuse-detection state.

Final key columns: `token_hash BYTEA NOT NULL`, `refresh_family_id TEXT NOT NULL`, `client_id TEXT NOT NULL`, `user_id UUID NOT NULL`, `granted_scopes TEXT[] NOT NULL`, `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `expires_at TIMESTAMPTZ NOT NULL`, `rotated_at TIMESTAMPTZ NULL`, `replaced_by_token_hash BYTEA NULL`, `revoked_at TIMESTAMPTZ NULL`, `last_used_at TIMESTAMPTZ NULL`.

PK/FKs: PK `token_hash`; FK `client_id -> zeroship.oauth_clients(client_id) ON DELETE CASCADE`; FK `user_id -> zeroship.users(id) ON DELETE CASCADE`; index/unique family policy on `(refresh_family_id, client_id, user_id)` per OP spec.

### `zeroship.signing_keys`

Old: `zeroship.jwk_key_state` -> `zeroship.signing_keys`.
Creating migration: `V0002__auth.sql`.
Purpose: signing-key/JWKS rotation state. P1 OP should expand this from "state row" into the platform AS public key and rotation registry.

Final columns: `kid TEXT NOT NULL`, `alg TEXT NOT NULL`, `public_jwk JSONB NOT NULL`, `status TEXT NOT NULL CHECK (status IN ('active','next','retiring'))`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `activated_at TIMESTAMPTZ NULL`, `retiring_at TIMESTAMPTZ NULL`, `retired_at TIMESTAMPTZ NULL`.

PK/FKs: PK `kid`; no FKs.

Custody rule: no raw private signing key is stored in this table. `crates/auth` loads the active private key from `AUTH_SIGNING_KEY_FILE` at boot (PEM/PKCS#8), mirroring the gateway's `GATEWAY_SIGNING_KEY_FILE`. If multi-node rotation later needs persisted private key material, the persisted value must be encrypted at rest with a wrapping key from the same file/KMS custody path; the database must never contain plaintext private key material and no database role may be able to read usable private key bytes.

### `zeroship.token_revocations`

Old: `zeroship.token_revocations` -> `zeroship.token_revocations`.
Creating migration: `V0002__auth.sql`.
Purpose: cross-node token-family revocation marker for app tokens.

Final columns: `client_id TEXT NOT NULL`, `token_subject TEXT NOT NULL` (old `sub`; projected token `sub`, not a platform-user FK), `revoked_after TIMESTAMPTZ NOT NULL DEFAULT NOW()`.

PK/FKs: PK `(client_id, token_subject)`; FK `client_id -> zeroship.oauth_clients(client_id) ON DELETE CASCADE`.

### `zeroship.dpop_jtis`

Old: `zeroship.dpop_jti` -> `zeroship.dpop_jtis`.
Creating migration: `V0002__auth.sql`.
Purpose: DPoP proof replay cache.

Final columns: `jti TEXT NOT NULL`, `inserted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`.

PK/FKs: PK `jti`; no FKs.

### `zeroship.rate_limits`

Old: `zeroship.rate_limits` -> `zeroship.rate_limits`.
Creating migration: `V0002__auth.sql`; TTL hardening in `V0033__auth_rate_limits_ttl.sql`.
Purpose: auth login throttling state and relay sentinel cleanup substrate.

Final columns: `bucket_key TEXT NOT NULL`, `tokens REAL NOT NULL`, `updated_at TIMESTAMPTZ NOT NULL`.

PK/FKs: PK `bucket_key`; no FKs.

### `zeroship.audit_events`

Old: `zeroship.audit_events` -> `zeroship.audit_events`.
Creating migration: `V0002__auth.sql`.
Purpose: auth service structured event log with append-only trigger.

Final columns: `id BIGSERIAL NOT NULL`, `occurred_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `event_type TEXT NOT NULL`, `outcome TEXT NOT NULL`, `actor_user_id UUID NULL`, `client_id TEXT NULL`, `request_id TEXT NULL`, `ip INET NULL`, `user_agent TEXT NULL`, `auth_method TEXT NULL`, `detail JSONB NULL`.

PK/FKs: PK `id`; optional FK `actor_user_id -> zeroship.users(id)` may be added if retention/anonymization behavior allows it.

### `zeroship.cron_state`

Old: `zeroship.cron_state` -> `zeroship.cron_state`.
Creating migration: `V0002__auth.sql`.
Purpose: auth cron bookkeeping.

Final columns: `key TEXT NOT NULL`, `last_rotated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `notes TEXT NULL`.

PK/FKs: PK `key`; no FKs.

### `zeroship.oauth_clients`

Old: `zeroship.oauth_clients` -> `zeroship.oauth_clients`.
Creating migration: `V0004__control.sql`.
Purpose: authoritative OAuth client registry. It is no longer a Hydra mirror.

Final columns: `client_id TEXT NOT NULL`, `client_name TEXT NOT NULL`, `client_uri TEXT NULL`, `logo_uri TEXT NULL`, `redirect_uris TEXT[] NOT NULL`, `client_secret_hash TEXT NULL` (P1 OP addition for confidential clients; column-level grant only to auth/control roles that verify or rotate client credentials), `scopes TEXT[] NOT NULL`, `skip_consent BOOLEAN NOT NULL DEFAULT FALSE`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `created_by_user_id UUID NULL` (old `created_by`). Delete stale `hydra_client_id`.

PK/FKs: PK `client_id`; FK `created_by_user_id -> zeroship.users(id)`.

### `zeroship.app_oauth_clients`

Old: `zeroship.app_oauth_clients` -> `zeroship.app_oauth_clients`.
Creating migration: `V0005__control_app_oauth_clients.sql`.
Purpose: per-hosted-app extension of the OAuth client registry: app link and pairwise sector.

Final columns: `app_id UUID NOT NULL`, `client_id TEXT NOT NULL`, `sector_identifier TEXT NOT NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`.

PK/FKs: PK/FK `app_id -> zeroship.apps(id) ON DELETE CASCADE`; unique/FK `client_id -> zeroship.oauth_clients(client_id) ON DELETE CASCADE`.

### `zeroship.app_scope_defs`

Old: `zeroship.app_scope_defs` -> `zeroship.app_scope_defs`.
Creating migration: `V0007__control_app_scope_defs.sql`.
Purpose: app-declared custom end-user OAuth scope registry.

Final columns: `app_id UUID NOT NULL`, `scope_id TEXT NOT NULL`, `label TEXT NOT NULL`, `description TEXT NULL`.

PK/FKs: PK `(app_id, scope_id)`; FK `app_id -> zeroship.apps(id) ON DELETE CASCADE`.

### `zeroship.permission_tokens`

Old: `zeroship.permission_tokens` -> `zeroship.permission_tokens`.
Creating migration: `V0004__control.sql`.
Purpose: platform permission tokens, including PATs and OAuth-derived control tokens.

Final columns: `id UUID NOT NULL`, `user_id UUID NOT NULL` (old `owner_id`), `kind TEXT NOT NULL CHECK (kind IN ('pat','oauth_grant'))`, `client_id TEXT NULL`, `name TEXT NOT NULL`, `policies JSONB NOT NULL`, `policy_hash TEXT NOT NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `expires_at TIMESTAMPTZ NULL`, `revoked_at TIMESTAMPTZ NULL`, `last_used_at TIMESTAMPTZ NULL`.

PK/FKs: PK `id`; FK `user_id -> zeroship.users(id) ON DELETE CASCADE`; optional FK `client_id -> zeroship.oauth_clients(client_id)` when `kind = 'oauth_grant'`.

### `zeroship.principal_grants`

Old: `zeroship.principal_grants` -> `zeroship.principal_grants`.
Creating migration: `V0060__identity_links_principal_grants.sql`.
Purpose: deploy/control-plane authorization grants. Keep the table name because it describes authorization semantics, but the user FK column is `user_id`.

Final columns: `user_id UUID NOT NULL` (old `principal_id`), `grant_name TEXT NOT NULL`.

PK/FKs: PK `(user_id, grant_name)`; FK `user_id -> zeroship.users(id)`.

### `zeroship.device_authorizations`

Old: `zeroship.device_grants` -> `zeroship.device_authorizations`.
Creating migration: `V0061__device_grants.sql`.
Purpose: pending platform-mediated OAuth device authorization for providers without a native device endpoint.

Final columns: `device_code_hash TEXT NOT NULL`, `user_code TEXT NOT NULL UNIQUE`, `status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','approved','denied'))`, `user_id UUID NULL` (old `principal_id`), `provider_refresh_token_enc BYTEA NULL` (old `gotrue_refresh_token_enc`), `provider TEXT NOT NULL`, `requested_scope TEXT NULL` (old `scope`), `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `expires_at TIMESTAMPTZ NOT NULL`, `last_polled_at TIMESTAMPTZ NULL`.

PK/FKs: PK `device_code_hash`; FK `user_id -> zeroship.users(id)`.

### `zeroship.app_net_grants`

Old: `zeroship.app_net_grants` -> `zeroship.app_net_grants`.
Creating migration: `V0058__app_net_grants.sql`.
Purpose: operator-authorized app outbound raw-TCP egress grants.

Final columns: `app_id UUID NOT NULL`, `host TEXT NOT NULL`, `port INT NOT NULL CHECK (port BETWEEN 1 AND 65535)`, `granted_by_user_id UUID NULL` (old `granted_by TEXT`, currently populated from `guard.principal_id.to_string()`), `granted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `note TEXT NULL`.

PK/FKs: PK `(app_id, host, port)`; FK `app_id -> zeroship.apps(id) ON DELETE CASCADE`; FK `granted_by_user_id -> zeroship.users(id)` when present.

### `zeroship.sessions`

Old: `zeroship.gateway_sessions` -> `zeroship.sessions`.
Creating/altering migrations: `V0002__auth.sql`, deferred app FK in `V0004__control.sql`, `V0008__auth_gateway_sessions_granted_scopes.sql`, `V0010__auth_gateway_sessions_auth_time_amr.sql`.
Purpose: per-origin hosted-app browser session rows for the gateway's `__Host-zeroship_app_session` cookie.

Final columns: `id UUID NOT NULL DEFAULT gen_random_uuid()`, `user_id UUID NOT NULL`, `app_id UUID NOT NULL`, `email CITEXT NULL`, `name TEXT NULL`, `avatar_url TEXT NULL`, `email_verified BOOLEAN NOT NULL DEFAULT FALSE`, `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `idle_expires_at TIMESTAMPTZ NOT NULL`, `absolute_expires_at TIMESTAMPTZ NOT NULL` (old `abs_expires_at`), `revoked_at TIMESTAMPTZ NULL`, `granted_scopes TEXT[] NOT NULL DEFAULT '{}'`, `auth_time TIMESTAMPTZ NULL`, `amr TEXT[] NOT NULL DEFAULT '{}'`.

PK/FKs: PK `id`; FK `user_id -> zeroship.users(id)`; FK `app_id -> zeroship.apps(id) ON DELETE CASCADE`.

### `zeroship.app_session_anchors`

Old: `zeroship.app_session_anchors` -> `zeroship.app_session_anchors`.
Creating migration: `V0006__auth_app_session_anchors.sql`.
Purpose: SDK reload-recovery anchor store, separate from interactive gateway sessions.

Final columns: `id UUID NOT NULL DEFAULT gen_random_uuid()`, `app_id UUID NOT NULL`, `client_id TEXT NOT NULL`, `user_id UUID NOT NULL` (old `global_user_id`), `refresh_token_enc BYTEA NOT NULL`, `refresh_family_id TEXT NOT NULL`, `granted_scopes TEXT[] NOT NULL DEFAULT '{}'`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `absolute_expires_at TIMESTAMPTZ NOT NULL` (old `abs_expires_at`), `revoked_at TIMESTAMPTZ NULL`.

PK/FKs: PK `id`; FK `app_id -> zeroship.apps(id) ON DELETE CASCADE`; FK `client_id -> zeroship.oauth_clients(client_id) ON DELETE CASCADE`; FK `user_id -> zeroship.users(id) ON DELETE CASCADE`.

## 4. Merges and deletions

| Current shape | Final shape | Decision |
| --- | --- | --- |
| `zeroship.identity_links` + `zeroship.federated_identities` | `zeroship.federated_identities` | Merge. The audit confirms both tables are the same concept: `(provider, provider-local subject) -> zeroship.users(id)`. `identity_links.principal_id` and `federated_identities.user_id` target the same platform user table; `identity_links.provider_subject` and `federated_identities.subject` are the same external subject. Keep the richer `federated_identities` name and column set. |
| `zeroship.device_grants` | `zeroship.device_authorizations` | Rename. The row is a pending device authorization, not an issued grant. Also remove provider-specific `gotrue_` column naming. |
| `zeroship.oauth_grants` vs P0 `zeroship.consents` | `zeroship.oauth_grants` only | Keep `oauth_grants`. It is already the end-user consent-grant ledger and the task's grant taxonomy reserves that name for this meaning. P1 should add remembered-consent fields here instead of creating `zeroship.consents`. |
| `zeroship.oauth_clients` + `zeroship.app_oauth_clients` | `zeroship.oauth_clients` + `zeroship.app_oauth_clients` | Keep two tables. `oauth_clients` is the authoritative generic RP/client registry; `app_oauth_clients` is the hosted-app extension with `app_id` and `sector_identifier`. Merging would force nullable app fields for console/non-app clients. Delete `hydra_client_id`. |
| `zeroship.jwk_key_state` | `zeroship.signing_keys` | Rename/expand. The OP now owns signing keys; the final table should not be framed as a missing Hydra/JWK cron state workaround. |
| `zeroship.dpop_jti` | `zeroship.dpop_jtis` | Rename to plural table name. |
| `zeroship.gateway_sessions` | `zeroship.sessions` | Rename to the final browser-session table name under the single `zeroship` schema. |
| Relay alias storage | `zeroship.app_user_identities.relay_email` | No separate relay-alias table exists in migrations. Keep the alias on the pairwise identity row with the active partial unique index. |

## 5. Full rename map

This map is intentionally flat and implementation-oriented. Rows marked `delete` should not get replacement columns. Rows marked `merge` are absorbed into the target table, not kept as aliases.

| Old object | New object | Action |
| --- | --- | --- |
| `zeroship.users` | `zeroship.users` | keep table |
| `zeroship.users.id` | `zeroship.users.id` | keep |
| `zeroship.users.email` | `zeroship.users.email` | keep |
| `zeroship.users.email_verified_at` | `zeroship.users.email_verified_at` | keep |
| `zeroship.users.name` | `zeroship.users.name` | keep |
| `zeroship.users.avatar_url` | `zeroship.users.avatar_url` | keep |
| `zeroship.users.password_hash` | `zeroship.users.password_hash` | keep |
| `zeroship.users.credential_version` | `zeroship.users.credential_version` | keep |
| `zeroship.users.locked_until` | `zeroship.users.lock_expires_at` | rename |
| `zeroship.users.disabled_at` | `zeroship.users.disabled_at` | keep |
| `zeroship.users.created_at` | `zeroship.users.created_at` | keep |
| `zeroship.users.updated_at` | `zeroship.users.updated_at` | keep |
| `zeroship.users.last_login_at` | `zeroship.users.last_login_at` | keep |
| `zeroship.users.failed_login_count` | `zeroship.users.failed_login_count` | keep |
| `zeroship.users.deletion_requested_at` | `zeroship.users.deletion_requested_at` | keep |
| `zeroship.users.deletion_scheduled_for` | `zeroship.users.deletion_scheduled_at` | rename |
| `zeroship.users.anonymized_at` | `zeroship.users.anonymized_at` | keep |
| `zeroship.federated_identities` | `zeroship.federated_identities` | keep table; merged target |
| `zeroship.federated_identities.id` | `zeroship.federated_identities.id` | keep |
| `zeroship.federated_identities.user_id` | `zeroship.federated_identities.user_id` | keep |
| `zeroship.federated_identities.provider` | `zeroship.federated_identities.provider` | keep |
| `zeroship.federated_identities.subject` | `zeroship.federated_identities.provider_subject` | rename |
| `zeroship.federated_identities.email_at_link` | `zeroship.federated_identities.email_at_link` | keep |
| `zeroship.federated_identities.raw_profile` | `zeroship.federated_identities.raw_profile` | keep |
| `zeroship.federated_identities.linked_at` | `zeroship.federated_identities.linked_at` | keep |
| `zeroship.identity_links` | `zeroship.federated_identities` | merge/delete old table |
| `zeroship.identity_links.principal_id` | `zeroship.federated_identities.user_id` | rename/merge |
| `zeroship.identity_links.provider` | `zeroship.federated_identities.provider` | merge |
| `zeroship.identity_links.provider_subject` | `zeroship.federated_identities.provider_subject` | merge |
| `zeroship.identity_links.email` | `zeroship.federated_identities.email_at_link` | rename/merge |
| `zeroship.identity_links.created_at` | `zeroship.federated_identities.linked_at` | rename/merge |
| `zeroship.idp_sessions` | `zeroship.idp_sessions` | keep table |
| `zeroship.idp_sessions.id` | `zeroship.idp_sessions.id` | keep |
| `zeroship.idp_sessions.user_id` | `zeroship.idp_sessions.user_id` | keep |
| `zeroship.idp_sessions.auth_method` | `zeroship.idp_sessions.auth_method` | keep |
| `zeroship.idp_sessions.amr` | `zeroship.idp_sessions.amr` | keep protocol claim |
| `zeroship.idp_sessions.acr` | `zeroship.idp_sessions.acr` | keep protocol claim |
| `zeroship.idp_sessions.auth_time` | `zeroship.idp_sessions.auth_time` | keep protocol claim |
| `zeroship.idp_sessions.credential_version` | `zeroship.idp_sessions.credential_version` | keep |
| `zeroship.idp_sessions.idle_expires_at` | `zeroship.idp_sessions.idle_expires_at` | keep |
| `zeroship.idp_sessions.abs_expires_at` | `zeroship.idp_sessions.absolute_expires_at` | rename |
| `zeroship.idp_sessions.revoked_at` | `zeroship.idp_sessions.revoked_at` | keep |
| `zeroship.magic_links` | `zeroship.magic_links` | keep table |
| `zeroship.magic_links.token_hash` | `zeroship.magic_links.token_hash` | keep |
| `zeroship.magic_links.email` | `zeroship.magic_links.email` | keep |
| `zeroship.magic_links.user_id` | `zeroship.magic_links.user_id` | keep |
| `zeroship.magic_links.csrf_nonce` | `zeroship.magic_links.csrf_nonce` | keep |
| `zeroship.magic_links.purpose` | `zeroship.magic_links.purpose` | keep |
| `zeroship.magic_links.request_ip` | `zeroship.magic_links.request_ip` | keep |
| `zeroship.magic_links.request_ua` | `zeroship.magic_links.request_user_agent` | rename |
| `zeroship.magic_links.issued_at` | `zeroship.magic_links.issued_at` | keep |
| `zeroship.magic_links.expires_at` | `zeroship.magic_links.expires_at` | keep |
| `zeroship.magic_links.consumed_pending_at` | `zeroship.magic_links.consumed_pending_at` | keep |
| `zeroship.magic_links.consumed_at` | `zeroship.magic_links.consumed_at` | keep |
| `zeroship.magic_completions` | `zeroship.magic_completions` | keep table |
| `zeroship.magic_completions.csrf_nonce` | `zeroship.magic_completions.csrf_nonce` | keep |
| `zeroship.magic_completions.code` | `zeroship.magic_completions.code` | keep |
| `zeroship.magic_completions.email` | `zeroship.magic_completions.email` | keep |
| `zeroship.magic_completions.login_challenge` | `zeroship.magic_completions.login_challenge` | keep |
| `zeroship.magic_completions.attempts` | `zeroship.magic_completions.attempts` | keep |
| `zeroship.magic_completions.expires_at` | `zeroship.magic_completions.expires_at` | keep |
| `zeroship.magic_completions.consumed_pending_at` | `zeroship.magic_completions.consumed_pending_at` | keep |
| `zeroship.magic_completions.consumed_at` | `zeroship.magic_completions.consumed_at` | keep |
| `zeroship.email_verifications` | `zeroship.email_verifications` | keep table |
| `zeroship.email_verifications.token_hash` | `zeroship.email_verifications.token_hash` | keep |
| `zeroship.email_verifications.user_id` | `zeroship.email_verifications.user_id` | keep |
| `zeroship.email_verifications.email` | `zeroship.email_verifications.email` | keep |
| `zeroship.email_verifications.issued_at` | `zeroship.email_verifications.issued_at` | keep |
| `zeroship.email_verifications.expires_at` | `zeroship.email_verifications.expires_at` | keep |
| `zeroship.email_verifications.consumed_at` | `zeroship.email_verifications.consumed_at` | keep |
| `zeroship.email_suppressions` | `zeroship.email_suppressions` | keep table |
| `zeroship.email_suppressions.email` | `zeroship.email_suppressions.email` | keep |
| `zeroship.email_suppressions.reason` | `zeroship.email_suppressions.reason` | keep |
| `zeroship.email_suppressions.suppressed_at` | `zeroship.email_suppressions.suppressed_at` | keep |
| `zeroship.email_suppressions.provider_msg` | `zeroship.email_suppressions.provider_message` | rename |
| `zeroship.totp_credentials` | `zeroship.totp_credentials` | keep table |
| `zeroship.totp_credentials.user_id` | `zeroship.totp_credentials.user_id` | keep |
| `zeroship.totp_credentials.encrypted_secret` | `zeroship.totp_credentials.encrypted_secret` | keep |
| `zeroship.totp_credentials.confirmed_at` | `zeroship.totp_credentials.confirmed_at` | keep |
| `zeroship.totp_credentials.created_at` | `zeroship.totp_credentials.created_at` | keep |
| `zeroship.totp_backup_codes` | `zeroship.totp_backup_codes` | keep table |
| `zeroship.totp_backup_codes.id` | `zeroship.totp_backup_codes.id` | keep |
| `zeroship.totp_backup_codes.user_id` | `zeroship.totp_backup_codes.user_id` | keep |
| `zeroship.totp_backup_codes.code_hash` | `zeroship.totp_backup_codes.code_hash` | keep |
| `zeroship.totp_backup_codes.used_at` | `zeroship.totp_backup_codes.used_at` | keep |
| `zeroship.totp_backup_codes.created_at` | `zeroship.totp_backup_codes.created_at` | keep |
| `zeroship.app_user_identities` | `zeroship.app_user_identities` | keep table |
| `zeroship.app_user_identities.app_client_id` | `zeroship.app_user_identities.client_id` | rename |
| `zeroship.app_user_identities.global_user_id` | `zeroship.app_user_identities.user_id` | rename |
| `zeroship.app_user_identities.pairwise_sub` | `zeroship.app_user_identities.pairwise_sub` | keep projected subject |
| `zeroship.app_user_identities.relay_email` | `zeroship.app_user_identities.relay_email` | keep |
| `zeroship.app_user_identities.created_at` | `zeroship.app_user_identities.created_at` | keep |
| `zeroship.app_user_identities.revoked_at` | `zeroship.app_user_identities.revoked_at` | keep |
| `zeroship.oauth_grants` | `zeroship.oauth_grants` | keep table; keep name instead of `consents` |
| `zeroship.oauth_grants.user_id` | `zeroship.oauth_grants.user_id` | keep |
| `zeroship.oauth_grants.client_id` | `zeroship.oauth_grants.client_id` | keep |
| `zeroship.oauth_grants.granted_scopes` | `zeroship.oauth_grants.granted_scopes` | keep |
| `zeroship.oauth_grants.granted_at` | `zeroship.oauth_grants.granted_at` | keep |
| `zeroship.oauth_grants.updated_at` | `zeroship.oauth_grants.updated_at` | keep |
| `zeroship.oauth_grants.last_used_at` | `zeroship.oauth_grants.last_used_at` | keep |
| `(new)` | `zeroship.oauth_grants.remember_expires_at` | P1 OP addition; no `zeroship.consents` table |
| `(new)` | `zeroship.oauth_authorization_codes` | P1 OP addition |
| `(new)` | `zeroship.oauth_refresh_tokens` | P1 OP addition |
| `zeroship.jwk_key_state` | `zeroship.signing_keys` | rename/expand table |
| `zeroship.jwk_key_state.set_name` | `(delete)` | stale key-set name; single OP signing registry is keyed by `kid` |
| `zeroship.jwk_key_state.kid` | `zeroship.signing_keys.kid` | keep protocol field |
| `zeroship.jwk_key_state.created_at` | `zeroship.signing_keys.created_at` | keep |
| `(new)` | `zeroship.signing_keys.alg` | P1 OP addition |
| `(new)` | `zeroship.signing_keys.public_jwk` | P1 OP addition; public material only |
| `(new)` | `zeroship.signing_keys.status` | P1 OP addition: `active`, `next`, or `retiring` |
| `(new)` | `zeroship.signing_keys.activated_at` | P1 OP rotation timestamp |
| `(new)` | `zeroship.signing_keys.retiring_at` | P1 OP rotation timestamp |
| `(new)` | `zeroship.signing_keys.retired_at` | P1 OP rotation timestamp |
| `zeroship.token_revocations` | `zeroship.token_revocations` | keep table |
| `zeroship.token_revocations.client_id` | `zeroship.token_revocations.client_id` | keep |
| `zeroship.token_revocations.sub` | `zeroship.token_revocations.token_subject` | rename |
| `zeroship.token_revocations.revoked_after` | `zeroship.token_revocations.revoked_after` | keep |
| `zeroship.dpop_jti` | `zeroship.dpop_jtis` | rename table to plural |
| `zeroship.dpop_jti.jti` | `zeroship.dpop_jtis.jti` | keep protocol field |
| `zeroship.dpop_jti.inserted_at` | `zeroship.dpop_jtis.inserted_at` | keep |
| `zeroship.rate_limits` | `zeroship.rate_limits` | keep table |
| `zeroship.rate_limits.bucket_key` | `zeroship.rate_limits.bucket_key` | keep |
| `zeroship.rate_limits.tokens` | `zeroship.rate_limits.tokens` | keep |
| `zeroship.rate_limits.updated_at` | `zeroship.rate_limits.updated_at` | keep |
| `zeroship.audit_events` | `zeroship.audit_events` | keep table |
| `zeroship.audit_events.id` | `zeroship.audit_events.id` | keep |
| `zeroship.audit_events.occurred_at` | `zeroship.audit_events.occurred_at` | keep |
| `zeroship.audit_events.event_type` | `zeroship.audit_events.event_type` | keep |
| `zeroship.audit_events.outcome` | `zeroship.audit_events.outcome` | keep |
| `zeroship.audit_events.actor_user_id` | `zeroship.audit_events.actor_user_id` | keep |
| `zeroship.audit_events.client_id` | `zeroship.audit_events.client_id` | keep |
| `zeroship.audit_events.request_id` | `zeroship.audit_events.request_id` | keep |
| `zeroship.audit_events.ip` | `zeroship.audit_events.ip` | keep |
| `zeroship.audit_events.user_agent` | `zeroship.audit_events.user_agent` | keep |
| `zeroship.audit_events.auth_method` | `zeroship.audit_events.auth_method` | keep |
| `zeroship.audit_events.detail` | `zeroship.audit_events.detail` | keep |
| `zeroship.cron_state` | `zeroship.cron_state` | keep table |
| `zeroship.cron_state.key` | `zeroship.cron_state.key` | keep |
| `zeroship.cron_state.last_rotated_at` | `zeroship.cron_state.last_rotated_at` | keep |
| `zeroship.cron_state.notes` | `zeroship.cron_state.notes` | keep |
| `zeroship.oauth_clients` | `zeroship.oauth_clients` | keep table; authoritative registry |
| `zeroship.oauth_clients.client_id` | `zeroship.oauth_clients.client_id` | keep |
| `zeroship.oauth_clients.client_name` | `zeroship.oauth_clients.client_name` | keep |
| `zeroship.oauth_clients.client_uri` | `zeroship.oauth_clients.client_uri` | keep |
| `zeroship.oauth_clients.logo_uri` | `zeroship.oauth_clients.logo_uri` | keep |
| `zeroship.oauth_clients.redirect_uris` | `zeroship.oauth_clients.redirect_uris` | keep |
| `(new)` | `zeroship.oauth_clients.client_secret_hash` | P1 OP addition; column-level grant only to auth/control credential handlers |
| `zeroship.oauth_clients.scopes` | `zeroship.oauth_clients.scopes` | keep |
| `zeroship.oauth_clients.skip_consent` | `zeroship.oauth_clients.skip_consent` | keep |
| `zeroship.oauth_clients.created_at` | `zeroship.oauth_clients.created_at` | keep |
| `zeroship.oauth_clients.created_by` | `zeroship.oauth_clients.created_by_user_id` | rename |
| `zeroship.oauth_clients.hydra_client_id` | `(delete)` | stale Hydra mirror column |
| `zeroship.app_oauth_clients` | `zeroship.app_oauth_clients` | keep table |
| `zeroship.app_oauth_clients.app_id` | `zeroship.app_oauth_clients.app_id` | keep |
| `zeroship.app_oauth_clients.client_id` | `zeroship.app_oauth_clients.client_id` | keep |
| `zeroship.app_oauth_clients.sector_identifier` | `zeroship.app_oauth_clients.sector_identifier` | keep |
| `zeroship.app_oauth_clients.created_at` | `zeroship.app_oauth_clients.created_at` | keep |
| `zeroship.app_oauth_clients.updated_at` | `zeroship.app_oauth_clients.updated_at` | keep |
| `zeroship.app_scope_defs` | `zeroship.app_scope_defs` | keep table |
| `zeroship.app_scope_defs.app_id` | `zeroship.app_scope_defs.app_id` | keep |
| `zeroship.app_scope_defs.scope_id` | `zeroship.app_scope_defs.scope_id` | keep |
| `zeroship.app_scope_defs.label` | `zeroship.app_scope_defs.label` | keep |
| `zeroship.app_scope_defs.description` | `zeroship.app_scope_defs.description` | keep |
| `zeroship.permission_tokens` | `zeroship.permission_tokens` | keep table |
| `zeroship.permission_tokens.id` | `zeroship.permission_tokens.id` | keep |
| `zeroship.permission_tokens.owner_id` | `zeroship.permission_tokens.user_id` | rename |
| `zeroship.permission_tokens.kind` | `zeroship.permission_tokens.kind` | keep |
| `zeroship.permission_tokens.client_id` | `zeroship.permission_tokens.client_id` | keep |
| `zeroship.permission_tokens.name` | `zeroship.permission_tokens.name` | keep |
| `zeroship.permission_tokens.policies` | `zeroship.permission_tokens.policies` | keep |
| `zeroship.permission_tokens.policy_hash` | `zeroship.permission_tokens.policy_hash` | keep |
| `zeroship.permission_tokens.created_at` | `zeroship.permission_tokens.created_at` | keep |
| `zeroship.permission_tokens.expires_at` | `zeroship.permission_tokens.expires_at` | keep |
| `zeroship.permission_tokens.revoked_at` | `zeroship.permission_tokens.revoked_at` | keep |
| `zeroship.permission_tokens.last_used_at` | `zeroship.permission_tokens.last_used_at` | keep |
| `zeroship.principal_grants` | `zeroship.principal_grants` | keep table; keep grant table name |
| `zeroship.principal_grants.principal_id` | `zeroship.principal_grants.user_id` | rename |
| `zeroship.principal_grants.grant_name` | `zeroship.principal_grants.grant_name` | keep |
| `zeroship.device_grants` | `zeroship.device_authorizations` | rename |
| `zeroship.device_grants.device_code_hash` | `zeroship.device_authorizations.device_code_hash` | keep |
| `zeroship.device_grants.user_code` | `zeroship.device_authorizations.user_code` | keep |
| `zeroship.device_grants.status` | `zeroship.device_authorizations.status` | keep |
| `zeroship.device_grants.principal_id` | `zeroship.device_authorizations.user_id` | rename |
| `zeroship.device_grants.gotrue_refresh_token_enc` | `zeroship.device_authorizations.provider_refresh_token_enc` | rename |
| `zeroship.device_grants.provider` | `zeroship.device_authorizations.provider` | keep |
| `zeroship.device_grants.scope` | `zeroship.device_authorizations.requested_scope` | rename |
| `zeroship.device_grants.created_at` | `zeroship.device_authorizations.created_at` | keep |
| `zeroship.device_grants.expires_at` | `zeroship.device_authorizations.expires_at` | keep |
| `zeroship.device_grants.last_polled_at` | `zeroship.device_authorizations.last_polled_at` | keep |
| `zeroship.app_net_grants` | `zeroship.app_net_grants` | keep table |
| `zeroship.app_net_grants.app_id` | `zeroship.app_net_grants.app_id` | keep |
| `zeroship.app_net_grants.host` | `zeroship.app_net_grants.host` | keep |
| `zeroship.app_net_grants.port` | `zeroship.app_net_grants.port` | keep |
| `zeroship.app_net_grants.granted_by` | `zeroship.app_net_grants.granted_by_user_id` | rename/type to UUID |
| `zeroship.app_net_grants.granted_at` | `zeroship.app_net_grants.granted_at` | keep |
| `zeroship.app_net_grants.note` | `zeroship.app_net_grants.note` | keep |
| `zeroship.gateway_sessions` | `zeroship.sessions` | rename/move |
| `zeroship.gateway_sessions.id` | `zeroship.sessions.id` | keep |
| `zeroship.gateway_sessions.user_id` | `zeroship.sessions.user_id` | keep |
| `zeroship.gateway_sessions.app_id` | `zeroship.sessions.app_id` | keep |
| `zeroship.gateway_sessions.email` | `zeroship.sessions.email` | keep |
| `zeroship.gateway_sessions.name` | `zeroship.sessions.name` | keep |
| `zeroship.gateway_sessions.avatar_url` | `zeroship.sessions.avatar_url` | keep |
| `zeroship.gateway_sessions.email_verified` | `zeroship.sessions.email_verified` | keep |
| `zeroship.gateway_sessions.issued_at` | `zeroship.sessions.issued_at` | keep |
| `zeroship.gateway_sessions.idle_expires_at` | `zeroship.sessions.idle_expires_at` | keep |
| `zeroship.gateway_sessions.abs_expires_at` | `zeroship.sessions.absolute_expires_at` | rename |
| `zeroship.gateway_sessions.revoked_at` | `zeroship.sessions.revoked_at` | keep |
| `zeroship.gateway_sessions.granted_scopes` | `zeroship.sessions.granted_scopes` | keep |
| `zeroship.gateway_sessions.auth_time` | `zeroship.sessions.auth_time` | keep protocol claim |
| `zeroship.gateway_sessions.amr` | `zeroship.sessions.amr` | keep protocol claim |
| `zeroship.app_session_anchors` | `zeroship.app_session_anchors` | keep table |
| `zeroship.app_session_anchors.id` | `zeroship.app_session_anchors.id` | keep |
| `zeroship.app_session_anchors.app_id` | `zeroship.app_session_anchors.app_id` | keep |
| `zeroship.app_session_anchors.client_id` | `zeroship.app_session_anchors.client_id` | keep |
| `zeroship.app_session_anchors.global_user_id` | `zeroship.app_session_anchors.user_id` | rename |
| `zeroship.app_session_anchors.refresh_token_enc` | `zeroship.app_session_anchors.refresh_token_enc` | keep |
| `zeroship.app_session_anchors.refresh_family_id` | `zeroship.app_session_anchors.refresh_family_id` | keep |
| `zeroship.app_session_anchors.granted_scopes` | `zeroship.app_session_anchors.granted_scopes` | keep |
| `zeroship.app_session_anchors.created_at` | `zeroship.app_session_anchors.created_at` | keep |
| `zeroship.app_session_anchors.abs_expires_at` | `zeroship.app_session_anchors.absolute_expires_at` | rename |
| `zeroship.app_session_anchors.revoked_at` | `zeroship.app_session_anchors.revoked_at` | keep |

## 6. Open questions

1. **Separate operator population:** the audit did not find one. `principal_id` is only a code/authz concept today and all persisted rows target `zeroship.users(id)`. If product later requires non-user service principals/operators, add a distinct actor table then; do not keep `principal_id` in DB as a speculative placeholder now.
2. **`app_net_grants.granted_by_user_id`:** current code writes `guard.principal_id.to_string()` into `granted_by TEXT`, so this redesign types it as `UUID` and FKs it to `zeroship.users(id)`. If P1 needs system-authored grants, add an explicit nullable `granted_by_system TEXT` or an actor-union design rather than overloading a user FK.
3. **Final OP token table internals:** `oauth_authorization_codes` and `oauth_refresh_tokens` names are canonical here, but exact token hashing, family, reuse-detection, and status columns should be finalized in the OP AS spec. Do not create differently named tables while that spec is refined.
4. **DPoP replay-cache ownership:** this proposal keeps `zeroship.dpop_jtis` as token-security state. If P1 proves the resource-server replay cache must be gateway-local for latency, keep the same plural table name and revisit the grants/RLS owner, not the single-schema decision.
5. **Audit-event FKs and retention:** `zeroship.audit_events.actor_user_id` can remain nullable without a hard FK if account erasure/retention needs append-only historical rows after user deletion. The name is already clear and should not become `principal_id`.
