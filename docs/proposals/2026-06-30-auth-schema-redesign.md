# Auth schema redesign: canonical naming taxonomy

**Status:** proposal, design-only. No migration code in this change.
**Date:** 2026-06-30.
**Scope:** auth, identity, OAuth, session, grant, token, and client tables currently created by `db/migrations/V*.sql` excluding `*.down.sql`.

This is a pre-launch clean redesign. P1 should implement the final names directly; do not add compatibility aliases, migration shims, or old-name fallbacks.

## 1. Naming conventions

1. **Schemas express trust domains.** A table lives with the service that owns its write authority and invariants, not in the historical catch-all `zeroship` schema.
2. **Table names are plural snake_case nouns.** Keep current table names when they already communicate the concept. Rename only when a name is misleading, duplicated, singular, stale, or tied to Hydra/GoTrue implementation detail.
3. **The canonical platform-user FK is `user_id`.** Every database column that references the platform user row is named `user_id` and FKs to `auth.users(id)`. The audit found no separate database-backed operator/principal population: current `principal_id`, `global_user_id`, and control-plane bearer `subject` paths all resolve to `zeroship.users(id)`.
4. **Keep protocol subject names only for projected or external subjects.** `pairwise_sub`, `provider_subject`, `token_subject`, `jti`, `kid`, `amr`, `acr`, and `auth_time` are allowed because they are protocol claims or projected identifiers, not platform-user FKs.
5. **FK columns use `_id`.** Actor columns include the target noun: `created_by_user_id`, `granted_by_user_id`. Hashed secrets use `_hash`; encrypted secrets use `_enc`.
6. **Timestamps use `_at` or established expiry wording.** Use `created_at`, `updated_at`, `revoked_at`, `expires_at`, `idle_expires_at`, `absolute_expires_at`. Rename ambiguous non-`_at` current names such as `locked_until` and `deletion_scheduled_for`.
7. **OAuth grant names are disambiguated by meaning.** `oauth_grants` is the end-user OAuth consent grant. `principal_grants` is deploy/control authorization. `device_authorizations` is the RFC 8628-style pending device authorization. `app_net_grants` is operator-granted network egress.
8. **Drop provider implementation framing.** `oauth_clients` is no longer a Hydra mirror after the OP build. It is the authoritative client registry written by control and enforced by the platform AS in `auth`.

## 2. Schema-per-trust-domain layout

Current ground truth: `V0001__extensions_schemas.sql` creates one `zeroship` schema and all hand-authored platform tables currently land there. `V0027__oauth_hydra_schema.sql` creates the vendor-owned `oauth_hydra` schema/role only; Hydra's own tables are not hand-authored in this repo and are retired by the OP replacement.

Final layout:

| Schema | Trust boundary | Tables in this redesign |
| --- | --- | --- |
| `auth` | Public OP/IdP and identity authority. Owns global users, upstream identity links, auth factors, IdP sessions, OAuth grants, token-state, signing keys, and per-app pairwise identity projections. | `users`, `federated_identities`, `idp_sessions`, `magic_links`, `magic_completions`, `email_verifications`, `email_suppressions`, `totp_credentials`, `totp_backup_codes`, `app_user_identities`, `oauth_grants`, `oauth_authorization_codes`, `oauth_refresh_tokens`, `signing_keys`, `token_revocations`, `dpop_jtis`, `rate_limits`, `audit_events`, `cron_state` |
| `control` | Internal creator/platform control plane. Owns app CRUD, authoritative OAuth client registry writes, declared scope registry, deploy/PAT authorization, device authorization approval, and operator network grants. | `oauth_clients`, `app_oauth_clients`, `app_scope_defs`, `permission_tokens`, `principal_grants`, `device_authorizations`, `app_net_grants` |
| `gateway` | Edge session state for hosted creator apps. Gateway owns browser cookie session validation and reload-recovery anchor lifecycle; auth/control may still have limited cross-tenant revocation writes. | `sessions`, `app_session_anchors` |

Boundary decisions:

- `auth.users` is the single platform user pool for end users, creators, and operators until a distinct actor population is intentionally introduced. All old FKs to `zeroship.users(id)` become FKs to `auth.users(id)`.
- `control.oauth_clients` and `control.app_oauth_clients` stay in `control` because app deploy/admin flows write them. The `auth` AS reads them over shared Postgres and enforces redirect URI and scope rules.
- `auth.app_user_identities` stays in `auth` because it is identity data: the persisted pairwise subject and relay alias projection for `(client_id, user_id)`. Gateway can write/upsert through grants/RLS, but it does not own the concept.
- `gateway.sessions` and `gateway.app_session_anchors` move to `gateway` because they are browser edge session credentials, not OP session state.
- There is no separate relay-alias table in the migrations. The relay alias is `app_user_identities.relay_email`.

## 3. The redesigned tables

Each entry lists the audited creating migration, current purpose from comments, final table, final key columns, and final PK/FKs. Column types/nullability/defaults are grounded in the current SQL unless marked as a P1 OP addition.

### `auth.users`

Old: `zeroship.users` -> `auth.users`.
Creating/altering migrations: `V0002__auth.sql`, `V0030__auth_failed_login_count.sql`, `V0034__auth_account_deletion.sql`.
Purpose: global platform user pool; all auth/control identity joins target this row.

Final columns: `id UUID NOT NULL DEFAULT gen_random_uuid()`, `email CITEXT NOT NULL UNIQUE`, `email_verified_at TIMESTAMPTZ NULL`, `name TEXT NOT NULL`, `avatar_url TEXT NULL`, `password_hash TEXT NULL`, `credential_version BIGINT NOT NULL DEFAULT 0`, `lock_expires_at TIMESTAMPTZ NULL` (old `locked_until`), `disabled_at TIMESTAMPTZ NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `last_login_at TIMESTAMPTZ NULL`, `failed_login_count INTEGER NOT NULL DEFAULT 0`, `deletion_requested_at TIMESTAMPTZ NULL`, `deletion_scheduled_at TIMESTAMPTZ NULL` (old `deletion_scheduled_for`), `anonymized_at TIMESTAMPTZ NULL`.

PK/FKs: PK `id`. No FKs.

### `auth.federated_identities`

Old: `zeroship.federated_identities` + `zeroship.identity_links` -> `auth.federated_identities`.
Creating migrations: `V0002__auth.sql`, `V0060__identity_links_principal_grants.sql`.
Purpose: one external provider-local subject bound to one platform user. This merges the duplicate control-plane identity bridge into the existing auth identity-link table.

Final columns: `id UUID NOT NULL DEFAULT gen_random_uuid()`, `user_id UUID NOT NULL`, `provider TEXT NOT NULL`, `provider_subject TEXT NOT NULL` (old `federated_identities.subject`; same as `identity_links.provider_subject`), `email_at_link CITEXT NULL` (old `identity_links.email` merged here), `raw_profile JSONB NULL`, `linked_at TIMESTAMPTZ NOT NULL DEFAULT NOW()` (old `identity_links.created_at`).

PK/FKs: PK `id`; FK `user_id -> auth.users(id) ON DELETE CASCADE`; unique `(provider, provider_subject)`.

### `auth.idp_sessions`

Old: `zeroship.idp_sessions` -> `auth.idp_sessions`.
Creating migration: `V0002__auth.sql`.
Purpose: auth service IdP login session for the `__Host-zsidp_session` cookie.

Final columns: `id UUID NOT NULL DEFAULT gen_random_uuid()`, `user_id UUID NOT NULL`, `auth_method TEXT NOT NULL`, `amr TEXT[] NOT NULL`, `acr TEXT NULL`, `auth_time TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `credential_version BIGINT NOT NULL DEFAULT 0`, `idle_expires_at TIMESTAMPTZ NOT NULL`, `absolute_expires_at TIMESTAMPTZ NOT NULL` (old `abs_expires_at`), `revoked_at TIMESTAMPTZ NULL`.

PK/FKs: PK `id`; FK `user_id -> auth.users(id) ON DELETE CASCADE`.

### `auth.magic_links`

Old: `zeroship.magic_links` -> `auth.magic_links`.
Creating/altering migrations: `V0002__auth.sql`, `V0029__auth_magic_links_user_id.sql`.
Purpose: magic-link login tokens and password-reset tokens.

Final columns: `token_hash BYTEA NOT NULL`, `email CITEXT NOT NULL`, `user_id UUID NULL`, `csrf_nonce TEXT NOT NULL`, `purpose TEXT NOT NULL`, `request_ip INET NULL`, `request_user_agent TEXT NULL` (old `request_ua`), `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `expires_at TIMESTAMPTZ NOT NULL`, `consumed_pending_at TIMESTAMPTZ NULL`, `consumed_at TIMESTAMPTZ NULL`.

PK/FKs: PK `token_hash`; FK `user_id -> auth.users(id) ON DELETE CASCADE`. Keep email index and user index under new names.

### `auth.magic_completions`

Old: `zeroship.magic_completions` -> `auth.magic_completions`.
Creating migration: `V0002__auth.sql`.
Purpose: cross-device magic-link completion handshakes.

Final columns: `csrf_nonce TEXT NOT NULL`, `code TEXT NOT NULL`, `email CITEXT NOT NULL`, `login_challenge TEXT NOT NULL`, `attempts SMALLINT NOT NULL DEFAULT 0`, `expires_at TIMESTAMPTZ NOT NULL`, `consumed_pending_at TIMESTAMPTZ NULL`, `consumed_at TIMESTAMPTZ NULL`.

PK/FKs: PK `csrf_nonce`; no FKs.

### `auth.email_verifications`

Old: `zeroship.email_verifications` -> `auth.email_verifications`.
Creating migration: `V0002__auth.sql`.
Purpose: email-verification tokens.

Final columns: `token_hash BYTEA NOT NULL`, `user_id UUID NOT NULL`, `email CITEXT NOT NULL`, `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `expires_at TIMESTAMPTZ NOT NULL`, `consumed_at TIMESTAMPTZ NULL`.

PK/FKs: PK `token_hash`; FK `user_id -> auth.users(id) ON DELETE CASCADE`.

### `auth.email_suppressions`

Old: `zeroship.email_suppressions` -> `auth.email_suppressions`.
Creating migration: `V0002__auth.sql`.
Purpose: auth mailer bounce/complaint suppression list.

Final columns: `email CITEXT NOT NULL`, `reason TEXT NOT NULL`, `suppressed_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `provider_message TEXT NULL` (old `provider_msg`).

PK/FKs: PK `email`; no FKs.

### `auth.totp_credentials`

Old: `zeroship.totp_credentials` -> `auth.totp_credentials`.
Creating migration: `V0035__auth_totp_2fa.sql`.
Purpose: per-user TOTP shared secret, encrypted at rest.

Final columns: `user_id UUID NOT NULL`, `encrypted_secret BYTEA NOT NULL`, `confirmed_at TIMESTAMPTZ NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`.

PK/FKs: PK/FK `user_id -> auth.users(id) ON DELETE CASCADE`.

### `auth.totp_backup_codes`

Old: `zeroship.totp_backup_codes` -> `auth.totp_backup_codes`.
Creating migration: `V0035__auth_totp_2fa.sql`.
Purpose: single-use TOTP recovery codes.

Final columns: `id BIGINT GENERATED ALWAYS AS IDENTITY`, `user_id UUID NOT NULL`, `code_hash TEXT NOT NULL`, `used_at TIMESTAMPTZ NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`.

PK/FKs: PK `id`; FK `user_id -> auth.users(id) ON DELETE CASCADE`.

### `auth.app_user_identities`

Old: `zeroship.app_user_identities` -> `auth.app_user_identities`.
Creating migration: `V0009__auth_app_user_identities.sql`.
Purpose: per-app pairwise and relay identity mapping for `(client_id, user_id)`.

Final columns: `client_id TEXT NOT NULL` (old `app_client_id`), `user_id UUID NOT NULL` (old `global_user_id`), `pairwise_sub TEXT NOT NULL`, `relay_email TEXT NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `revoked_at TIMESTAMPTZ NULL`.

PK/FKs: PK `(client_id, user_id)`; FK `client_id -> control.oauth_clients(client_id) ON DELETE CASCADE`; FK `user_id -> auth.users(id) ON DELETE CASCADE`; index `pairwise_sub`; active unique index on `relay_email` where non-null and `revoked_at IS NULL`.

### `auth.oauth_grants`

Old: `zeroship.oauth_grants` -> `auth.oauth_grants`.
Creating migration: `V0004__control.sql`.
Purpose: end-user OAuth consent grant: the granted scope set for a `(user_id, client_id)` pair. This is the single table for the P0-proposed consent state; do not create `auth.consents`.

Final columns: `user_id UUID NOT NULL`, `client_id TEXT NOT NULL`, `granted_scopes TEXT[] NOT NULL`, `granted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `last_used_at TIMESTAMPTZ NULL`, `remember_expires_at TIMESTAMPTZ NULL` (P1 OP addition for remembered-consent reprompt skip).

PK/FKs: PK `(user_id, client_id)`; FK `user_id -> auth.users(id) ON DELETE CASCADE`; FK `client_id -> control.oauth_clients(client_id) ON DELETE CASCADE`.

### `auth.oauth_authorization_codes`

Old: new P1 OP table; no hand-authored current table. Hydra currently owns this in its vendor schema.
Purpose: single-use OAuth authorization code store with PKCE, nonce, redirect URI, and subject binding.

Final key columns: `code_hash BYTEA NOT NULL`, `client_id TEXT NOT NULL`, `user_id UUID NOT NULL`, `redirect_uri TEXT NOT NULL`, `pkce_challenge TEXT NOT NULL`, `pkce_method TEXT NOT NULL`, `nonce TEXT NULL`, `requested_scopes TEXT[] NOT NULL`, `granted_scopes TEXT[] NOT NULL`, `auth_time TIMESTAMPTZ NOT NULL`, `amr TEXT[] NOT NULL DEFAULT '{}'`, `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `expires_at TIMESTAMPTZ NOT NULL`, `consumed_at TIMESTAMPTZ NULL`.

PK/FKs: PK `code_hash`; FK `client_id -> control.oauth_clients(client_id) ON DELETE CASCADE`; FK `user_id -> auth.users(id) ON DELETE CASCADE`.

### `auth.oauth_refresh_tokens`

Old: new P1 OP table; no hand-authored current table. Hydra currently owns refresh families.
Purpose: platform-owned refresh-token rotation family and reuse-detection state.

Final key columns: `token_hash BYTEA NOT NULL`, `refresh_family_id TEXT NOT NULL`, `client_id TEXT NOT NULL`, `user_id UUID NOT NULL`, `granted_scopes TEXT[] NOT NULL`, `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `expires_at TIMESTAMPTZ NOT NULL`, `rotated_at TIMESTAMPTZ NULL`, `replaced_by_token_hash BYTEA NULL`, `revoked_at TIMESTAMPTZ NULL`, `last_used_at TIMESTAMPTZ NULL`.

PK/FKs: PK `token_hash`; FK `client_id -> control.oauth_clients(client_id) ON DELETE CASCADE`; FK `user_id -> auth.users(id) ON DELETE CASCADE`; index/unique family policy on `(refresh_family_id, client_id, user_id)` per OP spec.

### `auth.signing_keys`

Old: `zeroship.jwk_key_state` -> `auth.signing_keys`.
Creating migration: `V0002__auth.sql`.
Purpose: signing-key/JWKS rotation state. P1 OP should expand this from "state row" into the platform AS signing key registry.

Final columns: `key_set TEXT NOT NULL` (old `set_name`), `kid TEXT NOT NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, plus P1 OP key material/status columns such as `algorithm`, `public_jwk`, `private_key_enc`, `activated_at`, `retired_at`.

PK/FKs: PK `(key_set, kid)`; no FKs.

### `auth.token_revocations`

Old: `zeroship.token_revocations` -> `auth.token_revocations`.
Creating migration: `V0002__auth.sql`.
Purpose: cross-node token-family revocation marker for app tokens.

Final columns: `client_id TEXT NOT NULL`, `token_subject TEXT NOT NULL` (old `sub`; projected token `sub`, not a platform-user FK), `revoked_after TIMESTAMPTZ NOT NULL DEFAULT NOW()`.

PK/FKs: PK `(client_id, token_subject)`; FK `client_id -> control.oauth_clients(client_id) ON DELETE CASCADE`.

### `auth.dpop_jtis`

Old: `zeroship.dpop_jti` -> `auth.dpop_jtis`.
Creating migration: `V0002__auth.sql`.
Purpose: DPoP proof replay cache.

Final columns: `jti TEXT NOT NULL`, `inserted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`.

PK/FKs: PK `jti`; no FKs.

### `auth.rate_limits`

Old: `zeroship.rate_limits` -> `auth.rate_limits`.
Creating migration: `V0002__auth.sql`; TTL hardening in `V0033__auth_rate_limits_ttl.sql`.
Purpose: auth login throttling state and relay sentinel cleanup substrate.

Final columns: `bucket_key TEXT NOT NULL`, `tokens REAL NOT NULL`, `updated_at TIMESTAMPTZ NOT NULL`.

PK/FKs: PK `bucket_key`; no FKs.

### `auth.audit_events`

Old: `zeroship.audit_events` -> `auth.audit_events`.
Creating migration: `V0002__auth.sql`.
Purpose: auth service structured event log with append-only trigger.

Final columns: `id BIGSERIAL NOT NULL`, `occurred_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `event_type TEXT NOT NULL`, `outcome TEXT NOT NULL`, `actor_user_id UUID NULL`, `client_id TEXT NULL`, `request_id TEXT NULL`, `ip INET NULL`, `user_agent TEXT NULL`, `auth_method TEXT NULL`, `detail JSONB NULL`.

PK/FKs: PK `id`; optional FK `actor_user_id -> auth.users(id)` may be added if retention/anonymization behavior allows it.

### `auth.cron_state`

Old: `zeroship.cron_state` -> `auth.cron_state`.
Creating migration: `V0002__auth.sql`.
Purpose: auth cron bookkeeping.

Final columns: `key TEXT NOT NULL`, `last_rotated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `notes TEXT NULL`.

PK/FKs: PK `key`; no FKs.

### `control.oauth_clients`

Old: `zeroship.oauth_clients` -> `control.oauth_clients`.
Creating migration: `V0004__control.sql`.
Purpose: authoritative OAuth client registry. It is no longer a Hydra mirror.

Final columns: `client_id TEXT NOT NULL`, `client_name TEXT NOT NULL`, `client_uri TEXT NULL`, `logo_uri TEXT NULL`, `redirect_uris TEXT[] NOT NULL`, `scopes TEXT[] NOT NULL`, `skip_consent BOOLEAN NOT NULL DEFAULT FALSE`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `created_by_user_id UUID NULL` (old `created_by`). Delete stale `hydra_client_id`.

PK/FKs: PK `client_id`; FK `created_by_user_id -> auth.users(id)`.

### `control.app_oauth_clients`

Old: `zeroship.app_oauth_clients` -> `control.app_oauth_clients`.
Creating migration: `V0005__control_app_oauth_clients.sql`.
Purpose: per-hosted-app extension of the OAuth client registry: app link and pairwise sector.

Final columns: `app_id UUID NOT NULL`, `client_id TEXT NOT NULL`, `sector_identifier TEXT NOT NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `updated_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`.

PK/FKs: PK/FK `app_id -> control.apps(id) ON DELETE CASCADE`; unique/FK `client_id -> control.oauth_clients(client_id) ON DELETE CASCADE`.

### `control.app_scope_defs`

Old: `zeroship.app_scope_defs` -> `control.app_scope_defs`.
Creating migration: `V0007__control_app_scope_defs.sql`.
Purpose: app-declared custom end-user OAuth scope registry.

Final columns: `app_id UUID NOT NULL`, `scope_id TEXT NOT NULL`, `label TEXT NOT NULL`, `description TEXT NULL`.

PK/FKs: PK `(app_id, scope_id)`; FK `app_id -> control.apps(id) ON DELETE CASCADE`.

### `control.permission_tokens`

Old: `zeroship.permission_tokens` -> `control.permission_tokens`.
Creating migration: `V0004__control.sql`.
Purpose: platform permission tokens, including PATs and OAuth-derived control tokens.

Final columns: `id UUID NOT NULL`, `user_id UUID NOT NULL` (old `owner_id`), `kind TEXT NOT NULL CHECK (kind IN ('pat','oauth_grant'))`, `client_id TEXT NULL`, `name TEXT NOT NULL`, `policies JSONB NOT NULL`, `policy_hash TEXT NOT NULL`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `expires_at TIMESTAMPTZ NULL`, `revoked_at TIMESTAMPTZ NULL`, `last_used_at TIMESTAMPTZ NULL`.

PK/FKs: PK `id`; FK `user_id -> auth.users(id) ON DELETE CASCADE`; optional FK `client_id -> control.oauth_clients(client_id)` when `kind = 'oauth_grant'`.

### `control.principal_grants`

Old: `zeroship.principal_grants` -> `control.principal_grants`.
Creating migration: `V0060__identity_links_principal_grants.sql`.
Purpose: deploy/control-plane authorization grants. Keep the table name because it describes authorization semantics, but the user FK column is `user_id`.

Final columns: `user_id UUID NOT NULL` (old `principal_id`), `grant_name TEXT NOT NULL`.

PK/FKs: PK `(user_id, grant_name)`; FK `user_id -> auth.users(id)`.

### `control.device_authorizations`

Old: `zeroship.device_grants` -> `control.device_authorizations`.
Creating migration: `V0061__device_grants.sql`.
Purpose: pending platform-mediated OAuth device authorization for providers without a native device endpoint.

Final columns: `device_code_hash TEXT NOT NULL`, `user_code TEXT NOT NULL UNIQUE`, `status TEXT NOT NULL DEFAULT 'pending' CHECK (status IN ('pending','approved','denied'))`, `user_id UUID NULL` (old `principal_id`), `provider_refresh_token_enc BYTEA NULL` (old `gotrue_refresh_token_enc`), `provider TEXT NOT NULL`, `requested_scope TEXT NULL` (old `scope`), `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `expires_at TIMESTAMPTZ NOT NULL`, `last_polled_at TIMESTAMPTZ NULL`.

PK/FKs: PK `device_code_hash`; FK `user_id -> auth.users(id)`.

### `control.app_net_grants`

Old: `zeroship.app_net_grants` -> `control.app_net_grants`.
Creating migration: `V0058__app_net_grants.sql`.
Purpose: operator-authorized app outbound raw-TCP egress grants.

Final columns: `app_id UUID NOT NULL`, `host TEXT NOT NULL`, `port INT NOT NULL CHECK (port BETWEEN 1 AND 65535)`, `granted_by_user_id UUID NULL` (old `granted_by TEXT`, currently populated from `guard.principal_id.to_string()`), `granted_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `note TEXT NULL`.

PK/FKs: PK `(app_id, host, port)`; FK `app_id -> control.apps(id) ON DELETE CASCADE`; FK `granted_by_user_id -> auth.users(id)` when present.

### `gateway.sessions`

Old: `zeroship.gateway_sessions` -> `gateway.sessions`.
Creating/altering migrations: `V0002__auth.sql`, deferred app FK in `V0004__control.sql`, `V0008__auth_gateway_sessions_granted_scopes.sql`, `V0010__auth_gateway_sessions_auth_time_amr.sql`.
Purpose: per-origin hosted-app browser session rows for the gateway's `__Host-zeroship_app_session` cookie.

Final columns: `id UUID NOT NULL DEFAULT gen_random_uuid()`, `user_id UUID NOT NULL`, `app_id UUID NOT NULL`, `email CITEXT NULL`, `name TEXT NULL`, `avatar_url TEXT NULL`, `email_verified BOOLEAN NOT NULL DEFAULT FALSE`, `issued_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `idle_expires_at TIMESTAMPTZ NOT NULL`, `absolute_expires_at TIMESTAMPTZ NOT NULL` (old `abs_expires_at`), `revoked_at TIMESTAMPTZ NULL`, `granted_scopes TEXT[] NOT NULL DEFAULT '{}'`, `auth_time TIMESTAMPTZ NULL`, `amr TEXT[] NOT NULL DEFAULT '{}'`.

PK/FKs: PK `id`; FK `user_id -> auth.users(id)`; FK `app_id -> control.apps(id) ON DELETE CASCADE`.

### `gateway.app_session_anchors`

Old: `zeroship.app_session_anchors` -> `gateway.app_session_anchors`.
Creating migration: `V0006__auth_app_session_anchors.sql`.
Purpose: SDK reload-recovery anchor store, separate from interactive gateway sessions.

Final columns: `id UUID NOT NULL DEFAULT gen_random_uuid()`, `app_id UUID NOT NULL`, `client_id TEXT NOT NULL`, `user_id UUID NOT NULL` (old `global_user_id`), `refresh_token_enc BYTEA NOT NULL`, `refresh_family_id TEXT NOT NULL`, `granted_scopes TEXT[] NOT NULL DEFAULT '{}'`, `created_at TIMESTAMPTZ NOT NULL DEFAULT NOW()`, `absolute_expires_at TIMESTAMPTZ NOT NULL` (old `abs_expires_at`), `revoked_at TIMESTAMPTZ NULL`.

PK/FKs: PK `id`; FK `app_id -> control.apps(id) ON DELETE CASCADE`; FK `client_id -> control.oauth_clients(client_id) ON DELETE CASCADE`; FK `user_id -> auth.users(id) ON DELETE CASCADE`.

## 4. Merges and deletions

| Current shape | Final shape | Decision |
| --- | --- | --- |
| `zeroship.identity_links` + `zeroship.federated_identities` | `auth.federated_identities` | Merge. The audit confirms both tables are the same concept: `(provider, provider-local subject) -> zeroship.users(id)`. `identity_links.principal_id` and `federated_identities.user_id` target the same platform user table; `identity_links.provider_subject` and `federated_identities.subject` are the same external subject. Keep the richer `federated_identities` name and column set. |
| `zeroship.device_grants` | `control.device_authorizations` | Rename. The row is a pending device authorization, not an issued grant. Also remove provider-specific `gotrue_` column naming. |
| `zeroship.oauth_grants` vs P0 `auth.consents` | `auth.oauth_grants` only | Keep `oauth_grants`. It is already the end-user consent-grant ledger and the task's grant taxonomy reserves that name for this meaning. P1 should add remembered-consent fields here instead of creating `auth.consents`. |
| `zeroship.oauth_clients` + `zeroship.app_oauth_clients` | `control.oauth_clients` + `control.app_oauth_clients` | Keep two tables. `oauth_clients` is the authoritative generic RP/client registry; `app_oauth_clients` is the hosted-app extension with `app_id` and `sector_identifier`. Merging would force nullable app fields for console/non-app clients. Delete `hydra_client_id`. |
| `zeroship.jwk_key_state` | `auth.signing_keys` | Rename/expand. The OP now owns signing keys; the final table should not be framed as a missing Hydra/JWK cron state workaround. |
| `zeroship.dpop_jti` | `auth.dpop_jtis` | Rename to plural table name. |
| `zeroship.gateway_sessions` | `gateway.sessions` | Move to gateway schema and shorten the table name; the schema supplies the gateway qualifier. |
| Relay alias storage | `auth.app_user_identities.relay_email` | No separate relay-alias table exists in migrations. Keep the alias on the pairwise identity row with the active partial unique index. |

## 5. Full rename map

This map is intentionally flat and implementation-oriented. Rows marked `delete` should not get replacement columns. Rows marked `merge` are absorbed into the target table, not kept as aliases.

| Old object | New object | Action |
| --- | --- | --- |
| `zeroship.users` | `auth.users` | table move |
| `zeroship.users.id` | `auth.users.id` | keep |
| `zeroship.users.email` | `auth.users.email` | keep |
| `zeroship.users.email_verified_at` | `auth.users.email_verified_at` | keep |
| `zeroship.users.name` | `auth.users.name` | keep |
| `zeroship.users.avatar_url` | `auth.users.avatar_url` | keep |
| `zeroship.users.password_hash` | `auth.users.password_hash` | keep |
| `zeroship.users.credential_version` | `auth.users.credential_version` | keep |
| `zeroship.users.locked_until` | `auth.users.lock_expires_at` | rename |
| `zeroship.users.disabled_at` | `auth.users.disabled_at` | keep |
| `zeroship.users.created_at` | `auth.users.created_at` | keep |
| `zeroship.users.updated_at` | `auth.users.updated_at` | keep |
| `zeroship.users.last_login_at` | `auth.users.last_login_at` | keep |
| `zeroship.users.failed_login_count` | `auth.users.failed_login_count` | keep |
| `zeroship.users.deletion_requested_at` | `auth.users.deletion_requested_at` | keep |
| `zeroship.users.deletion_scheduled_for` | `auth.users.deletion_scheduled_at` | rename |
| `zeroship.users.anonymized_at` | `auth.users.anonymized_at` | keep |
| `zeroship.federated_identities` | `auth.federated_identities` | table move; merged target |
| `zeroship.federated_identities.id` | `auth.federated_identities.id` | keep |
| `zeroship.federated_identities.user_id` | `auth.federated_identities.user_id` | keep |
| `zeroship.federated_identities.provider` | `auth.federated_identities.provider` | keep |
| `zeroship.federated_identities.subject` | `auth.federated_identities.provider_subject` | rename |
| `zeroship.federated_identities.email_at_link` | `auth.federated_identities.email_at_link` | keep |
| `zeroship.federated_identities.raw_profile` | `auth.federated_identities.raw_profile` | keep |
| `zeroship.federated_identities.linked_at` | `auth.federated_identities.linked_at` | keep |
| `zeroship.identity_links` | `auth.federated_identities` | merge/delete old table |
| `zeroship.identity_links.principal_id` | `auth.federated_identities.user_id` | rename/merge |
| `zeroship.identity_links.provider` | `auth.federated_identities.provider` | merge |
| `zeroship.identity_links.provider_subject` | `auth.federated_identities.provider_subject` | merge |
| `zeroship.identity_links.email` | `auth.federated_identities.email_at_link` | rename/merge |
| `zeroship.identity_links.created_at` | `auth.federated_identities.linked_at` | rename/merge |
| `zeroship.idp_sessions` | `auth.idp_sessions` | table move |
| `zeroship.idp_sessions.id` | `auth.idp_sessions.id` | keep |
| `zeroship.idp_sessions.user_id` | `auth.idp_sessions.user_id` | keep |
| `zeroship.idp_sessions.auth_method` | `auth.idp_sessions.auth_method` | keep |
| `zeroship.idp_sessions.amr` | `auth.idp_sessions.amr` | keep protocol claim |
| `zeroship.idp_sessions.acr` | `auth.idp_sessions.acr` | keep protocol claim |
| `zeroship.idp_sessions.auth_time` | `auth.idp_sessions.auth_time` | keep protocol claim |
| `zeroship.idp_sessions.credential_version` | `auth.idp_sessions.credential_version` | keep |
| `zeroship.idp_sessions.idle_expires_at` | `auth.idp_sessions.idle_expires_at` | keep |
| `zeroship.idp_sessions.abs_expires_at` | `auth.idp_sessions.absolute_expires_at` | rename |
| `zeroship.idp_sessions.revoked_at` | `auth.idp_sessions.revoked_at` | keep |
| `zeroship.magic_links` | `auth.magic_links` | table move |
| `zeroship.magic_links.token_hash` | `auth.magic_links.token_hash` | keep |
| `zeroship.magic_links.email` | `auth.magic_links.email` | keep |
| `zeroship.magic_links.user_id` | `auth.magic_links.user_id` | keep |
| `zeroship.magic_links.csrf_nonce` | `auth.magic_links.csrf_nonce` | keep |
| `zeroship.magic_links.purpose` | `auth.magic_links.purpose` | keep |
| `zeroship.magic_links.request_ip` | `auth.magic_links.request_ip` | keep |
| `zeroship.magic_links.request_ua` | `auth.magic_links.request_user_agent` | rename |
| `zeroship.magic_links.issued_at` | `auth.magic_links.issued_at` | keep |
| `zeroship.magic_links.expires_at` | `auth.magic_links.expires_at` | keep |
| `zeroship.magic_links.consumed_pending_at` | `auth.magic_links.consumed_pending_at` | keep |
| `zeroship.magic_links.consumed_at` | `auth.magic_links.consumed_at` | keep |
| `zeroship.magic_completions` | `auth.magic_completions` | table move |
| `zeroship.magic_completions.csrf_nonce` | `auth.magic_completions.csrf_nonce` | keep |
| `zeroship.magic_completions.code` | `auth.magic_completions.code` | keep |
| `zeroship.magic_completions.email` | `auth.magic_completions.email` | keep |
| `zeroship.magic_completions.login_challenge` | `auth.magic_completions.login_challenge` | keep |
| `zeroship.magic_completions.attempts` | `auth.magic_completions.attempts` | keep |
| `zeroship.magic_completions.expires_at` | `auth.magic_completions.expires_at` | keep |
| `zeroship.magic_completions.consumed_pending_at` | `auth.magic_completions.consumed_pending_at` | keep |
| `zeroship.magic_completions.consumed_at` | `auth.magic_completions.consumed_at` | keep |
| `zeroship.email_verifications` | `auth.email_verifications` | table move |
| `zeroship.email_verifications.token_hash` | `auth.email_verifications.token_hash` | keep |
| `zeroship.email_verifications.user_id` | `auth.email_verifications.user_id` | keep |
| `zeroship.email_verifications.email` | `auth.email_verifications.email` | keep |
| `zeroship.email_verifications.issued_at` | `auth.email_verifications.issued_at` | keep |
| `zeroship.email_verifications.expires_at` | `auth.email_verifications.expires_at` | keep |
| `zeroship.email_verifications.consumed_at` | `auth.email_verifications.consumed_at` | keep |
| `zeroship.email_suppressions` | `auth.email_suppressions` | table move |
| `zeroship.email_suppressions.email` | `auth.email_suppressions.email` | keep |
| `zeroship.email_suppressions.reason` | `auth.email_suppressions.reason` | keep |
| `zeroship.email_suppressions.suppressed_at` | `auth.email_suppressions.suppressed_at` | keep |
| `zeroship.email_suppressions.provider_msg` | `auth.email_suppressions.provider_message` | rename |
| `zeroship.totp_credentials` | `auth.totp_credentials` | table move |
| `zeroship.totp_credentials.user_id` | `auth.totp_credentials.user_id` | keep |
| `zeroship.totp_credentials.encrypted_secret` | `auth.totp_credentials.encrypted_secret` | keep |
| `zeroship.totp_credentials.confirmed_at` | `auth.totp_credentials.confirmed_at` | keep |
| `zeroship.totp_credentials.created_at` | `auth.totp_credentials.created_at` | keep |
| `zeroship.totp_backup_codes` | `auth.totp_backup_codes` | table move |
| `zeroship.totp_backup_codes.id` | `auth.totp_backup_codes.id` | keep |
| `zeroship.totp_backup_codes.user_id` | `auth.totp_backup_codes.user_id` | keep |
| `zeroship.totp_backup_codes.code_hash` | `auth.totp_backup_codes.code_hash` | keep |
| `zeroship.totp_backup_codes.used_at` | `auth.totp_backup_codes.used_at` | keep |
| `zeroship.totp_backup_codes.created_at` | `auth.totp_backup_codes.created_at` | keep |
| `zeroship.app_user_identities` | `auth.app_user_identities` | table move |
| `zeroship.app_user_identities.app_client_id` | `auth.app_user_identities.client_id` | rename |
| `zeroship.app_user_identities.global_user_id` | `auth.app_user_identities.user_id` | rename |
| `zeroship.app_user_identities.pairwise_sub` | `auth.app_user_identities.pairwise_sub` | keep projected subject |
| `zeroship.app_user_identities.relay_email` | `auth.app_user_identities.relay_email` | keep |
| `zeroship.app_user_identities.created_at` | `auth.app_user_identities.created_at` | keep |
| `zeroship.app_user_identities.revoked_at` | `auth.app_user_identities.revoked_at` | keep |
| `zeroship.oauth_grants` | `auth.oauth_grants` | table move; keep name instead of `consents` |
| `zeroship.oauth_grants.user_id` | `auth.oauth_grants.user_id` | keep |
| `zeroship.oauth_grants.client_id` | `auth.oauth_grants.client_id` | keep |
| `zeroship.oauth_grants.granted_scopes` | `auth.oauth_grants.granted_scopes` | keep |
| `zeroship.oauth_grants.granted_at` | `auth.oauth_grants.granted_at` | keep |
| `zeroship.oauth_grants.updated_at` | `auth.oauth_grants.updated_at` | keep |
| `zeroship.oauth_grants.last_used_at` | `auth.oauth_grants.last_used_at` | keep |
| `(new)` | `auth.oauth_grants.remember_expires_at` | P1 OP addition; no `auth.consents` table |
| `(new)` | `auth.oauth_authorization_codes` | P1 OP addition |
| `(new)` | `auth.oauth_refresh_tokens` | P1 OP addition |
| `zeroship.jwk_key_state` | `auth.signing_keys` | rename/expand table |
| `zeroship.jwk_key_state.set_name` | `auth.signing_keys.key_set` | rename |
| `zeroship.jwk_key_state.kid` | `auth.signing_keys.kid` | keep protocol field |
| `zeroship.jwk_key_state.created_at` | `auth.signing_keys.created_at` | keep |
| `zeroship.token_revocations` | `auth.token_revocations` | table move |
| `zeroship.token_revocations.client_id` | `auth.token_revocations.client_id` | keep |
| `zeroship.token_revocations.sub` | `auth.token_revocations.token_subject` | rename |
| `zeroship.token_revocations.revoked_after` | `auth.token_revocations.revoked_after` | keep |
| `zeroship.dpop_jti` | `auth.dpop_jtis` | rename table to plural |
| `zeroship.dpop_jti.jti` | `auth.dpop_jtis.jti` | keep protocol field |
| `zeroship.dpop_jti.inserted_at` | `auth.dpop_jtis.inserted_at` | keep |
| `zeroship.rate_limits` | `auth.rate_limits` | table move |
| `zeroship.rate_limits.bucket_key` | `auth.rate_limits.bucket_key` | keep |
| `zeroship.rate_limits.tokens` | `auth.rate_limits.tokens` | keep |
| `zeroship.rate_limits.updated_at` | `auth.rate_limits.updated_at` | keep |
| `zeroship.audit_events` | `auth.audit_events` | table move |
| `zeroship.audit_events.id` | `auth.audit_events.id` | keep |
| `zeroship.audit_events.occurred_at` | `auth.audit_events.occurred_at` | keep |
| `zeroship.audit_events.event_type` | `auth.audit_events.event_type` | keep |
| `zeroship.audit_events.outcome` | `auth.audit_events.outcome` | keep |
| `zeroship.audit_events.actor_user_id` | `auth.audit_events.actor_user_id` | keep |
| `zeroship.audit_events.client_id` | `auth.audit_events.client_id` | keep |
| `zeroship.audit_events.request_id` | `auth.audit_events.request_id` | keep |
| `zeroship.audit_events.ip` | `auth.audit_events.ip` | keep |
| `zeroship.audit_events.user_agent` | `auth.audit_events.user_agent` | keep |
| `zeroship.audit_events.auth_method` | `auth.audit_events.auth_method` | keep |
| `zeroship.audit_events.detail` | `auth.audit_events.detail` | keep |
| `zeroship.cron_state` | `auth.cron_state` | table move |
| `zeroship.cron_state.key` | `auth.cron_state.key` | keep |
| `zeroship.cron_state.last_rotated_at` | `auth.cron_state.last_rotated_at` | keep |
| `zeroship.cron_state.notes` | `auth.cron_state.notes` | keep |
| `zeroship.oauth_clients` | `control.oauth_clients` | table move; authoritative registry |
| `zeroship.oauth_clients.client_id` | `control.oauth_clients.client_id` | keep |
| `zeroship.oauth_clients.client_name` | `control.oauth_clients.client_name` | keep |
| `zeroship.oauth_clients.client_uri` | `control.oauth_clients.client_uri` | keep |
| `zeroship.oauth_clients.logo_uri` | `control.oauth_clients.logo_uri` | keep |
| `zeroship.oauth_clients.redirect_uris` | `control.oauth_clients.redirect_uris` | keep |
| `zeroship.oauth_clients.scopes` | `control.oauth_clients.scopes` | keep |
| `zeroship.oauth_clients.skip_consent` | `control.oauth_clients.skip_consent` | keep |
| `zeroship.oauth_clients.created_at` | `control.oauth_clients.created_at` | keep |
| `zeroship.oauth_clients.created_by` | `control.oauth_clients.created_by_user_id` | rename |
| `zeroship.oauth_clients.hydra_client_id` | `(delete)` | stale Hydra mirror column |
| `zeroship.app_oauth_clients` | `control.app_oauth_clients` | table move |
| `zeroship.app_oauth_clients.app_id` | `control.app_oauth_clients.app_id` | keep |
| `zeroship.app_oauth_clients.client_id` | `control.app_oauth_clients.client_id` | keep |
| `zeroship.app_oauth_clients.sector_identifier` | `control.app_oauth_clients.sector_identifier` | keep |
| `zeroship.app_oauth_clients.created_at` | `control.app_oauth_clients.created_at` | keep |
| `zeroship.app_oauth_clients.updated_at` | `control.app_oauth_clients.updated_at` | keep |
| `zeroship.app_scope_defs` | `control.app_scope_defs` | table move |
| `zeroship.app_scope_defs.app_id` | `control.app_scope_defs.app_id` | keep |
| `zeroship.app_scope_defs.scope_id` | `control.app_scope_defs.scope_id` | keep |
| `zeroship.app_scope_defs.label` | `control.app_scope_defs.label` | keep |
| `zeroship.app_scope_defs.description` | `control.app_scope_defs.description` | keep |
| `zeroship.permission_tokens` | `control.permission_tokens` | table move |
| `zeroship.permission_tokens.id` | `control.permission_tokens.id` | keep |
| `zeroship.permission_tokens.owner_id` | `control.permission_tokens.user_id` | rename |
| `zeroship.permission_tokens.kind` | `control.permission_tokens.kind` | keep |
| `zeroship.permission_tokens.client_id` | `control.permission_tokens.client_id` | keep |
| `zeroship.permission_tokens.name` | `control.permission_tokens.name` | keep |
| `zeroship.permission_tokens.policies` | `control.permission_tokens.policies` | keep |
| `zeroship.permission_tokens.policy_hash` | `control.permission_tokens.policy_hash` | keep |
| `zeroship.permission_tokens.created_at` | `control.permission_tokens.created_at` | keep |
| `zeroship.permission_tokens.expires_at` | `control.permission_tokens.expires_at` | keep |
| `zeroship.permission_tokens.revoked_at` | `control.permission_tokens.revoked_at` | keep |
| `zeroship.permission_tokens.last_used_at` | `control.permission_tokens.last_used_at` | keep |
| `zeroship.principal_grants` | `control.principal_grants` | table move; keep grant table name |
| `zeroship.principal_grants.principal_id` | `control.principal_grants.user_id` | rename |
| `zeroship.principal_grants.grant_name` | `control.principal_grants.grant_name` | keep |
| `zeroship.device_grants` | `control.device_authorizations` | rename |
| `zeroship.device_grants.device_code_hash` | `control.device_authorizations.device_code_hash` | keep |
| `zeroship.device_grants.user_code` | `control.device_authorizations.user_code` | keep |
| `zeroship.device_grants.status` | `control.device_authorizations.status` | keep |
| `zeroship.device_grants.principal_id` | `control.device_authorizations.user_id` | rename |
| `zeroship.device_grants.gotrue_refresh_token_enc` | `control.device_authorizations.provider_refresh_token_enc` | rename |
| `zeroship.device_grants.provider` | `control.device_authorizations.provider` | keep |
| `zeroship.device_grants.scope` | `control.device_authorizations.requested_scope` | rename |
| `zeroship.device_grants.created_at` | `control.device_authorizations.created_at` | keep |
| `zeroship.device_grants.expires_at` | `control.device_authorizations.expires_at` | keep |
| `zeroship.device_grants.last_polled_at` | `control.device_authorizations.last_polled_at` | keep |
| `zeroship.app_net_grants` | `control.app_net_grants` | table move |
| `zeroship.app_net_grants.app_id` | `control.app_net_grants.app_id` | keep |
| `zeroship.app_net_grants.host` | `control.app_net_grants.host` | keep |
| `zeroship.app_net_grants.port` | `control.app_net_grants.port` | keep |
| `zeroship.app_net_grants.granted_by` | `control.app_net_grants.granted_by_user_id` | rename/type to UUID |
| `zeroship.app_net_grants.granted_at` | `control.app_net_grants.granted_at` | keep |
| `zeroship.app_net_grants.note` | `control.app_net_grants.note` | keep |
| `zeroship.gateway_sessions` | `gateway.sessions` | rename/move |
| `zeroship.gateway_sessions.id` | `gateway.sessions.id` | keep |
| `zeroship.gateway_sessions.user_id` | `gateway.sessions.user_id` | keep |
| `zeroship.gateway_sessions.app_id` | `gateway.sessions.app_id` | keep |
| `zeroship.gateway_sessions.email` | `gateway.sessions.email` | keep |
| `zeroship.gateway_sessions.name` | `gateway.sessions.name` | keep |
| `zeroship.gateway_sessions.avatar_url` | `gateway.sessions.avatar_url` | keep |
| `zeroship.gateway_sessions.email_verified` | `gateway.sessions.email_verified` | keep |
| `zeroship.gateway_sessions.issued_at` | `gateway.sessions.issued_at` | keep |
| `zeroship.gateway_sessions.idle_expires_at` | `gateway.sessions.idle_expires_at` | keep |
| `zeroship.gateway_sessions.abs_expires_at` | `gateway.sessions.absolute_expires_at` | rename |
| `zeroship.gateway_sessions.revoked_at` | `gateway.sessions.revoked_at` | keep |
| `zeroship.gateway_sessions.granted_scopes` | `gateway.sessions.granted_scopes` | keep |
| `zeroship.gateway_sessions.auth_time` | `gateway.sessions.auth_time` | keep protocol claim |
| `zeroship.gateway_sessions.amr` | `gateway.sessions.amr` | keep protocol claim |
| `zeroship.app_session_anchors` | `gateway.app_session_anchors` | table move |
| `zeroship.app_session_anchors.id` | `gateway.app_session_anchors.id` | keep |
| `zeroship.app_session_anchors.app_id` | `gateway.app_session_anchors.app_id` | keep |
| `zeroship.app_session_anchors.client_id` | `gateway.app_session_anchors.client_id` | keep |
| `zeroship.app_session_anchors.global_user_id` | `gateway.app_session_anchors.user_id` | rename |
| `zeroship.app_session_anchors.refresh_token_enc` | `gateway.app_session_anchors.refresh_token_enc` | keep |
| `zeroship.app_session_anchors.refresh_family_id` | `gateway.app_session_anchors.refresh_family_id` | keep |
| `zeroship.app_session_anchors.granted_scopes` | `gateway.app_session_anchors.granted_scopes` | keep |
| `zeroship.app_session_anchors.created_at` | `gateway.app_session_anchors.created_at` | keep |
| `zeroship.app_session_anchors.abs_expires_at` | `gateway.app_session_anchors.absolute_expires_at` | rename |
| `zeroship.app_session_anchors.revoked_at` | `gateway.app_session_anchors.revoked_at` | keep |

## 6. Open questions

1. **Separate operator population:** the audit did not find one. `principal_id` is only a code/authz concept today and all persisted rows target `zeroship.users(id)`. If product later requires non-user service principals/operators, add a distinct actor table then; do not keep `principal_id` in DB as a speculative placeholder now.
2. **`app_net_grants.granted_by_user_id`:** current code writes `guard.principal_id.to_string()` into `granted_by TEXT`, so this redesign types it as `UUID` and FKs it to `auth.users(id)`. If P1 needs system-authored grants, add an explicit nullable `granted_by_system TEXT` or an actor-union design rather than overloading a user FK.
3. **Final OP token table internals:** `oauth_authorization_codes`, `oauth_refresh_tokens`, and `signing_keys` names are canonical here, but exact crypto/status columns should be finalized in the OP AS spec. Do not create differently named tables while that spec is refined.
4. **DPoP replay-cache placement:** this proposal puts `dpop_jtis` in `auth` because it is token-security state. If P1 proves the resource-server replay cache must be gateway-local for latency, keep the same plural table name under `gateway`, not the current singular `dpop_jti`.
5. **Audit-event FKs and retention:** `auth.audit_events.actor_user_id` can remain nullable without a hard FK if account erasure/retention needs append-only historical rows after user deletion. The name is already clear and should not become `principal_id`.
