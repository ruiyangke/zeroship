import {
  nextval,
  table,
  t,
  now,
  uuidV4,
  interval,
  sequence,
} from "@zeroship/migrate";

export default {
  name: "auth_oauth_tables",
  schema() {
    table("app_session_anchors", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        app_id: t.text().notNull(),
        client_id: t.text().notNull(),
        global_user_id: t.uuid().notNull(),
        refresh_token_enc: t.bytes().notNull(),
        refresh_family_id: t.text().notNull(),
        granted_scopes: t.textArray().notNull().default([]),
        created_at: t.timestamp().notNull().default(now()),
        abs_expires_at: t.timestamp().notNull(),
        revoked_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("app_user_identities", { schema: "zeroship" }).create({
      columns: {
        app_client_id: t.text().notNull(),
        global_user_id: t.uuid().notNull(),
        pairwise_sub: t.text().notNull(),
        relay_email: t.text(),
        created_at: t.timestamp().notNull().default(now()),
        revoked_at: t.timestamp(),
      },
      primaryKey: ["app_client_id", "global_user_id"],
    });
    table("audit_events", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().default(nextval("audit_events_id_seq", { schema: "zeroship" })),
        occurred_at: t.timestamp().notNull().default(now()),
        event_type: t.text().notNull(),
        outcome: t.text().notNull(),
        actor_user_id: t.uuid(),
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
        id: t.uuid().notNull().default(uuidV4()),
        occurred_at: t.timestamp().notNull().default(now()),
        actor_user_id: t.uuid(),
        token_id: t.uuid(),
        action: t.text().notNull(),
        resource_type: t.text().notNull(),
        resource_id: t.text(),
        decision: t.text().notNull(),
        matched_policies: t.textArray().notNull().default([]),
        request_ip: t.inet(),
        request_id: t.text(),
      },
      primaryKey: ["id"],
    });
    table("authz_decisions", { schema: "zeroship" }).check("authz_decisions_decision_check").add({ expr: (col) => col("decision").in(["allow", "deny"]) });
    table("cron_state", { schema: "zeroship" }).create({
      columns: {
        key: t.text().notNull(),
        last_rotated_at: t.timestamp().notNull().default(now()),
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
        created_at: t.timestamp().notNull().default(now()),
        expires_at: t.timestamp().notNull(),
        last_polled_at: t.timestamp(),
        auth_credential_version: t.bigInt().notNull().default(0),
        client_id: t.text(),
        sid: t.text(),
        poll_interval_secs: t.int().notNull().default(5),
      },
      primaryKey: ["device_code_hash"],
    });
    table("device_grants", { schema: "zeroship" }).check("device_grants_poll_interval_secs_check").add({ expr: (col) => col("poll_interval_secs").gt(0) });
    table("device_grants", { schema: "zeroship" }).check("device_grants_status_check").add({ expr: (col) => col("status").in(["pending", "approved", "denied"]) });
    table("dpop_jti", { schema: "zeroship" }).create({
      columns: {
        jti: t.text().notNull(),
        inserted_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["jti"],
    });
    table("email_suppressions", { schema: "zeroship" }).create({
      columns: {
        email: t.text({ caseSensitive: false }).notNull(),
        reason: t.text().notNull(),
        suppressed_at: t.timestamp().notNull().default(now()),
        provider_msg: t.text(),
      },
      primaryKey: ["email"],
    });
    table("email_verifications", { schema: "zeroship" }).create({
      columns: {
        token_hash: t.bytes().notNull(),
        user_id: t.uuid().notNull(),
        email: t.text({ caseSensitive: false }).notNull(),
        issued_at: t.timestamp().notNull().default(now()),
        expires_at: t.timestamp().notNull(),
        consumed_at: t.timestamp(),
      },
      primaryKey: ["token_hash"],
    });
    table("federated_identities", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        user_id: t.uuid().notNull(),
        provider: t.text().notNull(),
        subject: t.text().notNull(),
        email_at_link: t.text({ caseSensitive: false }),
        raw_profile: t.json(),
        linked_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("gateway_sessions", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        user_id: t.uuid().notNull(),
        app_id: t.text().notNull(),
        email: t.text({ caseSensitive: false }),
        name: t.text(),
        avatar_url: t.text(),
        email_verified: t.boolean().notNull().default(false),
        issued_at: t.timestamp().notNull().default(now()),
        idle_expires_at: t.timestamp().notNull(),
        abs_expires_at: t.timestamp().notNull(),
        revoked_at: t.timestamp(),
        granted_scopes: t.textArray().notNull().default([]),
        auth_time: t.timestamp(),
        amr: t.textArray().notNull().default([]),
        sid: t.text(),
      },
      primaryKey: ["id"],
    });
    table("identity_links", { schema: "zeroship" }).create({
      columns: {
        principal_id: t.uuid().notNull(),
        provider: t.text().notNull(),
        provider_subject: t.text().notNull(),
        email: t.text(),
        created_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["provider", "provider_subject"],
    });
    table("idp_sessions", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        user_id: t.uuid().notNull(),
        auth_method: t.text().notNull(),
        amr: t.textArray().notNull(),
        acr: t.text(),
        auth_time: t.timestamp().notNull().default(now()),
        credential_version: t.bigInt().notNull().default(0),
        idle_expires_at: t.timestamp().notNull(),
        abs_expires_at: t.timestamp().notNull(),
        revoked_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
    table("jwk_key_state", { schema: "zeroship" }).create({
      columns: {
        set_name: t.text().notNull(),
        kid: t.text().notNull(),
        created_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["set_name", "kid"],
    });
    table("magic_completions", { schema: "zeroship" }).create({
      columns: {
        csrf_nonce: t.text().notNull(),
        code: t.text().notNull(),
        email: t.text({ caseSensitive: false }).notNull(),
        login_challenge: t.text().notNull(),
        attempts: t.smallInt().notNull().default(0),
        expires_at: t.timestamp().notNull(),
        consumed_pending_at: t.timestamp(),
        consumed_at: t.timestamp(),
      },
      primaryKey: ["csrf_nonce"],
    });
    table("magic_links", { schema: "zeroship" }).create({
      columns: {
        token_hash: t.bytes().notNull(),
        email: t.text({ caseSensitive: false }).notNull(),
        csrf_nonce: t.text().notNull(),
        purpose: t.text().notNull(),
        request_ip: t.inet(),
        request_ua: t.text(),
        issued_at: t.timestamp().notNull().default(now()),
        expires_at: t.timestamp().notNull(),
        consumed_pending_at: t.timestamp(),
        consumed_at: t.timestamp(),
        user_id: t.uuid(),
      },
      primaryKey: ["token_hash"],
    });
    table("oauth_authorization_codes", { schema: "zeroship" }).create({
      columns: {
        code_hash: t.bytes().notNull(),
        client_id: t.text().notNull(),
        redirect_uri: t.text().notNull(),
        pkce_challenge: t.text().notNull(),
        pkce_method: t.text().notNull(),
        requested_scopes: t.textArray().notNull(),
        granted_scopes: t.textArray().notNull(),
        nonce: t.text(),
        user_id: t.uuid().notNull(),
        created_at: t.timestamp().notNull().default(now()),
        expires_at: t.timestamp().notNull(),
        consumed_at: t.timestamp(),
        auth_credential_version: t.bigInt().notNull().default(0),
        sid: t.text().notNull(),
      },
      primaryKey: ["code_hash"],
    });
    table("oauth_authorization_codes", { schema: "zeroship" }).check("oauth_authorization_codes_max_ttl").add({ expr: (col) => col("expires_at").le(col("created_at").add(interval({ minutes: 1 }))) });
    table("oauth_authorization_codes", { schema: "zeroship" }).check("oauth_authorization_codes_pkce_method_check").add({ expr: (col) => col("pkce_method").eq("S256") });
    table("oauth_clients", { schema: "zeroship" }).create({
      columns: {
        client_id: t.text().notNull(),
        client_name: t.text().notNull(),
        client_uri: t.text(),
        logo_uri: t.text(),
        redirect_uris: t.textArray().notNull(),
        scopes: t.textArray().notNull(),
        skip_consent: t.boolean().notNull().default(false),
        created_at: t.timestamp().notNull().default(now()),
        created_by: t.uuid(),
        client_secret_hash: t.text(),
        refresh_allowed: t.boolean().notNull().default(false),
        token_endpoint_auth_method: t.text().notNull().default("none"),
        brokered: t.boolean().notNull().default(false),
        backchannel_logout_uri: t.text(),
      },
      primaryKey: ["client_id"],
    });
    table("oauth_clients", { schema: "zeroship" }).check("oauth_clients_brokered_requires_secret_basic").add({ expr: (col) => col("brokered").eq(false).or(col("token_endpoint_auth_method").eq("client_secret_basic")) });
    table("oauth_clients", { schema: "zeroship" }).check("oauth_clients_token_endpoint_auth_method_check").add({ expr: (col) => col("token_endpoint_auth_method").in(["none", "client_secret_basic", "client_secret_post"]) });
    table("oauth_grants", { schema: "zeroship" }).create({
      columns: {
        user_id: t.uuid().notNull(),
        client_id: t.text().notNull(),
        granted_scopes: t.textArray().notNull(),
        granted_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
        last_used_at: t.timestamp(),
      },
      primaryKey: ["user_id", "client_id"],
    });
    table("oauth_refresh_tokens", { schema: "zeroship" }).create({
      columns: {
        token_hash: t.bytes().notNull(),
        hash_key_version: t.smallInt().notNull(),
        refresh_family_id: t.text().notNull(),
        replaced_by_token_hash: t.bytes(),
        client_id: t.text().notNull(),
        user_id: t.uuid().notNull(),
        sub: t.text().notNull(),
        granted_scopes: t.textArray().notNull(),
        family_granted_scopes: t.textArray().notNull(),
        issued_at: t.timestamp().notNull().default(now()),
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
    table("oauth_refresh_tokens", { schema: "zeroship" }).check("oauth_refresh_tokens_idle_le_ceiling").add({ expr: (col) => col("expires_at").le(col("family_absolute_expires_at")) });
    table("oidc_session_clients", { schema: "zeroship" }).create({
      columns: {
        idp_session_id: t.uuid().notNull(),
        user_id: t.uuid().notNull(),
        client_id: t.text().notNull(),
        sid: t.text().notNull(),
        sub: t.text().notNull(),
        first_seen_at: t.timestamp().notNull().default(now()),
        last_seen_at: t.timestamp().notNull().default(now()),
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
        tokens: t.real().notNull(),
        updated_at: t.timestamp().notNull(),
      },
      primaryKey: ["bucket_key"],
    });
    table("signing_keys", { schema: "zeroship" }).create({
      columns: {
        kid: t.text().notNull(),
        alg: t.text().notNull(),
        public_jwk: t.json().notNull(),
        status: t.text().notNull(),
        created_at: t.timestamp().notNull().default(now()),
        activated_at: t.timestamp(),
        retiring_at: t.timestamp(),
        retired_at: t.timestamp(),
      },
      primaryKey: ["kid"],
    });
    table("signing_keys", { schema: "zeroship" }).check("signing_keys_alg_check").add({ expr: (col) => col("alg").eq("EdDSA") });
    table("signing_keys", { schema: "zeroship" }).check("signing_keys_status_check").add({ expr: (col) => col("status").in(["active", "next", "retiring"]) });
    table("token_revocations", { schema: "zeroship" }).create({
      columns: {
        client_id: t.text().notNull(),
        sub: t.text().notNull(),
        revoked_after: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["client_id", "sub"],
    });
    table("totp_backup_codes", { schema: "zeroship" }).create({
      columns: {
        id: t.bigInt().notNull().identity({ always: true }),
        user_id: t.uuid().notNull(),
        code_hash: t.text().notNull(),
        used_at: t.timestamp(),
        created_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["id"],
    });
    table("totp_credentials", { schema: "zeroship" }).create({
      columns: {
        user_id: t.uuid().notNull(),
        encrypted_secret: t.bytes().notNull(),
        confirmed_at: t.timestamp(),
        created_at: t.timestamp().notNull().default(now()),
      },
      primaryKey: ["user_id"],
    });
    table("users", { schema: "zeroship" }).create({
      columns: {
        id: t.uuid().notNull().default(uuidV4()),
        email: t.text({ caseSensitive: false }).notNull(),
        email_verified_at: t.timestamp(),
        name: t.text().notNull(),
        avatar_url: t.text(),
        password_hash: t.text(),
        credential_version: t.bigInt().notNull().default(0),
        locked_until: t.timestamp(),
        disabled_at: t.timestamp(),
        created_at: t.timestamp().notNull().default(now()),
        updated_at: t.timestamp().notNull().default(now()),
        last_login_at: t.timestamp(),
        failed_login_count: t.int().notNull().default(0),
        deletion_requested_at: t.timestamp(),
        deletion_scheduled_for: t.timestamp(),
        anonymized_at: t.timestamp(),
      },
      primaryKey: ["id"],
    });
  },
};
