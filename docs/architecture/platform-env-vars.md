# Platform environment variables

The zeroship services' own configuration: the per-binary settings the API,
gateway, worker, auth and migration services parse, and the operator's
deployment surface. This is not a creator contract — app code does not set or
read these names. The creator-facing read model is
[environment variables](../reference/env-vars.md).

---

## Read the naming rule first

Everything from here to
[Gates that are NOT armed, and why](#gates-that-are-not-armed-and-why) is the
platform's own configuration. It is not a creator surface: app code does not set
these names and does not read them.

Every platform setting has one canonical identity, and its environment name is
always `ZEROSHIP_<CANONICAL>`; the overlay member is the canonical name itself.
`control.port`, for example, is the environment name `ZEROSHIP_CONTROL_PORT` and
the overlay member `[control] port`. There is no alias for an older spelling.

A secret has exactly one tier, a `--<name>-file PATH` flag, and never a value
flag, so secret material never reaches a process argument list. An identity with
no scope prefix is platform-global; a domain-scoped name belongs to that domain
even when more than one service reads it.

An overlay supplies either the value directly or a reference to a file that
holds it. A literal secret may appear in an overlay that is itself a mounted
secret, never in a tracked file. Every secret file must be owner-only, and one
readable through any group or other permission is refused at boot. The overlay
is auto-discovered or named by `ZEROSHIP_CONFIG`.

---

## The placeholder a service refuses to start on

A platform credential whose value is the placeholder below is treated exactly
like an empty one: both are refused, by the same branch and with the same text.

```
CHANGE_ME_ZEROSHIP_SERVICE_KEY
```

A release build exits on either, both at boot and under a configuration check. A
debug build prints the banner and continues, but its health check then answers
not-ready for the life of the process. No flag and no environment variable
reaches that last behaviour; it is decided by the build profile alone.

---

## A name is not an address

Two platform settings, `auth.platform_issuer` and `auth.platform_jwks_url`,
look adjacent in the table and are not interchangeable.

The first is a NAME: the exact string a platform token's issuer claim must
equal. It is the public issuer, always, and it is never dialled. The second is a
ROUTE: the address the reading service opens to fetch signing keys, fixed by
what that service can reach.

When the public name is served only to outside traffic, the two are necessarily
different values, and pointing the route at an internal address changes nothing
about what is trusted. The route defaults to the issuer's well-known keys path,
which is correct only when the public name is reachable from inside.

---

## The zeroship-owned contract

The table below is the platform's own settings, not a creator surface. Each row
is one canonical identity: supply it by the environment name, by the flag the
owning service spells, or at the overlay path, whichever the class allows. A `-`
means that class has no tier at all, which is a guarantee rather than an
omission.

The classes are:

- **operational** - an ordinary setting.
- **secret** - supplied by the `--<name>-file` flag or the overlay, never as a
  command-line value.
- **bootstrap control** - read while the process is starting, either before the
  overlay can be loaded or as a safety control that must not be persistable.
- **command control** - a mode of a command. An action, not a setting, so only
  the flag reaches it.

<!-- BEGIN GENERATED CONFIGURATION CONTRACT -->
<!--
DO NOT EDIT THIS REGION BY HAND. It is rendered from the COMPILED
ConfigSpec registries of the declaring binaries by
`cargo run -p zeroship-config-contract -- env-vars-doc`. Everything
OUTSIDE these two markers is hand-maintained and is never rewritten
by the generator.
-->

Every environment name below is `ZEROSHIP_<CANONICAL>` and every overlay path is the canonical name itself, because both are computed from the one declaration rather than spelled twice.

### shared (no scope prefix: read by more than one binary)

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `blob_store` | operational | `ZEROSHIP_BLOB_STORE` | `blob_store` | zeroship-control `--blob-store`<br>zeroship-gate `--blob-store`<br>zeroship-worker `--blob-store` | `./bundles` |
| `check_config` | command control | - | - | zeroship-auth `--check-config`<br>zeroship-control `--check-config`<br>zeroship-data-cdc-server `--check-config`<br>zeroship-gate `--check-config`<br>zeroship-migrate-server `--check-config`<br>zeroship-worker `--check-config`<br>zeroship-workflow-server `--check-config` | - |
| `check_config_format` | command control | - | - | zeroship-auth `--check-config-format`<br>zeroship-control `--check-config-format`<br>zeroship-data-cdc-server `--check-config-format`<br>zeroship-gate `--check-config-format`<br>zeroship-migrate-server `--check-config-format`<br>zeroship-worker `--check-config-format`<br>zeroship-workflow-server `--check-config-format` | `CheckFormat::Text` |
| `config` | bootstrap control | `ZEROSHIP_CONFIG` | - | zeroship-auth `--config`<br>zeroship-control `--config`<br>zeroship-data-cdc-server `--config`<br>zeroship-gate `--config`<br>zeroship-migrate-server `--config`<br>zeroship-workflow-server `--config` | - |
| `control_key` | secret | `ZEROSHIP_CONTROL_KEY` | `control_key` | zeroship-control `--control-key-file`<br>zeroship-gate `--control-key-file`<br>zeroship-migrate-server `--control-key-file`<br>zeroship-worker `--control-key-file` | - |
| `control_url` | operational | `ZEROSHIP_CONTROL_URL` | `control_url` | zeroship-auth `--control-url`<br>zeroship-gate `--control-url`<br>zeroship-worker `--control-url` | `http://localhost:9090` |
| `no_config` | bootstrap control | `ZEROSHIP_NO_CONFIG` | - | zeroship-auth `--no-config`<br>zeroship-control `--no-config`<br>zeroship-data-cdc-server `--no-config`<br>zeroship-gate `--no-config`<br>zeroship-migrate-server `--no-config`<br>zeroship-workflow-server `--no-config` | - |
| `oauth_audience` | operational | `ZEROSHIP_OAUTH_AUDIENCE` | `oauth_audience` | zeroship-auth `--oauth-audience`<br>zeroship-control `--oauth-audience`<br>zeroship-migrate-server `--oauth-audience` | `control.zeroship.ai` |
| `origin_scheme` | operational | `ZEROSHIP_ORIGIN_SCHEME` | `origin_scheme` | zeroship-control `--origin-scheme`<br>zeroship-gate `--origin-scheme` | `OriginScheme::Https` |
| `pairwise_salt` | secret | `ZEROSHIP_PAIRWISE_SALT` | `pairwise_salt` | zeroship-control `--pairwise-salt-file`<br>zeroship-gate `--pairwise-salt-file` | - |
| `poll_interval` | operational | `ZEROSHIP_POLL_INTERVAL` | `poll_interval` | zeroship-gate `--poll-interval`<br>zeroship-worker `--poll-interval` | `5` |
| `trust_proxy` | operational | `ZEROSHIP_TRUST_PROXY` | `trust_proxy` | zeroship-control `--trust-proxy`<br>zeroship-gate `--trust-proxy`<br>zeroship-migrate-server `--trust-proxy` | `false` |
| `trusted_origins` | operational | `ZEROSHIP_TRUSTED_ORIGINS` | `trusted_origins` | zeroship-gate `--trusted-origins` | empty |
| `worker_urls` | operational | `ZEROSHIP_WORKER_URLS` | `worker_urls` | zeroship-control `--worker-urls`<br>zeroship-gate `--worker-urls` | `http://localhost:8080` |

### auth

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `auth.addr` | operational | `ZEROSHIP_AUTH_ADDR` | `auth.addr` | zeroship-auth `--addr` | `127.0.0.1:9092` |
| `auth.audit_retention_check_secs` | operational | `ZEROSHIP_AUTH_AUDIT_RETENTION_CHECK_SECS` | `auth.audit_retention_check_secs` | zeroship-auth `--audit-retention-check-secs` | `3_600` |
| `auth.broker_secret_file` | operational | `ZEROSHIP_AUTH_BROKER_SECRET_FILE` | `auth.broker_secret_file` | zeroship-auth `--broker-secret-file` | empty |
| `auth.broker_secret_previous_file` | operational | `ZEROSHIP_AUTH_BROKER_SECRET_PREVIOUS_FILE` | `auth.broker_secret_previous_file` | zeroship-auth `--broker-secret-previous-file` | empty |
| `auth.cron_tick_secs` | operational | `ZEROSHIP_AUTH_CRON_TICK_SECS` | `auth.cron_tick_secs` | zeroship-auth `--cron-tick-secs` | `86_400` |
| `auth.database_url` | secret | `ZEROSHIP_AUTH_DATABASE_URL` | `auth.database_url` | zeroship-auth `--database-url-file` | - |
| `auth.frame_ancestor_origins` | operational | `ZEROSHIP_AUTH_FRAME_ANCESTOR_ORIGINS` | `auth.frame_ancestor_origins` | zeroship-auth `--frame-ancestor-origins` | empty |
| `auth.github_authorize_url` | operational | `ZEROSHIP_AUTH_GITHUB_AUTHORIZE_URL` | `auth.github_authorize_url` | zeroship-auth `--github-authorize-url` | `https://github.com/login/oauth/authorize` |
| `auth.github_client_id` | operational | `ZEROSHIP_AUTH_GITHUB_CLIENT_ID` | `auth.github_client_id` | zeroship-auth `--github-client-id` | empty |
| `auth.github_client_secret` | secret | `ZEROSHIP_AUTH_GITHUB_CLIENT_SECRET` | `auth.github_client_secret` | zeroship-auth `--github-client-secret-file` | - |
| `auth.github_emails_url` | operational | `ZEROSHIP_AUTH_GITHUB_EMAILS_URL` | `auth.github_emails_url` | zeroship-auth `--github-emails-url` | `https://api.github.com/user/emails` |
| `auth.github_redirect_uri` | operational | `ZEROSHIP_AUTH_GITHUB_REDIRECT_URI` | `auth.github_redirect_uri` | zeroship-auth `--github-redirect-uri` | `https://auth.zeroship.ai/oauth/github/callback` |
| `auth.github_token_url` | operational | `ZEROSHIP_AUTH_GITHUB_TOKEN_URL` | `auth.github_token_url` | zeroship-auth `--github-token-url` | `https://github.com/login/oauth/access_token` |
| `auth.github_user_url` | operational | `ZEROSHIP_AUTH_GITHUB_USER_URL` | `auth.github_user_url` | zeroship-auth `--github-user-url` | `https://api.github.com/user` |
| `auth.google_auth_url` | operational | `ZEROSHIP_AUTH_GOOGLE_AUTH_URL` | `auth.google_auth_url` | zeroship-auth `--google-auth-url` | `https://accounts.google.com/o/oauth2/v2/auth` |
| `auth.google_client_id` | operational | `ZEROSHIP_AUTH_GOOGLE_CLIENT_ID` | `auth.google_client_id` | zeroship-auth `--google-client-id` | empty |
| `auth.google_client_secret` | secret | `ZEROSHIP_AUTH_GOOGLE_CLIENT_SECRET` | `auth.google_client_secret` | zeroship-auth `--google-client-secret-file` | - |
| `auth.google_issuer` | operational | `ZEROSHIP_AUTH_GOOGLE_ISSUER` | `auth.google_issuer` | zeroship-auth `--google-issuer` | `https://accounts.google.com` |
| `auth.google_jwks_url` | operational | `ZEROSHIP_AUTH_GOOGLE_JWKS_URL` | `auth.google_jwks_url` | zeroship-auth `--google-jwks-url` | `https://www.googleapis.com/oauth2/v3/certs` |
| `auth.google_redirect_uri` | operational | `ZEROSHIP_AUTH_GOOGLE_REDIRECT_URI` | `auth.google_redirect_uri` | zeroship-auth `--google-redirect-uri` | `https://auth.zeroship.ai/oauth/google/callback` |
| `auth.google_token_url` | operational | `ZEROSHIP_AUTH_GOOGLE_TOKEN_URL` | `auth.google_token_url` | zeroship-auth `--google-token-url` | `https://oauth2.googleapis.com/token` |
| `auth.gotrue_email_hook_secret` | secret | `ZEROSHIP_AUTH_GOTRUE_EMAIL_HOOK_SECRET` | `auth.gotrue_email_hook_secret` | zeroship-auth `--gotrue-email-hook-secret-file` | - |
| `auth.mail_from_email` | operational | `ZEROSHIP_AUTH_MAIL_FROM_EMAIL` | `auth.mail_from_email` | zeroship-auth `--mail-from-email` | `auth@zeroship.ai` |
| `auth.mail_from_name` | operational | `ZEROSHIP_AUTH_MAIL_FROM_NAME` | `auth.mail_from_name` | zeroship-auth `--mail-from-name` | `zeroship` |
| `auth.mailer` | operational | `ZEROSHIP_AUTH_MAILER` | `auth.mailer` | zeroship-auth `--mailer` | `stdout` |
| `auth.pairwise_salt_file` | operational | `ZEROSHIP_AUTH_PAIRWISE_SALT_FILE` | `auth.pairwise_salt_file` | zeroship-auth `--pairwise-salt-file` | empty |
| `auth.platform_issuer` | operational | `ZEROSHIP_AUTH_PLATFORM_ISSUER` | `auth.platform_issuer` | zeroship-control `--auth-platform-issuer`<br>zeroship-migrate-server `--auth-platform-issuer` | empty |
| `auth.platform_jwks_url` | operational | `ZEROSHIP_AUTH_PLATFORM_JWKS_URL` | `auth.platform_jwks_url` | zeroship-control `--auth-platform-jwks-url`<br>zeroship-migrate-server `--auth-platform-jwks-url` | empty |
| `auth.postmark_webhook_password` | secret | `ZEROSHIP_AUTH_POSTMARK_WEBHOOK_PASSWORD` | `auth.postmark_webhook_password` | zeroship-auth `--postmark-webhook-password-file` | - |
| `auth.postmark_webhook_user` | operational | `ZEROSHIP_AUTH_POSTMARK_WEBHOOK_USER` | `auth.postmark_webhook_user` | zeroship-auth `--postmark-webhook-user` | empty |
| `auth.provider` | operational | `ZEROSHIP_AUTH_PROVIDER` | `auth.provider` | zeroship-auth `--provider`<br>zeroship-control `--auth-provider` | `AuthProviderKind::Native` |
| `auth.public_url` | operational | `ZEROSHIP_AUTH_PUBLIC_URL` | `auth.public_url` | zeroship-auth `--public-url` | `http://localhost:9092` |
| `auth.refresh_hash_key_file` | operational | `ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE` | `auth.refresh_hash_key_file` | zeroship-auth `--refresh-hash-key-file` | empty |
| `auth.refresh_idem_key_file` | operational | `ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE` | `auth.refresh_idem_key_file` | zeroship-auth `--refresh-idem-key-file` | empty |
| `auth.refresh_pool_size` | operational | `ZEROSHIP_AUTH_REFRESH_POOL_SIZE` | `auth.refresh_pool_size` | zeroship-auth `--refresh-pool-size` | `4` |
| `auth.relay_domain` | operational | `ZEROSHIP_AUTH_RELAY_DOMAIN` | `auth.relay_domain` | zeroship-auth `--relay-domain` | `relay.zeroship.localhost` |
| `auth.relay_forward_mailer` | operational | `ZEROSHIP_AUTH_RELAY_FORWARD_MAILER` | `auth.relay_forward_mailer` | zeroship-auth `--relay-forward-mailer` | `smtp` |
| `auth.relay_inbound_password` | secret | `ZEROSHIP_AUTH_RELAY_INBOUND_PASSWORD` | `auth.relay_inbound_password` | zeroship-auth `--relay-inbound-password-file` | - |
| `auth.relay_inbound_user` | operational | `ZEROSHIP_AUTH_RELAY_INBOUND_USER` | `auth.relay_inbound_user` | zeroship-auth `--relay-inbound-user` | empty |
| `auth.relay_smtp_host` | operational | `ZEROSHIP_AUTH_RELAY_SMTP_HOST` | `auth.relay_smtp_host` | zeroship-auth `--relay-smtp-host` | empty |
| `auth.relay_smtp_password` | secret | `ZEROSHIP_AUTH_RELAY_SMTP_PASSWORD` | `auth.relay_smtp_password` | zeroship-auth `--relay-smtp-password-file` | - |
| `auth.relay_smtp_port` | operational | `ZEROSHIP_AUTH_RELAY_SMTP_PORT` | `auth.relay_smtp_port` | zeroship-auth `--relay-smtp-port` | `587` |
| `auth.relay_smtp_tls` | operational | `ZEROSHIP_AUTH_RELAY_SMTP_TLS` | `auth.relay_smtp_tls` | zeroship-auth `--relay-smtp-tls` | `SmtpTls::Starttls` |
| `auth.relay_smtp_username` | operational | `ZEROSHIP_AUTH_RELAY_SMTP_USERNAME` | `auth.relay_smtp_username` | zeroship-auth `--relay-smtp-username` | empty |
| `auth.resend_api_key` | secret | `ZEROSHIP_AUTH_RESEND_API_KEY` | `auth.resend_api_key` | zeroship-auth `--resend-api-key-file` | - |
| `auth.service_key_file` | operational | `ZEROSHIP_AUTH_SERVICE_KEY_FILE` | `auth.service_key_file` | zeroship-auth `--service-key-file` | empty |
| `auth.service_peers_file` | operational | `ZEROSHIP_AUTH_SERVICE_PEERS_FILE` | `auth.service_peers_file` | zeroship-auth `--service-peers-file` | empty |
| `auth.signing_key_file` | operational | `ZEROSHIP_AUTH_SIGNING_KEY_FILE` | `auth.signing_key_file` | zeroship-auth `--signing-key-file` | empty |
| `auth.smtp_host` | operational | `ZEROSHIP_AUTH_SMTP_HOST` | `auth.smtp_host` | zeroship-auth `--smtp-host` | empty |
| `auth.smtp_password` | secret | `ZEROSHIP_AUTH_SMTP_PASSWORD` | `auth.smtp_password` | zeroship-auth `--smtp-password-file` | - |
| `auth.smtp_port` | operational | `ZEROSHIP_AUTH_SMTP_PORT` | `auth.smtp_port` | zeroship-auth `--smtp-port` | `587` |
| `auth.smtp_tls` | operational | `ZEROSHIP_AUTH_SMTP_TLS` | `auth.smtp_tls` | zeroship-auth `--smtp-tls` | `SmtpTls::Starttls` |
| `auth.smtp_username` | operational | `ZEROSHIP_AUTH_SMTP_USERNAME` | `auth.smtp_username` | zeroship-auth `--smtp-username` | empty |
| `auth.stash_signing_key` | secret | `ZEROSHIP_AUTH_STASH_SIGNING_KEY` | `auth.stash_signing_key` | zeroship-auth `--stash-signing-key-file` | - |
| `auth.supabase_anon_key` | operational | `ZEROSHIP_AUTH_SUPABASE_ANON_KEY` | `auth.supabase_anon_key` | zeroship-auth `--supabase-anon-key`<br>zeroship-control `--auth-supabase-anon-key` | empty |
| `auth.supabase_jwt_secret` | secret | `ZEROSHIP_AUTH_SUPABASE_JWT_SECRET` | `auth.supabase_jwt_secret` | zeroship-control `--auth-supabase-jwt-secret-file` | - |
| `auth.supabase_service_role_key` | secret | `ZEROSHIP_AUTH_SUPABASE_SERVICE_ROLE_KEY` | `auth.supabase_service_role_key` | zeroship-control `--auth-supabase-service-role-key-file` | - |
| `auth.supabase_url` | operational | `ZEROSHIP_AUTH_SUPABASE_URL` | `auth.supabase_url` | zeroship-auth `--supabase-url`<br>zeroship-control `--auth-supabase-url` | empty |
| `auth.totp_enc_key` | secret | `ZEROSHIP_AUTH_TOTP_ENC_KEY` | `auth.totp_enc_key` | zeroship-auth `--totp-enc-key-file` | - |

### control

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `control.allow_unsupported_billing` | bootstrap control | `ZEROSHIP_CONTROL_ALLOW_UNSUPPORTED_BILLING` | - | zeroship-control `--allow-unsupported-billing` | - |
| `control.app_base_domain` | operational | `ZEROSHIP_CONTROL_APP_BASE_DOMAIN` | `control.app_base_domain` | zeroship-control `--app-base-domain` | `zeroship.ai` |
| `control.audit_retention_check_secs` | operational | `ZEROSHIP_CONTROL_AUDIT_RETENTION_CHECK_SECS` | `control.audit_retention_check_secs` | zeroship-control `--audit-retention-check-secs` | `crate::cron::audit_retention::DEFAULT_CHECK_SECS` |
| `control.audit_retention_months` | operational | `ZEROSHIP_CONTROL_AUDIT_RETENTION_MONTHS` | `control.audit_retention_months` | zeroship-control `--audit-retention-months` | `crate::cron::audit_retention::DEFAULT_RETENTION_MONTHS` |
| `control.billing_forwarder_group_id` | operational | `ZEROSHIP_CONTROL_BILLING_FORWARDER_GROUP_ID` | `control.billing_forwarder_group_id` | zeroship-control `--billing-forwarder-group-id` | `crate::DEFAULT_BILLING_FORWARDER_GROUP_ID` |
| `control.bind` | operational | `ZEROSHIP_CONTROL_BIND` | `control.bind` | zeroship-control `--bind` | `127.0.0.1` |
| `control.catalog_max_connections` | operational | `ZEROSHIP_CONTROL_CATALOG_MAX_CONNECTIONS` | `control.catalog_max_connections` | zeroship-control `--catalog-max-connections` | `crate::publication::shared::DEFAULT_MAX_CONNECTIONS.get()` |
| `control.database_url` | secret | `ZEROSHIP_CONTROL_DATABASE_URL` | `control.database_url` | zeroship-control `--database-url-file` | - |
| `control.deploy_tmp_dir` | operational | `ZEROSHIP_CONTROL_DEPLOY_TMP_DIR` | `control.deploy_tmp_dir` | zeroship-control `--deploy-tmp-dir` | empty |
| `control.invoicer_provider` | operational | `ZEROSHIP_CONTROL_INVOICER_PROVIDER` | `control.invoicer_provider` | zeroship-control `--invoicer-provider` | `lite` |
| `control.join_signers_file` | operational | `ZEROSHIP_CONTROL_JOIN_SIGNERS_FILE` | `control.join_signers_file` | zeroship-control `--join-signers-file` | empty |
| `control.join_token_file` | operational | `ZEROSHIP_CONTROL_JOIN_TOKEN_FILE` | `control.join_token_file` | zeroship-control `--join-token-file` | empty |
| `control.join_token_signer_file` | operational | `ZEROSHIP_CONTROL_JOIN_TOKEN_SIGNER_FILE` | `control.join_token_signer_file` | zeroship-control `--join-token-signer-file` | empty |
| `control.join_token_zone` | operational | `ZEROSHIP_CONTROL_JOIN_TOKEN_ZONE` | `control.join_token_zone` | zeroship-control `--join-token-zone` | `zeroship_core::worker_join::DEFAULT_EXECUTION_ZONE` |
| `control.legacy_master_keys` | secret | `ZEROSHIP_CONTROL_LEGACY_MASTER_KEYS` | `control.legacy_master_keys` | zeroship-control `--legacy-master-keys-file` | - |
| `control.mailer` | operational | `ZEROSHIP_CONTROL_MAILER` | `control.mailer` | zeroship-control `--mailer` | `stdout` |
| `control.master_key` | secret | `ZEROSHIP_CONTROL_MASTER_KEY` | `control.master_key` | zeroship-control `--master-key-file` | - |
| `control.meter_provider` | operational | `ZEROSHIP_CONTROL_METER_PROVIDER` | `control.meter_provider` | zeroship-control `--meter-provider` | `lite` |
| `control.port` | operational | `ZEROSHIP_CONTROL_PORT` | `control.port` | zeroship-control `--port` | `9090` |
| `control.provider_config` | operational | `ZEROSHIP_CONTROL_PROVIDER_CONFIG` | `control.provider_config` | zeroship-control `--provider-config` | `{}` |
| `control.resend_api_key` | secret | `ZEROSHIP_CONTROL_RESEND_API_KEY` | `control.resend_api_key` | zeroship-control `--resend-api-key-file` | - |
| `control.service_key_file` | operational | `ZEROSHIP_CONTROL_SERVICE_KEY_FILE` | `control.service_key_file` | zeroship-control `--service-key-file` | empty |
| `control.service_peers_file` | operational | `ZEROSHIP_CONTROL_SERVICE_PEERS_FILE` | `control.service_peers_file` | zeroship-control `--service-peers-file` | empty |
| `control.smtp_host` | operational | `ZEROSHIP_CONTROL_SMTP_HOST` | `control.smtp_host` | zeroship-control `--smtp-host` | empty |
| `control.smtp_password` | secret | `ZEROSHIP_CONTROL_SMTP_PASSWORD` | `control.smtp_password` | zeroship-control `--smtp-password-file` | - |
| `control.smtp_port` | operational | `ZEROSHIP_CONTROL_SMTP_PORT` | `control.smtp_port` | zeroship-control `--smtp-port` | `587` |
| `control.smtp_username` | operational | `ZEROSHIP_CONTROL_SMTP_USERNAME` | `control.smtp_username` | zeroship-control `--smtp-username` | empty |
| `control.spend_recompute_group_id` | operational | `ZEROSHIP_CONTROL_SPEND_RECOMPUTE_GROUP_ID` | `control.spend_recompute_group_id` | zeroship-control `--spend-recompute-group-id` | `crate::DEFAULT_SPEND_RECOMPUTE_GROUP_ID` |
| `control.spend_recompute_interval` | operational | `ZEROSHIP_CONTROL_SPEND_RECOMPUTE_INTERVAL` | `control.spend_recompute_interval` | zeroship-control `--spend-recompute-interval` | `crate::cron::spend_recompute::DEFAULT_RECOMPUTE_INTERVAL_SECS` |
| `control.stream_config` | operational | `ZEROSHIP_CONTROL_STREAM_CONFIG` | `control.stream_config` | zeroship-control `--stream-config` | `{}` |
| `control.stream_transport` | operational | `ZEROSHIP_CONTROL_STREAM_TRANSPORT` | `control.stream_transport` | zeroship-control `--stream-transport` | empty |
| `control.stripe_base_url` | operational | `ZEROSHIP_CONTROL_STRIPE_BASE_URL` | `control.stripe_base_url` | zeroship-control `--stripe-base-url` | `https://api.stripe.com` |
| `control.stripe_secret_key` | secret | `ZEROSHIP_CONTROL_STRIPE_SECRET_KEY` | `control.stripe_secret_key` | zeroship-control `--stripe-secret-key-file` | - |
| `control.stripe_webhook_secret` | secret | `ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET` | `control.stripe_webhook_secret` | zeroship-control `--stripe-webhook-secret-file` | - |
| `control.supabase_jwks_url` | operational | `ZEROSHIP_CONTROL_SUPABASE_JWKS_URL` | `control.supabase_jwks_url` | zeroship-control `--supabase-jwks-url` | empty |
| `control.supabase_jwt_issuer` | operational | `ZEROSHIP_CONTROL_SUPABASE_JWT_ISSUER` | `control.supabase_jwt_issuer` | zeroship-control `--supabase-jwt-issuer` | empty |
| `control.tax_provider` | operational | `ZEROSHIP_CONTROL_TAX_PROVIDER` | `control.tax_provider` | zeroship-control `--tax-provider` | `native` |
| `control.worker_enrolment_networks` | operational | `ZEROSHIP_CONTROL_WORKER_ENROLMENT_NETWORKS` | `control.worker_enrolment_networks` | zeroship-control `--worker-enrolment-networks` | empty |
| `control.worker_enrolment_ports` | operational | `ZEROSHIP_CONTROL_WORKER_ENROLMENT_PORTS` | `control.worker_enrolment_ports` | zeroship-control `--worker-enrolment-ports` | empty |
| `control.workflow_coordinator_url` | operational | `ZEROSHIP_CONTROL_WORKFLOW_COORDINATOR_URL` | `control.workflow_coordinator_url` | zeroship-control `--workflow-coordinator-url` | `http://127.0.0.1:9093` |

### data-cdc-server

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `data_cdc_server.clients_per_app` | operational | `ZEROSHIP_DATA_CDC_SERVER_CLIENTS_PER_APP` | `data_cdc_server.clients_per_app` | zeroship-data-cdc-server `--clients-per-app` | `128` |
| `data_cdc_server.database_url` | secret | `ZEROSHIP_DATA_CDC_SERVER_DATABASE_URL` | `data_cdc_server.database_url` | zeroship-data-cdc-server `--database-url-file` | - |
| `data_cdc_server.listen` | operational | `ZEROSHIP_DATA_CDC_SERVER_LISTEN` | `data_cdc_server.listen` | zeroship-data-cdc-server `--listen` | `127.0.0.1:9094` |
| `data_cdc_server.max_apps` | operational | `ZEROSHIP_DATA_CDC_SERVER_MAX_APPS` | `data_cdc_server.max_apps` | zeroship-data-cdc-server `--max-apps` | `64` |
| `data_cdc_server.max_connections` | operational | `ZEROSHIP_DATA_CDC_SERVER_MAX_CONNECTIONS` | `data_cdc_server.max_connections` | zeroship-data-cdc-server `--max-connections` | `1024` |
| `data_cdc_server.max_relations` | operational | `ZEROSHIP_DATA_CDC_SERVER_MAX_RELATIONS` | `data_cdc_server.max_relations` | zeroship-data-cdc-server `--max-relations` | `4096` |
| `data_cdc_server.queue_capacity` | operational | `ZEROSHIP_DATA_CDC_SERVER_QUEUE_CAPACITY` | `data_cdc_server.queue_capacity` | zeroship-data-cdc-server `--queue-capacity` | `128` |
| `data_cdc_server.tls_cert_file` | operational | `ZEROSHIP_DATA_CDC_SERVER_TLS_CERT_FILE` | `data_cdc_server.tls_cert_file` | zeroship-data-cdc-server `--tls-cert-file` | empty |
| `data_cdc_server.tls_key_file` | operational | `ZEROSHIP_DATA_CDC_SERVER_TLS_KEY_FILE` | `data_cdc_server.tls_key_file` | zeroship-data-cdc-server `--tls-key-file` | empty |
| `data_cdc_server.transaction_bytes` | operational | `ZEROSHIP_DATA_CDC_SERVER_TRANSACTION_BYTES` | `data_cdc_server.transaction_bytes` | zeroship-data-cdc-server `--transaction-bytes` | `8 * 1024 * 1024` |
| `data_cdc_server.transaction_changes` | operational | `ZEROSHIP_DATA_CDC_SERVER_TRANSACTION_CHANGES` | `data_cdc_server.transaction_changes` | zeroship-data-cdc-server `--transaction-changes` | `10000` |

### gateway

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `gateway.auth_ui_url` | operational | `ZEROSHIP_GATEWAY_AUTH_UI_URL` | `gateway.auth_ui_url` | zeroship-gate `--auth-ui-url` | `http://auth:9092` |
| `gateway.bind` | operational | `ZEROSHIP_GATEWAY_BIND` | `gateway.bind` | zeroship-gate `--bind` | `127.0.0.1` |
| `gateway.blob_cache_disk_gb` | operational | `ZEROSHIP_GATEWAY_BLOB_CACHE_DISK_GB` | `gateway.blob_cache_disk_gb` | zeroship-gate `--blob-cache-disk-gb` | `20` |
| `gateway.blob_cache_disk_root` | operational | `ZEROSHIP_GATEWAY_BLOB_CACHE_DISK_ROOT` | `gateway.blob_cache_disk_root` | zeroship-gate `--blob-cache-disk-root` | `./blob-cache` |
| `gateway.blob_cache_mem_mb` | operational | `ZEROSHIP_GATEWAY_BLOB_CACHE_MEM_MB` | `gateway.blob_cache_mem_mb` | zeroship-gate `--blob-cache-mem-mb` | `256` |
| `gateway.broker_secret_file` | operational | `ZEROSHIP_GATEWAY_BROKER_SECRET_FILE` | `gateway.broker_secret_file` | zeroship-gate `--broker-secret-file` | empty |
| `gateway.database_url` | secret | `ZEROSHIP_GATEWAY_DATABASE_URL` | `gateway.database_url` | zeroship-gate `--database-url-file` | - |
| `gateway.db_pool_size` | operational | `ZEROSHIP_GATEWAY_DB_POOL_SIZE` | `gateway.db_pool_size` | zeroship-gate `--db-pool-size` | `16` |
| `gateway.port` | operational | `ZEROSHIP_GATEWAY_PORT` | `gateway.port` | zeroship-gate `--port` | `80` |
| `gateway.prev_signing_key_file` | operational | `ZEROSHIP_GATEWAY_PREV_SIGNING_KEY_FILE` | `gateway.prev_signing_key_file` | zeroship-gate `--prev-signing-key-file` | empty |
| `gateway.public_url` | operational | `ZEROSHIP_GATEWAY_PUBLIC_URL` | `gateway.public_url` | zeroship-gate `--public-url` | `https://api.zeroship.ai` |
| `gateway.service_key_file` | operational | `ZEROSHIP_GATEWAY_SERVICE_KEY_FILE` | `gateway.service_key_file` | zeroship-gate `--service-key-file` | empty |
| `gateway.service_peers_file` | operational | `ZEROSHIP_GATEWAY_SERVICE_PEERS_FILE` | `gateway.service_peers_file` | zeroship-gate `--service-peers-file` | empty |
| `gateway.signing_key_file` | operational | `ZEROSHIP_GATEWAY_SIGNING_KEY_FILE` | `gateway.signing_key_file` | zeroship-gate `--signing-key-file` | empty |
| `gateway.stash_signing_key` | secret | `ZEROSHIP_GATEWAY_STASH_SIGNING_KEY` | `gateway.stash_signing_key` | zeroship-gate `--stash-signing-key-file` | - |

### metering

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `metering.brokers` | operational | `ZEROSHIP_METERING_BROKERS` | `metering.brokers` | zeroship-gate `--metering-brokers`<br>zeroship-worker `--metering-brokers` | empty |
| `metering.events_topic` | operational | `ZEROSHIP_METERING_EVENTS_TOPIC` | `metering.events_topic` | zeroship-gate `--metering-events-topic`<br>zeroship-worker `--metering-events-topic` | `zeroship_metering::DEFAULT_USAGE_EVENTS_TOPIC` |
| `metering.outbox_wal_path` | operational | `ZEROSHIP_METERING_OUTBOX_WAL_PATH` | `metering.outbox_wal_path` | zeroship-gate `--metering-outbox-wal-path`<br>zeroship-worker `--metering-outbox-wal-path` | empty |
| `metering.producer_group_id` | operational | `ZEROSHIP_METERING_PRODUCER_GROUP_ID` | `metering.producer_group_id` | zeroship-gate `--metering-producer-group-id`<br>zeroship-worker `--metering-producer-group-id` | empty |

### migrate-server

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `migrate_server.bind` | operational | `ZEROSHIP_MIGRATE_SERVER_BIND` | `migrate_server.bind` | zeroship-migrate-server `--bind` | `127.0.0.1` |
| `migrate_server.database_url` | secret | `ZEROSHIP_MIGRATE_SERVER_DATABASE_URL` | `migrate_server.database_url` | zeroship-migrate-server `--database-url-file` | - |
| `migrate_server.execution_zone` | operational | `ZEROSHIP_MIGRATE_SERVER_EXECUTION_ZONE` | `migrate_server.execution_zone` | zeroship-migrate-server `--execution-zone` | empty |
| `migrate_server.mutation_rate_limit_burst` | operational | `ZEROSHIP_MIGRATE_SERVER_MUTATION_RATE_LIMIT_BURST` | `migrate_server.mutation_rate_limit_burst` | zeroship-migrate-server `--mutation-rate-limit-burst` | `2` |
| `migrate_server.mutation_rate_limit_per_minute` | operational | `ZEROSHIP_MIGRATE_SERVER_MUTATION_RATE_LIMIT_PER_MINUTE` | `migrate_server.mutation_rate_limit_per_minute` | zeroship-migrate-server `--mutation-rate-limit-per-minute` | `3` |
| `migrate_server.policy_ceiling_version` | operational | `ZEROSHIP_MIGRATE_SERVER_POLICY_CEILING_VERSION` | `migrate_server.policy_ceiling_version` | zeroship-migrate-server `--policy-ceiling-version` | `1` |
| `migrate_server.policy_seal_key` | secret | `ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY` | `migrate_server.policy_seal_key` | zeroship-migrate-server `--policy-seal-key-file` | - |
| `migrate_server.port` | operational | `ZEROSHIP_MIGRATE_SERVER_PORT` | `migrate_server.port` | zeroship-migrate-server `--port` | `9091` |
| `migrate_server.provision_database_url` | secret | `ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL` | `migrate_server.provision_database_url` | zeroship-migrate-server `--provision-database-url-file` | - |
| `migrate_server.reconcile_interval_seconds` | operational | `ZEROSHIP_MIGRATE_SERVER_RECONCILE_INTERVAL_SECONDS` | `migrate_server.reconcile_interval_seconds` | zeroship-migrate-server `--reconcile-interval-seconds` | `30` |
| `migrate_server.service_peers_file` | operational | `ZEROSHIP_MIGRATE_SERVER_SERVICE_PEERS_FILE` | `migrate_server.service_peers_file` | zeroship-migrate-server `--service-peers-file` | empty |
| `migrate_server.tmp_dir` | operational | `ZEROSHIP_MIGRATE_SERVER_TMP_DIR` | `migrate_server.tmp_dir` | zeroship-migrate-server `--tmp-dir` | `std::env::temp_dir().join("zeroship-migrate-server")` |

### observability

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `observability.log_filter` | operational | `ZEROSHIP_OBSERVABILITY_LOG_FILTER` | `observability.log_filter` | zeroship-auth `--observability-log-filter`<br>zeroship-control `--observability-log-filter`<br>zeroship-data-cdc-server `--observability-log-filter`<br>zeroship-gate `--observability-log-filter`<br>zeroship-migrate-server `--observability-log-filter`<br>zeroship-worker `--observability-log-filter`<br>zeroship-workflow-server `--observability-log-filter` | `DEFAULT_LOG_FILTER` |
| `observability.log_format` | operational | `ZEROSHIP_OBSERVABILITY_LOG_FORMAT` | `observability.log_format` | zeroship-auth `--observability-log-format`<br>zeroship-control `--observability-log-format`<br>zeroship-data-cdc-server `--observability-log-format`<br>zeroship-gate `--observability-log-format`<br>zeroship-migrate-server `--observability-log-format`<br>zeroship-worker `--observability-log-format`<br>zeroship-workflow-server `--observability-log-format` | `LogFormat::Auto` |

### worker

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `worker.bind` | operational | `ZEROSHIP_WORKER_BIND` | `worker.bind` | zeroship-worker `--bind` | `127.0.0.1` |
| `worker.cdc_relay_ca_file` | operational | `ZEROSHIP_WORKER_CDC_RELAY_CA_FILE` | `worker.cdc_relay_ca_file` | zeroship-worker `--cdc-relay-ca-file` | empty |
| `worker.cdc_relay_url` | operational | `ZEROSHIP_WORKER_CDC_RELAY_URL` | `worker.cdc_relay_url` | zeroship-worker `--cdc-relay-url` | empty |
| `worker.database_url` | secret | `ZEROSHIP_WORKER_DATABASE_URL` | `worker.database_url` | zeroship-worker `--database-url-file` | - |
| `worker.join_token_file` | operational | `ZEROSHIP_WORKER_JOIN_TOKEN_FILE` | `worker.join_token_file` | zeroship-worker `--join-token-file` | empty |
| `worker.kv_config` | secret | `ZEROSHIP_WORKER_KV_CONFIG` | `worker.kv_config` | zeroship-worker `--kv-config-file` | - |
| `worker.max_isolates` | operational | `ZEROSHIP_WORKER_MAX_ISOLATES` | `worker.max_isolates` | zeroship-worker `--max-isolates` | `200` |
| `worker.port` | operational | `ZEROSHIP_WORKER_PORT` | `worker.port` | zeroship-worker `--port` | `8080` |
| `worker.service_peers_file` | operational | `ZEROSHIP_WORKER_SERVICE_PEERS_FILE` | `worker.service_peers_file` | zeroship-worker `--service-peers-file` | empty |
| `worker.shutdown_timeout` | operational | `ZEROSHIP_WORKER_SHUTDOWN_TIMEOUT` | `worker.shutdown_timeout` | zeroship-worker `--shutdown-timeout` | `30` |
| `worker.socket` | operational | `ZEROSHIP_WORKER_SOCKET` | `worker.socket` | zeroship-worker `--socket` | empty |
| `worker.storage_url` | operational | `ZEROSHIP_WORKER_STORAGE_URL` | `worker.storage_url` | zeroship-worker `--storage-url` | empty |
| `worker.threads` | operational | `ZEROSHIP_WORKER_THREADS` | `worker.threads` | zeroship-worker `--threads` | `default_worker_threads()` |
| `worker.workflow_capacity` | operational | `ZEROSHIP_WORKER_WORKFLOW_CAPACITY` | `worker.workflow_capacity` | zeroship-worker `--workflow-capacity` | `64` |
| `worker.workflow_manager_url` | operational | `ZEROSHIP_WORKER_WORKFLOW_MANAGER_URL` | `worker.workflow_manager_url` | zeroship-worker `--workflow-manager-url` | empty |
| `worker.workflow_slots` | operational | `ZEROSHIP_WORKER_WORKFLOW_SLOTS` | `worker.workflow_slots` | zeroship-worker `--workflow-slots` | `4` |

### workflow

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `workflow.assignment_ttl_ms` | operational | `ZEROSHIP_WORKFLOW_ASSIGNMENT_TTL_MS` | `workflow.assignment_ttl_ms` | zeroship-workflow-server `--assignment-ttl-ms` | `30000` |
| `workflow.batch_limit` | operational | `ZEROSHIP_WORKFLOW_BATCH_LIMIT` | `workflow.batch_limit` | zeroship-workflow-server `--batch-limit` | `128` |
| `workflow.capacity_hold_down_ms` | operational | `ZEROSHIP_WORKFLOW_CAPACITY_HOLD_DOWN_MS` | `workflow.capacity_hold_down_ms` | zeroship-workflow-server `--capacity-hold-down-ms` | `300_000` |
| `workflow.capacity_max_slots` | operational | `ZEROSHIP_WORKFLOW_CAPACITY_MAX_SLOTS` | `workflow.capacity_max_slots` | zeroship-workflow-server `--capacity-max-slots` | `1024` |
| `workflow.capacity_min_slots` | operational | `ZEROSHIP_WORKFLOW_CAPACITY_MIN_SLOTS` | `workflow.capacity_min_slots` | zeroship-workflow-server `--capacity-min-slots` | `0` |
| `workflow.capacity_request_timeout_ms` | operational | `ZEROSHIP_WORKFLOW_CAPACITY_REQUEST_TIMEOUT_MS` | `workflow.capacity_request_timeout_ms` | zeroship-workflow-server `--capacity-request-timeout-ms` | `10000` |
| `workflow.capacity_retry_interval_ms` | operational | `ZEROSHIP_WORKFLOW_CAPACITY_RETRY_INTERVAL_MS` | `workflow.capacity_retry_interval_ms` | zeroship-workflow-server `--capacity-retry-interval-ms` | `30000` |
| `workflow.closing_backoff_max_ms` | operational | `ZEROSHIP_WORKFLOW_CLOSING_BACKOFF_MAX_MS` | `workflow.closing_backoff_max_ms` | zeroship-workflow-server `--closing-backoff-max-ms` | `3_600_000` |
| `workflow.closing_backoff_ms` | operational | `ZEROSHIP_WORKFLOW_CLOSING_BACKOFF_MS` | `workflow.closing_backoff_ms` | zeroship-workflow-server `--closing-backoff-ms` | `60000` |
| `workflow.closing_idle_ms` | operational | `ZEROSHIP_WORKFLOW_CLOSING_IDLE_MS` | `workflow.closing_idle_ms` | zeroship-workflow-server `--closing-idle-ms` | `900_000` |
| `workflow.closing_timeout_ms` | operational | `ZEROSHIP_WORKFLOW_CLOSING_TIMEOUT_MS` | `workflow.closing_timeout_ms` | zeroship-workflow-server `--closing-timeout-ms` | `300_000` |
| `workflow.control_url` | operational | `ZEROSHIP_WORKFLOW_CONTROL_URL` | `workflow.control_url` | zeroship-workflow-server `--control-url` | empty |
| `workflow.database_acquire_timeout_ms` | operational | `ZEROSHIP_WORKFLOW_DATABASE_ACQUIRE_TIMEOUT_MS` | `workflow.database_acquire_timeout_ms` | zeroship-workflow-server `--database-acquire-timeout-ms` | `5000` |
| `workflow.database_command_timeout_ms` | operational | `ZEROSHIP_WORKFLOW_DATABASE_COMMAND_TIMEOUT_MS` | `workflow.database_command_timeout_ms` | zeroship-workflow-server `--database-command-timeout-ms` | `10000` |
| `workflow.database_connections` | operational | `ZEROSHIP_WORKFLOW_DATABASE_CONNECTIONS` | `workflow.database_connections` | zeroship-workflow-server `--database-connections` | `8` |
| `workflow.database_url` | secret | `ZEROSHIP_WORKFLOW_DATABASE_URL` | `workflow.database_url` | zeroship-workflow-server `--database-url-file` | - |
| `workflow.driver_interval_ms` | operational | `ZEROSHIP_WORKFLOW_DRIVER_INTERVAL_MS` | `workflow.driver_interval_ms` | zeroship-workflow-server `--driver-interval-ms` | `1000` |
| `workflow.driver_lane_timeout_ms` | operational | `ZEROSHIP_WORKFLOW_DRIVER_LANE_TIMEOUT_MS` | `workflow.driver_lane_timeout_ms` | zeroship-workflow-server `--driver-lane-timeout-ms` | `10000` |
| `workflow.http_threads` | operational | `ZEROSHIP_WORKFLOW_HTTP_THREADS` | `workflow.http_threads` | zeroship-workflow-server `--http-threads` | `2` |
| `workflow.listen` | operational | `ZEROSHIP_WORKFLOW_LISTEN` | `workflow.listen` | zeroship-workflow-server `--listen` | `127.0.0.1:9093` |
| `workflow.max_connections` | operational | `ZEROSHIP_WORKFLOW_MAX_CONNECTIONS` | `workflow.max_connections` | zeroship-workflow-server `--max-connections` | `1024` |
| `workflow.max_pending_management` | operational | `ZEROSHIP_WORKFLOW_MAX_PENDING_MANAGEMENT` | `workflow.max_pending_management` | zeroship-workflow-server `--max-pending-management` | `1024` |
| `workflow.max_request_bytes` | operational | `ZEROSHIP_WORKFLOW_MAX_REQUEST_BYTES` | `workflow.max_request_bytes` | zeroship-workflow-server `--max-request-bytes` | `crate::api::DEFAULT_MAX_REQUEST_BYTES` |
| `workflow.migrate_url` | operational | `ZEROSHIP_WORKFLOW_MIGRATE_URL` | `workflow.migrate_url` | zeroship-workflow-server `--migrate-url` | empty |
| `workflow.policy_cache_entries` | operational | `ZEROSHIP_WORKFLOW_POLICY_CACHE_ENTRIES` | `workflow.policy_cache_entries` | zeroship-workflow-server `--policy-cache-entries` | `1024` |
| `workflow.replay_sweep_ms` | operational | `ZEROSHIP_WORKFLOW_REPLAY_SWEEP_MS` | `workflow.replay_sweep_ms` | zeroship-workflow-server `--replay-sweep-ms` | `30000` |
| `workflow.service_key_file` | operational | `ZEROSHIP_WORKFLOW_SERVICE_KEY_FILE` | `workflow.service_key_file` | zeroship-workflow-server `--service-key-file` | empty |
| `workflow.service_peers_file` | operational | `ZEROSHIP_WORKFLOW_SERVICE_PEERS_FILE` | `workflow.service_peers_file` | zeroship-workflow-server `--service-peers-file` | empty |
| `workflow.worker_ttl_ms` | operational | `ZEROSHIP_WORKFLOW_WORKER_TTL_MS` | `workflow.worker_ttl_ms` | zeroship-workflow-server `--worker-ttl-ms` | `30000` |

<!-- END GENERATED CONFIGURATION CONTRACT -->

### Couplings the table cannot show

Three constraints hold between the rows above, and no single row can express
them:

- The pairwise-salt value on control and the gateway and the contents of the
  auth service's salt file must be the same bytes. All three derive the same
  per-app anchor from it, and the file holds exactly those bytes with no
  trailing newline.
- The gateway's broker-secret file and the auth service's broker-secret file
  name one physical file, read as raw bytes on both sides with no normalization,
  so the bytes must match.
- `auth_provider` is one value for two services: auth serves the provider and
  control verifies its tokens. `native` means the platform's own issuer.

---

## Hand-maintained: the deployment surface

The deployment's own `.env` is the operator's surface. A name set there reaches
its container under the same name. Every setting in the contract table above
either has a working default or belongs to a service the stack does not run.

Provision the file and its secret directory before the first run. The command
is idempotent: existing valid values are kept, missing values are added, and a
conflict fails without rotating either side. Both locations can be overridden.

### Required on a real host

| Variable | Default | Why it must change |
| --- | --- | --- |
| `ZEROSHIP_IMAGE` | none | Which built image to run. |
| `ZEROSHIP_SECRETS_DIR` | `./secrets` | Where the file-backed key material lives. |
| `ZEROSHIP_DOMAIN` | `zeroship.localhost` | Drives the app base domain, the issuer, the gateway and auth public URLs, and the edge's site blocks and network aliases. |
| `ZEROSHIP_ORIGIN_SCHEME` | `http` | The scheme the public URLs advertise. Behind a TLS-terminating proxy the origin serves http while the public URLs say https. |

### Optional, and compose-local rather than a service setting

| Variable | Default | When to set it |
| --- | --- | --- |
| `ZEROSHIP_COMPOSE_SUBNET` | `172.30.0.0/16` | The subnet of the deployment's default network. It is pinned rather than driver-assigned because worker enrolment validates an observed peer address against a declared CIDR list. Set it if that range collides with another network on the host. |

### Generated scalar secrets

The provisioning command adds these to the gitignored `.env`, each generated
from 32 random bytes as lowercase hex. Rerunning keeps an existing valid value
rather than rotating it:

`ZEROSHIP_CONTROL_KEY` `ZEROSHIP_CONTROL_MASTER_KEY`
`ZEROSHIP_GATEWAY_STASH_SIGNING_KEY` `ZEROSHIP_PAIRWISE_SALT`
`ZEROSHIP_AUTH_STASH_SIGNING_KEY` `ZEROSHIP_AUTH_TOTP_ENC_KEY`
`ZEROSHIP_MIGRATE_SERVER_POLICY_SEAL_KEY`

The deployment requires these rather than falling back to a weak default, so one
generated value moves every consumer together. The control key reaches control,
the gateway, the worker and the migration service.

### Database DSNs an operator may override

All default to the deployment's own PostgreSQL, and each name reaches its
container under the same name:

| Variable |
| --- |
| `ZEROSHIP_CONTROL_DATABASE_URL` |
| `ZEROSHIP_GATEWAY_DATABASE_URL` |
| `ZEROSHIP_WORKER_DATABASE_URL` |
| `ZEROSHIP_AUTH_DATABASE_URL` |
| `ZEROSHIP_MIGRATE_SERVER_DATABASE_URL` |
| `ZEROSHIP_MIGRATE_SERVER_PROVISION_DATABASE_URL` |
| `ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET` |

The provisioning DSN is the one that must be privileged: the migration service
uses it to create each deployed app's schema and role. Because it is privileged,
its default is a mounted secret file rather than a literal DSN; setting the
variable to a literal DSN still works.

Optional: `OPENAI_API_KEY`, which defaults to empty.

The platform migration one-shot is not in the table because it takes no DSN from
the environment at all: it reads a mounted secret file. Its DSN is the database
superuser, so it is the most valuable credential in the deployment.

### Operational literals not supplied by the generator

A few values are literals in the deployment definition and ignore `.env`:

| Service | Variable | Value |
| --- | --- | --- |
| control | `ZEROSHIP_CONTROL_URL` | the control service's address |
| postgres | `POSTGRES_PASSWORD`, `POSTGRES_DB` | the database's password and name |

### Not environment variables at all

Several secrets are supplied as files in the secrets directory rather than as
variables, and the stack will not boot without them. One is a versioned keyring:
each nonempty line holds a version and a key that decodes to at least 32 bytes.
The generation recipes and the deploy walkthrough are in the runbooks.

---

## Hand-maintained: declared reads outside the contract

Not every environment read is a server setting. Some names are read by the
creator CLI, by the single-tenant dev runtime, or by a library inside a
service; each is a declared key with a class and a consumer.

**Creator CLI, tooling rather than app code:** `ZEROSHIP_TOKEN`
`ZEROSHIP_CONTROL_URL` `ZEROSHIP_CONFIG` `ZEROSHIP_CONFIG_HOME`
`ZEROSHIP_KV_PATH` `ZEROSHIP_KV_CONFIG_FILE` `ZEROSHIP_STORAGE_URL`
`ZEROSHIP_HEAP_LIMIT_MB` `ZEROSHIP_LOG_FORMAT` `ZEROSHIP_DIE_WITH_PARENT`

`ZEROSHIP_CONFIG` is one name over two contracts. For a server process it names
the operator's overlay; for the creator toolchain it names the project file
`zeroship.jsonc`, and the Vite plugin reads the same name. Both take it second,
after an explicit flag or plugin option, and a path that does not exist is an
error rather than a fall-through to auto-discovery. Exporting it in a shell that
runs both a server process and a deploy points each at a file written for the
other, so scope it to the command. See [`zeroship.jsonc`](../reference/project-config.md).

**Dev-only:** `ZEROSHIP_DEV` `ZEROSHIP_DEV_AUTH_SECRET`

`ZEROSHIP_RUNTIME_DESCRIPTOR` is not a setting. `zeroship serve` composes the
databases document from the deployment it loads and installs it under that name
for the runtime to read; a value standing in the environment is replaced, or
removed when the deployment declares no database, before any isolate is built.

**Platform-internal, read by a library rather than parsed as a setting:**
`ZEROSHIP_LOG` `ZEROSHIP_NET_GLOBAL_MAX_SOCKETS`
`ZEROSHIP_NET_RESOLVE_TIMEOUT_MS` `ZEROSHIP_STREAM_GLOBAL_CAP`
`ZEROSHIP_STORAGE_MAX_OBJECT_BYTES` `ZEROSHIP_STORAGE_MAX_STREAM_BYTES`
`ZEROSHIP_STORAGE_MAX_LIVE_GET_STREAMS` `ZEROSHIP_STORAGE_UPLOAD_CONCURRENCY`
`CONTROL_USAGE_OUTBOX_WAL_PATH` `CONTROL_DEPLOY_RETENTION_BATCH_SIZE`
`CONTROL_DEPLOY_RETENTION_GRACE_WINDOW_MS`
`CONTROL_WORKFLOW_RETENTION_BATCH_SIZE` `CONTROL_WORKFLOW_RETENTION_WINDOW_MS`

None of these is creator-facing.

**Readable by app code:** `ZEROSHIP_DEPLOY_ID`, covered under
[names the platform provides](../reference/env-vars.md#names-the-platform-provides). A process also
inherits ambient names it does not own - `PATH`, `HOME`, `HOSTNAME`, `PORT`,
`CI`, `XDG_CONFIG_HOME`, `SSL_CERT_FILE`, `SSL_CERT_DIR` and the AWS credential
names - as well as `DATABASE_URL` for the dev provisioning tool. Do not read an
ambient name from app code: it is present only because a process inherited it.

---

## Hand-maintained: external services

Names owned by another product. Nothing in zeroship can generate them, and
changing one here changes nothing about zeroship's own configuration.

- **Stripe**: `STRIPE_SECRET_KEY` `STRIPE_WEBHOOK_SECRET`, the spellings
  Stripe's own tooling and the live end-to-end suites use. The platform's own
  reads are the `control.stripe_*` rows in the table above.
- **Supabase / GoTrue**, an alternate auth provider: `SUPABASE_JWT_SECRET`
  `SUPABASE_SERVICE_ROLE_KEY` `SUPABASE_ANON_KEY` `GOTRUE_*`.
- **Lago / OpenMeter**, alternate billing providers: configured through
  `control.provider_config`, not through per-vendor variables.
- **AWS / S3**: `AWS_ACCESS_KEY_ID` `AWS_SECRET_ACCESS_KEY`
  `AWS_SESSION_TOKEN`, read by the object-storage client.

---

## Hand-maintained: JavaScript packages

These are read by the tooling packages, not by app code.

### @zeroship/vite-plugin

`ZEROSHIP_BIN` (override the `zeroship` binary the dev bootstrap spawns)
`ZEROSHIP_CONFIG` `ZEROSHIP_ENTRY` `ZEROSHIP_DEV` `ZEROSHIP_VITE_ORIGIN`
`ZEROSHIP_DIE_WITH_PARENT`
`NAPI_RS_NATIVE_LIBRARY_PATH` `DATABASE_URL` `OPENAI_API_KEY` `NODE_ENV`

`ZEROSHIP_BIN` is worth knowing: `pnpm dev` spawns a release binary, so a stale
one reproduces already-fixed bugs. This is the override.

### Other packages

`@zeroship/migrate`: `ZEROSHIP_MIGRATE_NATIVE` `GEN_IR_OUT`
`GEN_DIALECT_TS_OUT` `GEN_DIALECT_RUST_OUT`

`@zeroship/mcp`: `ZEROSHIP_CONTROL_URL` `ZEROSHIP_TOKEN`

The examples read `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`, `BASE_URL`,
`CHROMIUM_PATH`, `PLAYWRIGHT_PATH` and `PLAYWRIGHT_BROWSERS_PATH`, plus a
per-example dev port.

---

## Hand-maintained: test-only

Reachable only from test code. Setting any of them in a deployment does
nothing.

### The test backends live in a config file, not in variables

The test PostgreSQL instance is named once in a generated, gitignored
configuration file rather than in an environment variable, because its database
and key-value values contain credentials for the local instances. A misspelled
key there is an error rather than a value that configures nothing.

### The surviving test-only names

`PG_TEST_URL` is the PostgreSQL test override. It redirects the PostgreSQL
suites; it does not enable them. The key-value and Redis driver suites start
their own containers and fail when the container runtime cannot provide them, so
there is no Redis test address override and no opt-in switch.

CI also sets `PG_CONTAINER`, `PG_HOST`, `PG_PORT`, `PG_USER`, `PG_PASS`,
`POSTGRES_USER`, `POSTGRES_PASSWORD` and `ZS_FRESHNESS_STRICT`. The host and
port names are inputs to the provisioning script, which writes what they resolve
to into the generated file. Mailer tests own their database and mail containers
and accept no address override.

---

## Regenerating and auditing

Maintainer-only. The generated region above is rendered from the platform's
compiled declarations, so editing it by hand is pointless: the next render
overwrites it. Two audit commands keep it honest by re-rendering it and by
comparing it without writing. A third reads the deployment's `.env` surface, and
a fourth finds the JavaScript packages' environment reads, which no platform
declaration can see.

Run these under a shell that word-splits unquoted variables, so a multi-root
scan cannot collapse into one bogus path and report a confident zero.

---

## Gates that are NOT armed, and why

Recorded here so their absence is a stated position rather than an oversight.

Nothing enumerates the ambient `ZEROSHIP_*` names at process startup and rejects
the ones a service does not consume. A naive prefix rule would be wrong: several
names in that space are read by a library inside the same process without being
settings, so the allowed set has to be the union of the declared settings and
the declared per-consumer reads, and that union does not exist yet.
