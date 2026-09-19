import {
  nextval,
  table,
  t,
  now,
  uuidV4,
  interval,
  sequence,
} from "@zeroship/migrate";

const userIdColumnsByTable: Readonly<Record<string, readonly string[]>> = {
  app_session_anchors: ["global_user_id"],
  app_user_identities: ["global_user_id"],
  audit_events: ["actor_user_id"],
  authz_decisions: ["actor_user_id"],
  device_grants: ["principal_id"],
  email_verifications: ["user_id"],
  federated_identities: ["user_id"],
  gateway_sessions: ["user_id"],
  identity_links: ["principal_id"],
  idp_sessions: ["user_id"],
  magic_links: ["user_id"],
  oauth_authorization_codes: ["user_id"],
  oauth_clients: ["created_by"],
  oauth_grants: ["user_id"],
  oidc_session_clients: ["user_id"],
  principal_grants: ["principal_id"],
  totp_backup_codes: ["user_id"],
  totp_credentials: ["user_id"],
  users: ["id"],
};

export default {
  name: "auth_oauth_tables",
  schema() {
    table("app_session_anchors", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().required().default(uuidV4()),
        app_id: t.text().required(),
        client_id: t.text().required(),
        global_user_id: t.text().required(),
        refresh_token_enc: t.bytes().required(),
        refresh_family_id: t.text().required(),
        granted_scopes: t.array(t.text(), { storage: "native" }).required().default([]),
        created_at: t.timestamp().required().default(now()),
        abs_expires_at: t.timestamp().required(),
        revoked_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("app_user_identities", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        app_client_id: t.text().required(),
        global_user_id: t.text().required(),
        pairwise_sub: t.text().required(),
        relay_email: t.text(),
        created_at: t.timestamp().required().default(now()),
        revoked_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("app_user_identities", { schema: "zeroship" }).unique("app_user_identities_natural_key").add({ columns: ["app_client_id", "global_user_id"] });
    table("audit_events", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().default(nextval("audit_events_id_seq", { schema: "zeroship" })),
        occurred_at: t.timestamp().required().default(now()),
        event_type: t.text().required(),
        outcome: t.text().required(),
        actor_user_id: t.text(),
        client_id: t.text(),
        request_id: t.text(),
        ip: t.inet(),
        user_agent: t.text(),
        auth_method: t.text(),
        detail: t.json(),
      },
      primaryKey: ["id"],
    });
    sequence("audit_events_id_seq").alter({ schema: "zeroship", ownedBy: { table: "audit_events", column: "id" } });
    table("authz_decisions", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().required().default(uuidV4()),
        occurred_at: t.timestamp().required().default(now()),
        actor_user_id: t.text(),
        action: t.text().required(),
        resource_type: t.text().required(),
        resource_id: t.text(),
        decision: t.text().required(),
        matched_policies: t.array(t.text(), { storage: "native" }).required().default([]),
        request_ip: t.inet(),
        request_id: t.text(),
      },
      primaryKey: ["id"],
    });
    table("authz_decisions", { schema: "zeroship" }).check("authz_decisions_decision_check").add({ expr: (col) => col("decision").in(["allow", "deny"]) });
    table("cron_state", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        key: t.text().required(),
        last_rotated_at: t.timestamp().required().default(now()),
        notes: t.text(),
      },
      primaryKey: ["id"],
    });
    table("cron_state", { schema: "zeroship" }).unique("cron_state_natural_key").add({ columns: ["key"] });
    table("device_grants", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        device_code_hash: t.text().required(),
        user_code: t.text().required(),
        status: t.text().required().default("pending"),
        principal_id: t.text(),
        platform_access_token_enc: t.bytes(),
        provider: t.text().required(),
        scope: t.text(),
        created_at: t.timestamp().required().default(now()),
        expires_at: t.timestamp().required(),
        last_polled_at: t.timestamp(),
        auth_credential_version: t.bigInt().required().default(0),
        client_id: t.text(),
        sid: t.text(),
        poll_interval_secs: t.int().required().default(5),
      },
      primaryKey: ["id"],
    });
    table("device_grants", { schema: "zeroship" }).unique("device_grants_natural_key").add({ columns: ["device_code_hash"] });
    table("device_grants", { schema: "zeroship" }).check("device_grants_poll_interval_secs_check").add({ expr: (col) => col("poll_interval_secs").gt(0) });
    table("device_grants", { schema: "zeroship" }).check("device_grants_status_check").add({ expr: (col) => col("status").in(["pending", "approved", "denied"]) });
    table("dpop_jti", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        jti: t.text().required(),
        inserted_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("dpop_jti", { schema: "zeroship" }).unique("dpop_jti_natural_key").add({ columns: ["jti"] });
    table("email_suppressions", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        email: t.text({ caseSensitive: false }).required(),
        reason: t.text().required(),
        suppressed_at: t.timestamp().required().default(now()),
        provider_msg: t.text(),
      },
      primaryKey: ["id"],
    });
    table("email_suppressions", { schema: "zeroship" }).unique("email_suppressions_natural_key").add({ columns: ["email"] });
    table("email_verifications", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        token_hash: t.bytes().required(),
        user_id: t.text().required(),
        email: t.text({ caseSensitive: false }).required(),
        issued_at: t.timestamp().required().default(now()),
        expires_at: t.timestamp().required(),
        consumed_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("email_verifications", { schema: "zeroship" }).unique("email_verifications_natural_key").add({ columns: ["token_hash"] });
    table("federated_identities", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().required().default(uuidV4()),
        user_id: t.text().required(),
        provider: t.text().required(),
        subject: t.text().required(),
        email_at_link: t.text({ caseSensitive: false }),
        raw_profile: t.json(),
        linked_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("gateway_sessions", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().required().default(uuidV4()),
        user_id: t.text().required(),
        app_id: t.text().required(),
        email: t.text({ caseSensitive: false }),
        name: t.text(),
        avatar_url: t.text(),
        email_verified: t.boolean().required().default(false),
        issued_at: t.timestamp().required().default(now()),
        idle_expires_at: t.timestamp().required(),
        abs_expires_at: t.timestamp().required(),
        revoked_at: t.timestamp(),
        granted_scopes: t.array(t.text(), { storage: "native" }).required().default([]),
        auth_time: t.timestamp(),
        amr: t.array(t.text(), { storage: "native" }).required().default([]),
        sid: t.text(),
      },
      primaryKey: ["id"],
    });
    table("identity_links", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        principal_id: t.text().required(),
        provider: t.text().required(),
        provider_subject: t.text().required(),
        email: t.text(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("identity_links", { schema: "zeroship" }).unique("identity_links_natural_key").add({ columns: ["provider", "provider_subject"] });
    table("idp_sessions", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().required().default(uuidV4()),
        user_id: t.text().required(),
        auth_method: t.text().required(),
        amr: t.array(t.text(), { storage: "native" }).required(),
        acr: t.text(),
        auth_time: t.timestamp().required().default(now()),
        credential_version: t.bigInt().required().default(0),
        idle_expires_at: t.timestamp().required(),
        abs_expires_at: t.timestamp().required(),
        revoked_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("magic_completions", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        csrf_nonce: t.text().required(),
        code: t.text().required(),
        email: t.text({ caseSensitive: false }).required(),
        login_challenge: t.text().required(),
        attempts: t.int().required().default(0),
        expires_at: t.timestamp().required(),
        consumed_pending_at: t.timestamp(),
        consumed_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("magic_completions", { schema: "zeroship" }).unique("magic_completions_natural_key").add({ columns: ["csrf_nonce"] });
    table("magic_links", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        token_hash: t.bytes().required(),
        email: t.text({ caseSensitive: false }).required(),
        csrf_nonce: t.text().required(),
        purpose: t.text().required(),
        request_ip: t.inet(),
        request_ua: t.text(),
        issued_at: t.timestamp().required().default(now()),
        expires_at: t.timestamp().required(),
        consumed_pending_at: t.timestamp(),
        consumed_at: t.timestamp(),
        user_id: t.text(),
      },
      primaryKey: ["id"],
    });
    table("magic_links", { schema: "zeroship" }).unique("magic_links_natural_key").add({ columns: ["token_hash"] });
    table("oauth_authorization_codes", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        code_hash: t.bytes().required(),
        client_id: t.text().required(),
        redirect_uri: t.text().required(),
        pkce_challenge: t.text().required(),
        pkce_method: t.text().required(),
        requested_scopes: t.array(t.text(), { storage: "native" }).required(),
        granted_scopes: t.array(t.text(), { storage: "native" }).required(),
        nonce: t.text(),
        user_id: t.text().required(),
        created_at: t.timestamp().required().default(now()),
        expires_at: t.timestamp().required(),
        consumed_at: t.timestamp(),
        auth_credential_version: t.bigInt().required().default(0),
        sid: t.text().required(),
      },
      primaryKey: ["id"],
    });
    table("oauth_authorization_codes", { schema: "zeroship" }).unique("oauth_authorization_codes_natural_key").add({ columns: ["code_hash"] });
    table("oauth_authorization_codes", { schema: "zeroship" }).check("oauth_authorization_codes_max_ttl").add({ expr: (col) => col("expires_at").le(col("created_at").add(interval({ minutes: 1 }))) });
    table("oauth_authorization_codes", { schema: "zeroship" }).check("oauth_authorization_codes_pkce_method_check").add({ expr: (col) => col("pkce_method").eq("S256") });
    table("oauth_clients", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        client_id: t.text().required(),
        client_name: t.text().required(),
        client_uri: t.text(),
        logo_uri: t.text(),
        redirect_uris: t.array(t.text(), { storage: "native" }).required(),
        scopes: t.array(t.text(), { storage: "native" }).required(),
        skip_consent: t.boolean().required().default(false),
        created_at: t.timestamp().required().default(now()),
        created_by: t.text(),
        client_secret_hash: t.text(),
        refresh_allowed: t.boolean().required().default(false),
        token_endpoint_auth_method: t.text().required().default("none"),
        brokered: t.boolean().required().default(false),
        backchannel_logout_uri: t.text(),
      },
      primaryKey: ["id"],
    });
    table("oauth_clients", { schema: "zeroship" }).unique("oauth_clients_natural_key").add({ columns: ["client_id"] });
    table("oauth_clients", { schema: "zeroship" }).check("oauth_clients_brokered_requires_secret_basic").add({ expr: (col) => col("brokered").eq(false).or(col("token_endpoint_auth_method").eq("client_secret_basic")) });
    table("oauth_clients", { schema: "zeroship" }).check("oauth_clients_token_endpoint_auth_method_check").add({ expr: (col) => col("token_endpoint_auth_method").in(["none", "client_secret_basic", "client_secret_post"]) });
    table("oauth_grants", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        user_id: t.text().required(),
        client_id: t.text().required(),
        granted_scopes: t.array(t.text(), { storage: "native" }).required(),
        granted_at: t.timestamp().required().default(now()),
        updated_at: t.timestamp().required().default(now()),
        last_used_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("oauth_grants", { schema: "zeroship" }).unique("oauth_grants_natural_key").add({ columns: ["user_id", "client_id"] });
    table("oidc_session_clients", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        idp_session_id: t.uuid().required(),
        user_id: t.text().required(),
        client_id: t.text().required(),
        sid: t.text().required(),
        sub: t.text().required(),
        first_seen_at: t.timestamp().required().default(now()),
        last_seen_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("oidc_session_clients", { schema: "zeroship" }).unique("oidc_session_clients_natural_key").add({ columns: ["idp_session_id", "client_id"] });
    table("principal_grants", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        principal_id: t.text().required(),
        grant_name: t.text().required(),
      },
      primaryKey: ["id"],
    });
    table("principal_grants", { schema: "zeroship" }).unique("principal_grants_natural_key").add({ columns: ["principal_id", "grant_name"] });
    table("rate_limits", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        bucket_key: t.text().required(),
        tokens: t.double().required(),
        updated_at: t.timestamp().required(),
      },
      primaryKey: ["id"],
    });
    table("rate_limits", { schema: "zeroship" }).unique("rate_limits_natural_key").add({ columns: ["bucket_key"] });
    table("signing_keys", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        kid: t.text().required(),
        alg: t.text().required(),
        public_jwk: t.json().required(),
        status: t.text().required(),
        created_at: t.timestamp().required().default(now()),
        activated_at: t.timestamp(),
        retiring_at: t.timestamp(),
        retired_at: t.timestamp(),
        max_issued_expires_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("signing_keys", { schema: "zeroship" }).unique("signing_keys_natural_key").add({ columns: ["kid"] });
    table("signing_keys", { schema: "zeroship" }).check("signing_keys_alg_check").add({ expr: (col) => col("alg").eq("EdDSA") });
    table("signing_keys", { schema: "zeroship" }).check("signing_keys_status_check").add({ expr: (col) => col("status").in(["active", "next", "retiring", "retired"]) });
    table("token_revocations", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        client_id: t.text().required(),
        sub: t.text().required(),
        revoked_after: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("token_revocations", { schema: "zeroship" }).unique("token_revocations_natural_key").add({ columns: ["client_id", "sub"] });
    table("totp_backup_codes", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity({ always: true }),
        user_id: t.text().required(),
        code_hash: t.text().required(),
        used_at: t.timestamp(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("totp_credentials", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().required().identity(),
        user_id: t.text().required(),
        encrypted_secret: t.bytes().required(),
        confirmed_at: t.timestamp(),
        created_at: t.timestamp().required().default(now()),
      },
      primaryKey: ["id"],
    });
    table("totp_credentials", { schema: "zeroship" }).unique("totp_credentials_natural_key").add({ columns: ["user_id"] });
    table("users", { schema: "zeroship" }).create({
      columns: {
        id: t.text().required(),
        email: t.text({ caseSensitive: false }).required(),
        email_verified_at: t.timestamp(),
        name: t.text().required(),
        avatar_url: t.text(),
        password_hash: t.text(),
        credential_version: t.bigInt().required().default(0),
        locked_until: t.timestamp(),
        disabled_at: t.timestamp(),
        created_at: t.timestamp().required().default(now()),
        updated_at: t.timestamp().required().default(now()),
        last_login_at: t.timestamp(),
        failed_login_count: t.int().required().default(0),
        deletion_requested_at: t.timestamp(),
        deletion_scheduled_for: t.timestamp(),
        anonymized_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    for (const [tableName, columns] of Object.entries(userIdColumnsByTable)) {
      for (const column of columns) {
        table(tableName, { schema: "zeroship" })
          .check(`${tableName}_${column}_usr_shape`)
          .add({ expr: (col) => col(column).regex("^usr_[0-9a-z]{25}$") });
      }
    }
  },
};
