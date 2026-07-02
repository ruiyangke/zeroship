import { table, t } from "@zeroship/migrate";
import { raw, sequence } from "@zeroship/migrate/pg";

export const name = "auth_oauth_tables";

export function up() {
  table("app_session_anchors", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      app_id: t.uuid().notNull(),
      client_id: t.text().notNull(),
      global_user_id: t.uuid().notNull(),
      refresh_token_enc: t.bytes().notNull(),
      refresh_family_id: t.text().notNull(),
      granted_scopes: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      abs_expires_at: t.timestamp().notNull(),
      revoked_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for text[]
  raw({ sql: "ALTER TABLE ONLY zeroship.app_session_anchors ALTER COLUMN granted_scopes TYPE text[] USING granted_scopes::text[]", reason: "column app_session_anchors.granted_scopes uses PostgreSQL type text[], which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): array default is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.app_session_anchors ALTER COLUMN granted_scopes SET DEFAULT '{}'::text[]", reason: "column app_session_anchors.granted_scopes requires exact default '{}'::text[], which the current structural default surface/lowerer cannot emit for this table" });
  table("app_user_identities", { schema: "zeroship" }).create({
    columns: {
      app_client_id: t.text().notNull(),
      global_user_id: t.uuid().notNull(),
      pairwise_sub: t.text().notNull(),
      relay_email: t.text(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      revoked_at: t.timestamp(),
    },
    primaryKey: ["app_client_id", "global_user_id"],
  });
  table("audit_events", { schema: "zeroship" }).create({
    columns: {
      id: t.bigInt().notNull(),
      occurred_at: t.timestamp().notNull().default({ fn: "now" }),
      event_type: t.text().notNull(),
      outcome: t.text().notNull(),
      actor_user_id: t.uuid(),
      client_id: t.text(),
      request_id: t.text(),
      ip: t.text(),
      user_agent: t.text(),
      auth_method: t.text(),
      detail: t.json(),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for inet
  raw({ sql: "ALTER TABLE ONLY zeroship.audit_events ALTER COLUMN ip TYPE inet USING ip::inet", reason: "column audit_events.ip uses PostgreSQL type inet, which is not in the current closed column lexicon/lowerer use-site set" });
  sequence("audit_events_id_seq").alter({ schema: "zeroship", ownedBy: { table: "audit_events", column: "id" } });
  table("authz_decisions", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      occurred_at: t.timestamp().notNull().default({ fn: "now" }),
      actor_user_id: t.uuid(),
      token_id: t.uuid(),
      action: t.text().notNull(),
      resource_type: t.text().notNull(),
      resource_id: t.text(),
      decision: t.text().notNull(),
      matched_policies: t.text().notNull(),
      request_ip: t.text(),
      request_id: t.text(),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for text[]
  raw({ sql: "ALTER TABLE ONLY zeroship.authz_decisions ALTER COLUMN matched_policies TYPE text[] USING matched_policies::text[]", reason: "column authz_decisions.matched_policies uses PostgreSQL type text[], which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): array default is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.authz_decisions ALTER COLUMN matched_policies SET DEFAULT '{}'::text[]", reason: "column authz_decisions.matched_policies requires exact default '{}'::text[], which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): add structural column type support for inet
  raw({ sql: "ALTER TABLE ONLY zeroship.authz_decisions ALTER COLUMN request_ip TYPE inet USING request_ip::inet", reason: "column authz_decisions.request_ip uses PostgreSQL type inet, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint authz_decisions.authz_decisions_decision_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.authz_decisions ADD CONSTRAINT authz_decisions_decision_check CHECK ((decision = ANY (ARRAY['allow'::text, 'deny'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("cron_state", { schema: "zeroship" }).create({
    columns: {
      key: t.text().notNull(),
      last_rotated_at: t.timestamp().notNull().default({ fn: "now" }),
      notes: t.text(),
    },
    primaryKey: ["key"],
  });
  table("device_grants", { schema: "zeroship" }).create({
    columns: {
      device_code_hash: t.text().notNull(),
      user_code: t.text().notNull(),
      status: t.text().notNull().default("pending"),
      principal_id: t.uuid(),
      platform_access_token_enc: t.bytes(),
      provider: t.text().notNull(),
      scope: t.text(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      expires_at: t.timestamp().notNull(),
      last_polled_at: t.timestamp(),
      auth_credential_version: t.bigInt().notNull().default(0),
      client_id: t.text(),
      sid: t.text(),
      poll_interval_secs: t.integer().notNull().default(5),
    },
    primaryKey: ["device_code_hash"],
  });
  // TODO(dsl-v2): CHECK constraint device_grants.device_grants_poll_interval_secs_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.device_grants ADD CONSTRAINT device_grants_poll_interval_secs_check CHECK ((poll_interval_secs > 0))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint device_grants.device_grants_status_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.device_grants ADD CONSTRAINT device_grants_status_check CHECK ((status = ANY (ARRAY['pending'::text, 'approved'::text, 'denied'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("dpop_jti", { schema: "zeroship" }).create({
    columns: {
      jti: t.text().notNull(),
      inserted_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["jti"],
  });
  table("email_suppressions", { schema: "zeroship" }).create({
    columns: {
      email: t.text().notNull(),
      reason: t.text().notNull(),
      suppressed_at: t.timestamp().notNull().default({ fn: "now" }),
      provider_msg: t.text(),
    },
    primaryKey: ["email"],
  });
  // TODO(dsl-v2): add structural column type support for public.citext
  raw({ sql: "ALTER TABLE ONLY zeroship.email_suppressions ALTER COLUMN email TYPE public.citext USING email::public.citext", reason: "column email_suppressions.email uses PostgreSQL type public.citext, which is not in the current closed column lexicon/lowerer use-site set" });
  table("email_verifications", { schema: "zeroship" }).create({
    columns: {
      token_hash: t.bytes().notNull(),
      user_id: t.uuid().notNull(),
      email: t.text().notNull(),
      issued_at: t.timestamp().notNull().default({ fn: "now" }),
      expires_at: t.timestamp().notNull(),
      consumed_at: t.timestamp(),
    },
    primaryKey: ["token_hash"],
  });
  // TODO(dsl-v2): add structural column type support for public.citext
  raw({ sql: "ALTER TABLE ONLY zeroship.email_verifications ALTER COLUMN email TYPE public.citext USING email::public.citext", reason: "column email_verifications.email uses PostgreSQL type public.citext, which is not in the current closed column lexicon/lowerer use-site set" });
  table("federated_identities", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      user_id: t.uuid().notNull(),
      provider: t.text().notNull(),
      subject: t.text().notNull(),
      email_at_link: t.text(),
      raw_profile: t.json(),
      linked_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for public.citext
  raw({ sql: "ALTER TABLE ONLY zeroship.federated_identities ALTER COLUMN email_at_link TYPE public.citext USING email_at_link::public.citext", reason: "column federated_identities.email_at_link uses PostgreSQL type public.citext, which is not in the current closed column lexicon/lowerer use-site set" });
  table("gateway_sessions", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      user_id: t.uuid().notNull(),
      app_id: t.uuid().notNull(),
      email: t.text(),
      name: t.text(),
      avatar_url: t.text(),
      email_verified: t.boolean().notNull().default(false),
      issued_at: t.timestamp().notNull().default({ fn: "now" }),
      idle_expires_at: t.timestamp().notNull(),
      abs_expires_at: t.timestamp().notNull(),
      revoked_at: t.timestamp(),
      granted_scopes: t.text().notNull(),
      auth_time: t.timestamp(),
      amr: t.text().notNull(),
      sid: t.text(),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for public.citext
  raw({ sql: "ALTER TABLE ONLY zeroship.gateway_sessions ALTER COLUMN email TYPE public.citext USING email::public.citext", reason: "column gateway_sessions.email uses PostgreSQL type public.citext, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for text[]
  raw({ sql: "ALTER TABLE ONLY zeroship.gateway_sessions ALTER COLUMN granted_scopes TYPE text[] USING granted_scopes::text[]", reason: "column gateway_sessions.granted_scopes uses PostgreSQL type text[], which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): array default is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.gateway_sessions ALTER COLUMN granted_scopes SET DEFAULT '{}'::text[]", reason: "column gateway_sessions.granted_scopes requires exact default '{}'::text[], which the current structural default surface/lowerer cannot emit for this table" });
  // TODO(dsl-v2): add structural column type support for text[]
  raw({ sql: "ALTER TABLE ONLY zeroship.gateway_sessions ALTER COLUMN amr TYPE text[] USING amr::text[]", reason: "column gateway_sessions.amr uses PostgreSQL type text[], which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): array default is not expressible in createTable column defaults yet
  raw({ sql: "ALTER TABLE ONLY zeroship.gateway_sessions ALTER COLUMN amr SET DEFAULT '{}'::text[]", reason: "column gateway_sessions.amr requires exact default '{}'::text[], which the current structural default surface/lowerer cannot emit for this table" });
  table("identity_links", { schema: "zeroship" }).create({
    columns: {
      principal_id: t.uuid().notNull(),
      provider: t.text().notNull(),
      provider_subject: t.text().notNull(),
      email: t.text(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["provider", "provider_subject"],
  });
  table("idp_sessions", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      user_id: t.uuid().notNull(),
      auth_method: t.text().notNull(),
      amr: t.text().notNull(),
      acr: t.text(),
      auth_time: t.timestamp().notNull().default({ fn: "now" }),
      credential_version: t.bigInt().notNull().default(0),
      idle_expires_at: t.timestamp().notNull(),
      abs_expires_at: t.timestamp().notNull(),
      revoked_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for text[]
  raw({ sql: "ALTER TABLE ONLY zeroship.idp_sessions ALTER COLUMN amr TYPE text[] USING amr::text[]", reason: "column idp_sessions.amr uses PostgreSQL type text[], which is not in the current closed column lexicon/lowerer use-site set" });
  table("jwk_key_state", { schema: "zeroship" }).create({
    columns: {
      set_name: t.text().notNull(),
      kid: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["set_name", "kid"],
  });
  table("magic_completions", { schema: "zeroship" }).create({
    columns: {
      csrf_nonce: t.text().notNull(),
      code: t.text().notNull(),
      email: t.text().notNull(),
      login_challenge: t.text().notNull(),
      attempts: t.integer().notNull().default(0),
      expires_at: t.timestamp().notNull(),
      consumed_pending_at: t.timestamp(),
      consumed_at: t.timestamp(),
    },
    primaryKey: ["csrf_nonce"],
  });
  // TODO(dsl-v2): add structural column type support for public.citext
  raw({ sql: "ALTER TABLE ONLY zeroship.magic_completions ALTER COLUMN email TYPE public.citext USING email::public.citext", reason: "column magic_completions.email uses PostgreSQL type public.citext, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for smallint
  raw({ sql: "ALTER TABLE ONLY zeroship.magic_completions ALTER COLUMN attempts TYPE smallint USING attempts::smallint", reason: "column magic_completions.attempts uses PostgreSQL type smallint, which is not in the current closed column lexicon/lowerer use-site set" });
  table("magic_links", { schema: "zeroship" }).create({
    columns: {
      token_hash: t.bytes().notNull(),
      email: t.text().notNull(),
      csrf_nonce: t.text().notNull(),
      purpose: t.text().notNull(),
      request_ip: t.text(),
      request_ua: t.text(),
      issued_at: t.timestamp().notNull().default({ fn: "now" }),
      expires_at: t.timestamp().notNull(),
      consumed_pending_at: t.timestamp(),
      consumed_at: t.timestamp(),
      user_id: t.uuid(),
    },
    primaryKey: ["token_hash"],
  });
  // TODO(dsl-v2): add structural column type support for public.citext
  raw({ sql: "ALTER TABLE ONLY zeroship.magic_links ALTER COLUMN email TYPE public.citext USING email::public.citext", reason: "column magic_links.email uses PostgreSQL type public.citext, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for inet
  raw({ sql: "ALTER TABLE ONLY zeroship.magic_links ALTER COLUMN request_ip TYPE inet USING request_ip::inet", reason: "column magic_links.request_ip uses PostgreSQL type inet, which is not in the current closed column lexicon/lowerer use-site set" });
  table("oauth_authorization_codes", { schema: "zeroship" }).create({
    columns: {
      code_hash: t.bytes().notNull(),
      client_id: t.text().notNull(),
      redirect_uri: t.text().notNull(),
      pkce_challenge: t.text().notNull(),
      pkce_method: t.text().notNull(),
      requested_scopes: t.text().notNull(),
      granted_scopes: t.text().notNull(),
      nonce: t.text(),
      user_id: t.uuid().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      expires_at: t.timestamp().notNull(),
      consumed_at: t.timestamp(),
      auth_credential_version: t.bigInt().notNull().default(0),
      sid: t.text().notNull(),
    },
    primaryKey: ["code_hash"],
  });
  // TODO(dsl-v2): add structural column type support for text[]
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_authorization_codes ALTER COLUMN requested_scopes TYPE text[] USING requested_scopes::text[]", reason: "column oauth_authorization_codes.requested_scopes uses PostgreSQL type text[], which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for text[]
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_authorization_codes ALTER COLUMN granted_scopes TYPE text[] USING granted_scopes::text[]", reason: "column oauth_authorization_codes.granted_scopes uses PostgreSQL type text[], which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint oauth_authorization_codes.oauth_authorization_codes_max_ttl needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_authorization_codes ADD CONSTRAINT oauth_authorization_codes_max_ttl CHECK ((expires_at <= (created_at + '00:01:00'::interval)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint oauth_authorization_codes.oauth_authorization_codes_pkce_method_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_authorization_codes ADD CONSTRAINT oauth_authorization_codes_pkce_method_check CHECK ((pkce_method = 'S256'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("oauth_clients", { schema: "zeroship" }).create({
    columns: {
      client_id: t.text().notNull(),
      client_name: t.text().notNull(),
      client_uri: t.text(),
      logo_uri: t.text(),
      redirect_uris: t.text().notNull(),
      scopes: t.text().notNull(),
      skip_consent: t.boolean().notNull().default(false),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      created_by: t.uuid(),
      client_secret_hash: t.text(),
      refresh_allowed: t.boolean().notNull().default(false),
      token_endpoint_auth_method: t.text().notNull().default("none"),
      brokered: t.boolean().notNull().default(false),
      backchannel_logout_uri: t.text(),
    },
    primaryKey: ["client_id"],
  });
  // TODO(dsl-v2): add structural column type support for text[]
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_clients ALTER COLUMN redirect_uris TYPE text[] USING redirect_uris::text[]", reason: "column oauth_clients.redirect_uris uses PostgreSQL type text[], which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for text[]
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_clients ALTER COLUMN scopes TYPE text[] USING scopes::text[]", reason: "column oauth_clients.scopes uses PostgreSQL type text[], which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint oauth_clients.oauth_clients_brokered_requires_secret_basic needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_clients ADD CONSTRAINT oauth_clients_brokered_requires_secret_basic CHECK (((brokered = false) OR (token_endpoint_auth_method = 'client_secret_basic'::text)))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint oauth_clients.oauth_clients_token_endpoint_auth_method_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_clients ADD CONSTRAINT oauth_clients_token_endpoint_auth_method_check CHECK ((token_endpoint_auth_method = ANY (ARRAY['none'::text, 'client_secret_basic'::text, 'client_secret_post'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("oauth_grants", { schema: "zeroship" }).create({
    columns: {
      user_id: t.uuid().notNull(),
      client_id: t.text().notNull(),
      granted_scopes: t.text().notNull(),
      granted_at: t.timestamp().notNull().default({ fn: "now" }),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
      last_used_at: t.timestamp(),
    },
    primaryKey: ["user_id", "client_id"],
  });
  // TODO(dsl-v2): add structural column type support for text[]
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_grants ALTER COLUMN granted_scopes TYPE text[] USING granted_scopes::text[]", reason: "column oauth_grants.granted_scopes uses PostgreSQL type text[], which is not in the current closed column lexicon/lowerer use-site set" });
  table("oauth_refresh_tokens", { schema: "zeroship" }).create({
    columns: {
      token_hash: t.bytes().notNull(),
      hash_key_version: t.integer().notNull(),
      refresh_family_id: t.text().notNull(),
      replaced_by_token_hash: t.bytes(),
      client_id: t.text().notNull(),
      user_id: t.uuid().notNull(),
      sub: t.text().notNull(),
      granted_scopes: t.text().notNull(),
      family_granted_scopes: t.text().notNull(),
      issued_at: t.timestamp().notNull().default({ fn: "now" }),
      expires_at: t.timestamp().notNull(),
      family_absolute_expires_at: t.timestamp().notNull(),
      consumed_at: t.timestamp(),
      rotated_at: t.timestamp(),
      revoked_at: t.timestamp(),
      last_used_at: t.timestamp(),
      idem_response_enc: t.bytes(),
      idem_expires_at: t.timestamp(),
    },
    primaryKey: ["token_hash"],
  });
  // TODO(dsl-v2): add structural column type support for smallint
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_refresh_tokens ALTER COLUMN hash_key_version TYPE smallint USING hash_key_version::smallint", reason: "column oauth_refresh_tokens.hash_key_version uses PostgreSQL type smallint, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for text[]
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_refresh_tokens ALTER COLUMN granted_scopes TYPE text[] USING granted_scopes::text[]", reason: "column oauth_refresh_tokens.granted_scopes uses PostgreSQL type text[], which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add structural column type support for text[]
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_refresh_tokens ALTER COLUMN family_granted_scopes TYPE text[] USING family_granted_scopes::text[]", reason: "column oauth_refresh_tokens.family_granted_scopes uses PostgreSQL type text[], which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): CHECK constraint oauth_refresh_tokens.oauth_refresh_tokens_idle_le_ceiling needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.oauth_refresh_tokens ADD CONSTRAINT oauth_refresh_tokens_idle_le_ceiling CHECK ((expires_at <= family_absolute_expires_at))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("oidc_session_clients", { schema: "zeroship" }).create({
    columns: {
      idp_session_id: t.uuid().notNull(),
      user_id: t.uuid().notNull(),
      client_id: t.text().notNull(),
      sid: t.text().notNull(),
      sub: t.text().notNull(),
      first_seen_at: t.timestamp().notNull().default({ fn: "now" }),
      last_seen_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["idp_session_id", "client_id"],
  });
  table("principal_grants", { schema: "zeroship" }).create({
    columns: {
      principal_id: t.uuid().notNull(),
      grant_name: t.text().notNull(),
    },
    primaryKey: ["principal_id", "grant_name"],
  });
  table("rate_limits", { schema: "zeroship" }).create({
    columns: {
      bucket_key: t.text().notNull(),
      tokens: t.float().notNull(),
      updated_at: t.timestamp().notNull(),
    },
    primaryKey: ["bucket_key"],
  });
  // TODO(dsl-v2): add structural column type support for real
  raw({ sql: "ALTER TABLE ONLY zeroship.rate_limits ALTER COLUMN tokens TYPE real USING tokens::real", reason: "column rate_limits.tokens uses PostgreSQL type real, which is not in the current closed column lexicon/lowerer use-site set" });
  table("signing_keys", { schema: "zeroship" }).create({
    columns: {
      kid: t.text().notNull(),
      alg: t.text().notNull(),
      public_jwk: t.json().notNull(),
      status: t.text().notNull(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      activated_at: t.timestamp(),
      retiring_at: t.timestamp(),
      retired_at: t.timestamp(),
    },
    primaryKey: ["kid"],
  });
  // TODO(dsl-v2): CHECK constraint signing_keys.signing_keys_alg_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.signing_keys ADD CONSTRAINT signing_keys_alg_check CHECK ((alg = 'EdDSA'::text))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  // TODO(dsl-v2): CHECK constraint signing_keys.signing_keys_status_check needs the Expr->SQL renderer
  raw({ sql: "ALTER TABLE ONLY zeroship.signing_keys ADD CONSTRAINT signing_keys_status_check CHECK ((status = ANY (ARRAY['active'::text, 'next'::text, 'retiring'::text])))", reason: "CHECK constraints with SQL predicates remain raw until the structural expression renderer covers this predicate" });
  table("token_revocations", { schema: "zeroship" }).create({
    columns: {
      client_id: t.text().notNull(),
      sub: t.text().notNull(),
      revoked_after: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["client_id", "sub"],
  });
  table("totp_backup_codes", { schema: "zeroship" }).create({
    columns: {
      id: t.bigInt().notNull(),
      user_id: t.uuid().notNull(),
      code_hash: t.text().notNull(),
      used_at: t.timestamp(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["id"],
  });
  table("totp_credentials", { schema: "zeroship" }).create({
    columns: {
      user_id: t.uuid().notNull(),
      encrypted_secret: t.bytes().notNull(),
      confirmed_at: t.timestamp(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
    },
    primaryKey: ["user_id"],
  });
  table("users", { schema: "zeroship" }).create({
    columns: {
      id: t.uuid().notNull().default({ fn: "genRandomUuid" }),
      email: t.text().notNull(),
      email_verified_at: t.timestamp(),
      name: t.text().notNull(),
      avatar_url: t.text(),
      password_hash: t.text(),
      credential_version: t.bigInt().notNull().default(0),
      locked_until: t.timestamp(),
      disabled_at: t.timestamp(),
      created_at: t.timestamp().notNull().default({ fn: "now" }),
      updated_at: t.timestamp().notNull().default({ fn: "now" }),
      last_login_at: t.timestamp(),
      failed_login_count: t.integer().notNull().default(0),
      deletion_requested_at: t.timestamp(),
      deletion_scheduled_for: t.timestamp(),
      anonymized_at: t.timestamp(),
    },
    primaryKey: ["id"],
  });
  // TODO(dsl-v2): add structural column type support for public.citext
  raw({ sql: "ALTER TABLE ONLY zeroship.users ALTER COLUMN email TYPE public.citext USING email::public.citext", reason: "column users.email uses PostgreSQL type public.citext, which is not in the current closed column lexicon/lowerer use-site set" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER TABLE zeroship.totp_backup_codes ALTER COLUMN id ADD GENERATED ALWAYS AS IDENTITY (\n    SEQUENCE NAME zeroship.totp_backup_codes_id_seq\n    START WITH 1\n    INCREMENT BY 1\n    NO MINVALUE\n    NO MAXVALUE\n    CACHE 1\n)", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
  // TODO(dsl-v2): add a structural operation for this exact platform DDL fragment
  raw({ sql: "ALTER TABLE ONLY zeroship.audit_events ALTER COLUMN id SET DEFAULT nextval('zeroship.audit_events_id_seq'::regclass)", reason: "this platform DDL object has no exact structural operation in the current v2 surface" });
}

export function down() {

}
