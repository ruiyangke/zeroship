# Environment variables

This file has TWO HALVES and they carry different guarantees.

**The generated half** is the table between the
`BEGIN/END GENERATED CONFIGURATION CONTRACT` markers below. It is rendered from
the COMPILED `ConfigSpec` registries of the six declaring binaries by

```bash
cargo run -p zeroship-config-contract -- env-vars-doc
```

Every name in it is the name the binary parses, because both are the same
compiled value. It cannot be stale: `tests/config_name_alignment_gate.sh` fails
CI when the committed region differs from a fresh render.

**The hand-maintained half** is everything else. It documents what zeroship does
NOT declare - Stripe, Supabase, AWS and other third-party names, the Compose
`.env` interpolation surface an operator sets, the file-backed key material, the
JavaScript packages' `process.env` reads, and the test-only names. No registry
can re-derive those, so they are written by hand and the generator never touches
them. Treat a name in this half as evidence that someone checked it once, not as
a contract.

**What neither half covers**, so a gap here is not evidence of absence:

- Shell suites under `tests/` set and read many variables of their own.
- Names built at runtime by string concatenation are invisible to a text scan.
- The vendored `third_party/zero-migrate` engine is out of scope.
- Third-party crates and npm packages read their own variables (`RUST_LOG`,
  `NODE_ENV`, `AWS_*`, `PLAYWRIGHT_*`); only those the repo reads directly are
  listed.

---

## Read the naming rule first

Every operational value and every secret on control, gateway, worker, auth,
migrated and the workflow scheduler is generated from ONE canonical identity, so
its environment name is always `ZEROSHIP_<CANONICAL>` and its overlay path is the
canonical name itself: `control.port` gives `ZEROSHIP_CONTROL_PORT` and
`[control] port`. There is no alias for any old spelling - `GATE_PORT`,
`MAX_ISOLATES`, `WORKER_URLS`, `DATABASE_URL` and the rest are simply gone.

What is different about a SECRET is its FLAG: it gets exactly one, a
`--<name>-file PATH`, and never a value flag, so secret material cannot reach a
process argument list.

```
control_key          ZEROSHIP_CONTROL_KEY            --control-key-file
control.master_key   ZEROSHIP_CONTROL_MASTER_KEY     --master-key-file
gateway.database_url ZEROSHIP_GATEWAY_DATABASE_URL   --database-url-file
```

LOCATION ENCODES SHARING. A canonical name with no scope prefix is one several
binaries read; a name under `control.` is control's alone. The overlay follows
the same rule, so a key at the root of `zeroship.toml` is shared and a key under
`[control]` is not.

The alias hop is gone. `deploy/ops/zeroship.toml` used to carry a `[secrets]`
block whose every entry was a `urn:zeroship:env:<VAR>` reference - a config file
naming an environment variable so that a compose service could set the value
under a third name. That whole table is deleted, and so is the env-to-env
reference scheme that expressed it.

An overlay value is EITHER the secret itself OR `urn:zeroship:file:/abs/path`.
There is no third form: the Vault and AWS Secrets Manager spellings parsed but
always failed at boot, so they were syntax for a source that did not exist, and
they are deleted too. A literal is permitted, because the overlay may itself be
a mounted Kubernetes Secret - but never in a TRACKED file.

Under `--check-config` nothing is opened and no reference is followed. The run
establishes which source supplies each secret, validates that source's policy
and format, and reports presence only.

The overlay is auto-discovered at `/etc/zeroship/zeroship.toml`, or pointed at
with `ZEROSHIP_CONFIG`.

---

## The zeroship-owned contract

Read this table as: supply the value by the environment name, by the flag your
binary spells, or at the overlay path - whichever the class allows. A `-` means
that class has no such tier at all, which is a guarantee rather than an omission:
a command control has no environment name, so no ambient variable can trigger it.

<!-- BEGIN GENERATED CONFIGURATION CONTRACT -->
<!--
DO NOT EDIT THIS REGION BY HAND. It is rendered from the COMPILED
ConfigSpec registries of the six declaring binaries by
`cargo run -p zeroship-config-contract -- env-vars-doc`, and
tests/config_name_alignment_gate.sh fails when it drifts. Everything
OUTSIDE these two markers is hand-maintained and is never rewritten
by the generator.
-->

**150 canonical settings**: 112 operational, 31 secret, 5 bootstrap controls, 2 command controls. Every environment name below is `ZEROSHIP_<CANONICAL>` and every overlay path is the canonical name itself, because both are computed from the one declaration rather than spelled twice.

### shared (no scope prefix: read by more than one binary)

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `blob_store` | operational | `ZEROSHIP_BLOB_STORE` | `blob_store` | zeroship-control `--blob-store`<br>zeroship-gate `--blob-store`<br>zeroship-worker `--blob-store` | `./bundles` |
| `check_config` | command control | - | - | zeroship-auth `--check-config`<br>zeroship-control `--check-config`<br>zeroship-gate `--check-config`<br>zeroship-migrated `--check-config`<br>zeroship-worker `--check-config`<br>zeroship-workflow-scheduler `--check-config` | - |
| `check_config_format` | command control | - | - | zeroship-auth `--check-config-format`<br>zeroship-control `--check-config-format`<br>zeroship-gate `--check-config-format`<br>zeroship-migrated `--check-config-format`<br>zeroship-worker `--check-config-format`<br>zeroship-workflow-scheduler `--check-config-format` | `CheckFormat::Text` |
| `config` | bootstrap control | `ZEROSHIP_CONFIG` | - | zeroship-auth `--config`<br>zeroship-control `--config`<br>zeroship-gate `--config`<br>zeroship-migrated `--config`<br>zeroship-worker `--config`<br>zeroship-workflow-scheduler `--config` | - |
| `control_key` | secret | `ZEROSHIP_CONTROL_KEY` | `control_key` | zeroship-auth `--control-key-file`<br>zeroship-control `--control-key-file`<br>zeroship-gate `--control-key-file`<br>zeroship-migrated `--control-key-file`<br>zeroship-worker `--control-key-file` | - |
| `control_url` | operational | `ZEROSHIP_CONTROL_URL` | `control_url` | zeroship-auth `--control-url`<br>zeroship-gate `--control-url`<br>zeroship-worker `--control-url` | `http://localhost:9090` |
| `no_config` | bootstrap control | `ZEROSHIP_NO_CONFIG` | - | zeroship-auth `--no-config`<br>zeroship-control `--no-config`<br>zeroship-gate `--no-config`<br>zeroship-migrated `--no-config`<br>zeroship-worker `--no-config`<br>zeroship-workflow-scheduler `--no-config` | - |
| `oauth_audience` | operational | `ZEROSHIP_OAUTH_AUDIENCE` | `oauth_audience` | zeroship-control `--oauth-audience`<br>zeroship-migrated `--oauth-audience` | `control.zeroship.ai` |
| `origin_scheme` | operational | `ZEROSHIP_ORIGIN_SCHEME` | `origin_scheme` | zeroship-control `--origin-scheme`<br>zeroship-gate `--origin-scheme` | `OriginScheme::Https` |
| `pairwise_salt` | secret | `ZEROSHIP_PAIRWISE_SALT` | `pairwise_salt` | zeroship-control `--pairwise-salt-file`<br>zeroship-gate `--pairwise-salt-file` | - |
| `poll_interval` | operational | `ZEROSHIP_POLL_INTERVAL` | `poll_interval` | zeroship-gate `--poll-interval`<br>zeroship-worker `--poll-interval` | `5` |
| `trust_proxy` | operational | `ZEROSHIP_TRUST_PROXY` | `trust_proxy` | zeroship-control `--trust-proxy`<br>zeroship-gate `--trust-proxy` | `false` |
| `trusted_origins` | operational | `ZEROSHIP_TRUSTED_ORIGINS` | `trusted_origins` | zeroship-gate `--trusted-origins` | empty |
| `worker_key` | secret | `ZEROSHIP_WORKER_KEY` | `worker_key` | zeroship-control `--worker-key-file`<br>zeroship-gate `--worker-key-file`<br>zeroship-worker `--worker-key-file` | - |
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
| `auth.platform_issuer` | operational | `ZEROSHIP_AUTH_PLATFORM_ISSUER` | `auth.platform_issuer` | zeroship-control `--auth-platform-issuer`<br>zeroship-migrated `--auth-platform-issuer` | empty |
| `auth.platform_jwks_url` | operational | `ZEROSHIP_AUTH_PLATFORM_JWKS_URL` | `auth.platform_jwks_url` | zeroship-control `--auth-platform-jwks-url`<br>zeroship-migrated `--auth-platform-jwks-url` | empty |
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
| `control.database_url` | secret | `ZEROSHIP_CONTROL_DATABASE_URL` | `control.database_url` | zeroship-control `--database-url-file` | - |
| `control.deploy_tmp_dir` | operational | `ZEROSHIP_CONTROL_DEPLOY_TMP_DIR` | `control.deploy_tmp_dir` | zeroship-control `--deploy-tmp-dir` | empty |
| `control.disable_workflow_engine` | bootstrap control | - | - | zeroship-control `--disable-workflow-engine` | - |
| `control.gateway_url` | operational | `ZEROSHIP_CONTROL_GATEWAY_URL` | `control.gateway_url` | zeroship-control `--gateway-url` | `http://localhost` |
| `control.invoicer_provider` | operational | `ZEROSHIP_CONTROL_INVOICER_PROVIDER` | `control.invoicer_provider` | zeroship-control `--invoicer-provider` | `lite` |
| `control.legacy_master_keys` | secret | `ZEROSHIP_CONTROL_LEGACY_MASTER_KEYS` | `control.legacy_master_keys` | zeroship-control `--legacy-master-keys-file` | - |
| `control.mailer` | operational | `ZEROSHIP_CONTROL_MAILER` | `control.mailer` | zeroship-control `--mailer` | `stdout` |
| `control.master_key` | secret | `ZEROSHIP_CONTROL_MASTER_KEY` | `control.master_key` | zeroship-control `--master-key-file` | - |
| `control.meter_provider` | operational | `ZEROSHIP_CONTROL_METER_PROVIDER` | `control.meter_provider` | zeroship-control `--meter-provider` | `lite` |
| `control.port` | operational | `ZEROSHIP_CONTROL_PORT` | `control.port` | zeroship-control `--port` | `9090` |
| `control.provider_config` | operational | `ZEROSHIP_CONTROL_PROVIDER_CONFIG` | `control.provider_config` | zeroship-control `--provider-config` | `{}` |
| `control.resend_api_key` | secret | `ZEROSHIP_CONTROL_RESEND_API_KEY` | `control.resend_api_key` | zeroship-control `--resend-api-key-file` | - |
| `control.signing_key_file` | operational | `ZEROSHIP_CONTROL_SIGNING_KEY_FILE` | `control.signing_key_file` | zeroship-control `--signing-key-file` | empty |
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
| `gateway.signing_key_file` | operational | `ZEROSHIP_GATEWAY_SIGNING_KEY_FILE` | `gateway.signing_key_file` | zeroship-gate `--signing-key-file` | empty |
| `gateway.stash_signing_key` | secret | `ZEROSHIP_GATEWAY_STASH_SIGNING_KEY` | `gateway.stash_signing_key` | zeroship-gate `--stash-signing-key-file` | - |

### migrated

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `migrated.bind` | operational | `ZEROSHIP_MIGRATED_BIND` | `migrated.bind` | zeroship-migrated `--bind` | `127.0.0.1` |
| `migrated.database_url` | secret | `ZEROSHIP_MIGRATED_DATABASE_URL` | `migrated.database_url` | zeroship-migrated `--database-url-file` | - |
| `migrated.policy_ceiling_version` | operational | `ZEROSHIP_MIGRATED_POLICY_CEILING_VERSION` | `migrated.policy_ceiling_version` | zeroship-migrated `--policy-ceiling-version` | `1` |
| `migrated.policy_seal_key` | secret | `ZEROSHIP_MIGRATED_POLICY_SEAL_KEY` | `migrated.policy_seal_key` | zeroship-migrated `--policy-seal-key-file` | - |
| `migrated.port` | operational | `ZEROSHIP_MIGRATED_PORT` | `migrated.port` | zeroship-migrated `--port` | `9091` |
| `migrated.provision_database_url` | secret | `ZEROSHIP_MIGRATED_PROVISION_DATABASE_URL` | `migrated.provision_database_url` | zeroship-migrated `--provision-database-url-file` | - |
| `migrated.signing_key_file` | operational | `ZEROSHIP_MIGRATED_SIGNING_KEY_FILE` | `migrated.signing_key_file` | zeroship-migrated `--signing-key-file` | empty |
| `migrated.tmp_dir` | operational | `ZEROSHIP_MIGRATED_TMP_DIR` | `migrated.tmp_dir` | zeroship-migrated `--tmp-dir` | `std::env::temp_dir().join("zeroship-migrated")` |

### observability

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `observability.log_filter` | operational | `ZEROSHIP_OBSERVABILITY_LOG_FILTER` | `observability.log_filter` | zeroship-auth `--observability-log-filter`<br>zeroship-control `--observability-log-filter`<br>zeroship-gate `--observability-log-filter`<br>zeroship-migrated `--observability-log-filter`<br>zeroship-worker `--observability-log-filter`<br>zeroship-workflow-scheduler `--observability-log-filter` | `DEFAULT_LOG_FILTER` |
| `observability.log_format` | operational | `ZEROSHIP_OBSERVABILITY_LOG_FORMAT` | `observability.log_format` | zeroship-auth `--observability-log-format`<br>zeroship-control `--observability-log-format`<br>zeroship-gate `--observability-log-format`<br>zeroship-migrated `--observability-log-format`<br>zeroship-worker `--observability-log-format`<br>zeroship-workflow-scheduler `--observability-log-format` | `LogFormat::Auto` |

### worker

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `worker.bind` | operational | `ZEROSHIP_WORKER_BIND` | `worker.bind` | zeroship-worker `--bind` | `127.0.0.1` |
| `worker.database_url` | secret | `ZEROSHIP_WORKER_DATABASE_URL` | `worker.database_url` | zeroship-worker `--database-url-file` | - |
| `worker.kv_url` | secret | `ZEROSHIP_WORKER_KV_URL` | `worker.kv_url` | zeroship-worker `--kv-url-file` | - |
| `worker.max_isolates` | operational | `ZEROSHIP_WORKER_MAX_ISOLATES` | `worker.max_isolates` | zeroship-worker `--max-isolates` | `200` |
| `worker.max_pinned_isolates_per_app` | operational | `ZEROSHIP_WORKER_MAX_PINNED_ISOLATES_PER_APP` | `worker.max_pinned_isolates_per_app` | zeroship-worker `--max-pinned-isolates-per-app` | `4` |
| `worker.max_step_blob_bytes` | operational | `ZEROSHIP_WORKER_MAX_STEP_BLOB_BYTES` | `worker.max_step_blob_bytes` | zeroship-worker `--max-step-blob-bytes` | `67_108_864` |
| `worker.port` | operational | `ZEROSHIP_WORKER_PORT` | `worker.port` | zeroship-worker `--port` | `8080` |
| `worker.shutdown_timeout` | operational | `ZEROSHIP_WORKER_SHUTDOWN_TIMEOUT` | `worker.shutdown_timeout` | zeroship-worker `--shutdown-timeout` | `30` |
| `worker.socket` | operational | `ZEROSHIP_WORKER_SOCKET` | `worker.socket` | zeroship-worker `--socket` | empty |
| `worker.storage_url` | operational | `ZEROSHIP_WORKER_STORAGE_URL` | `worker.storage_url` | zeroship-worker `--storage-url` | empty |
| `worker.threads` | operational | `ZEROSHIP_WORKER_THREADS` | `worker.threads` | zeroship-worker `--threads` | `default_worker_threads()` |
| `worker.workflow_advance_unsigned` | bootstrap control | - | - | zeroship-worker `--workflow-advance-unsigned` | - |

### workflow-scheduler

| Canonical | Class | Environment | Overlay path | Flag by binary | Default |
| --- | --- | --- | --- | --- | --- |
| `workflow_scheduler.control_apply_url` | operational | `ZEROSHIP_WORKFLOW_SCHEDULER_CONTROL_APPLY_URL` | `workflow_scheduler.control_apply_url` | zeroship-workflow-scheduler `--control-apply-url` | empty |
| `workflow_scheduler.database_url` | secret | `ZEROSHIP_WORKFLOW_SCHEDULER_DATABASE_URL` | `workflow_scheduler.database_url` | zeroship-workflow-scheduler `--database-url-file` | - |
| `workflow_scheduler.gateway_url` | operational | `ZEROSHIP_WORKFLOW_SCHEDULER_GATEWAY_URL` | `workflow_scheduler.gateway_url` | zeroship-workflow-scheduler `--gateway-url` | empty |
| `workflow_scheduler.inflight_ttl_ms` | operational | `ZEROSHIP_WORKFLOW_SCHEDULER_INFLIGHT_TTL_MS` | `workflow_scheduler.inflight_ttl_ms` | zeroship-workflow-scheduler `--inflight-ttl-ms` | `120_000` |
| `workflow_scheduler.max_due_per_tick` | operational | `ZEROSHIP_WORKFLOW_SCHEDULER_MAX_DUE_PER_TICK` | `workflow_scheduler.max_due_per_tick` | zeroship-workflow-scheduler `--max-due-per-tick` | `64` |
| `workflow_scheduler.max_loaded_timers` | operational | `ZEROSHIP_WORKFLOW_SCHEDULER_MAX_LOADED_TIMERS` | `workflow_scheduler.max_loaded_timers` | zeroship-workflow-scheduler `--max-loaded-timers` | `1_024` |
| `workflow_scheduler.near_horizon_ms` | operational | `ZEROSHIP_WORKFLOW_SCHEDULER_NEAR_HORIZON_MS` | `workflow_scheduler.near_horizon_ms` | zeroship-workflow-scheduler `--near-horizon-ms` | `60_000` |
| `workflow_scheduler.reaper_interval_secs` | operational | `ZEROSHIP_WORKFLOW_SCHEDULER_REAPER_INTERVAL_SECS` | `workflow_scheduler.reaper_interval_secs` | zeroship-workflow-scheduler `--reaper-interval-secs` | `30` |
| `workflow_scheduler.schema` | operational | `ZEROSHIP_WORKFLOW_SCHEDULER_SCHEMA` | `workflow_scheduler.schema` | zeroship-workflow-scheduler `--schema` | `DEFAULT_SCHEDULER_SCHEMA` |
| `workflow_scheduler.tick_secs` | operational | `ZEROSHIP_WORKFLOW_SCHEDULER_TICK_SECS` | `workflow_scheduler.tick_secs` | zeroship-workflow-scheduler `--tick-secs` | `DEFAULT_TICK_SECS` |

<!-- END GENERATED CONFIGURATION CONTRACT -->

### Couplings the table cannot show

Three constraints hold BETWEEN the rows above, and no per-setting registry can
express them:

- The `pairwise_salt` value on control and gateway and the
  `auth.pairwise_salt_file` CONTENT on auth must be the same bytes. All three
  derive the same per-app `pws_` anchor from it. The file contains exactly the
  env bytes with no trailing newline. Rotating it requires a per-app `pws_`
  migration, so it is permanent in practice.
- `gateway.broker_secret_file` and `auth.broker_secret_file` name ONE physical
  file, read as raw bytes on both sides with no normalization. Same bytes is the
  requirement; `openssl rand -base64 48 > secret` and
  `head -c 32 /dev/urandom > secret` both work only because neither side trims.
- `auth_provider` is one value for two binaries: auth SERVES the provider and
  control VERIFIES its tokens. `native` means the platform's own OP is the
  issuer. Control used to have a separate `ZEROSHIP_CONTROL_AUTH_PROVIDER` that
  spelled this state `platform`, so the pair could be set to disagree; it is
  gone, as is its `[control] auth_provider` overlay key.

---

## Hand-maintained: the deployment surface

What an operator actually sets in `deploy/compose/.env`. THESE ARE NOT NAMES ANY
BINARY READS. Compose interpolates them into the canonical container variables
above, so `CONTROL_DATABASE_URL` in `.env` becomes
`ZEROSHIP_CONTROL_DATABASE_URL` in the control container. Everything else in the
contract table has a working default or belongs to a service the stack does not
run.

Before the first local compose run, provision the file and its sibling secret
directory from the repository root:

```bash
zeroship dev init
```

The defaults are `deploy/compose/secrets` and `deploy/compose/.env`. To place
them elsewhere, pass `--secrets-dir=PATH` and `--env-file=PATH`. The command is
idempotent: existing valid values are retained, missing values are added, and a
conflict fails without rotating either side.

### Required on a real host

| Variable | Default | Why it must change |
| --- | --- | --- |
| `ZEROSHIP_IMAGE` | none | Which built image to run. Only referenced by the server-side compose override. |
| `ZEROSHIP_SECRETS_DIR` | `./secrets` | Where the file-backed key material lives. Relative to the compose file, this is `deploy/compose/secrets`, the `zeroship dev init` default. |
| `ZEROSHIP_DOMAIN` | `zeroship.localhost` | Drives `--app-base-domain`, the OIDC issuer, gateway/auth public URLs, Caddy's site blocks and its network aliases. |
| `ZEROSHIP_ORIGIN_SCHEME` | `http` | The scheme PUBLIC urls advertise. Compose injects it into control and gateway and uses it to build gateway/auth public URLs. Behind a TLS-terminating proxy the origin serves http while public URLs say https. |

### Generated scalar secrets

`zeroship dev init` adds these to the gitignored `.env`. Each is generated from
32 random bytes as lowercase hex. Rerunning keeps an existing valid value rather
than rotating it. Here the `.env` name and the container name coincide:

`ZEROSHIP_CONTROL_KEY` `ZEROSHIP_CONTROL_MASTER_KEY` `ZEROSHIP_WORKER_KEY`
`ZEROSHIP_GATEWAY_STASH_SIGNING_KEY` `ZEROSHIP_PAIRWISE_SALT`
`ZEROSHIP_AUTH_STASH_SIGNING_KEY` `ZEROSHIP_AUTH_TOTP_ENC_KEY`
`ZEROSHIP_MIGRATED_POLICY_SEAL_KEY`

Compose uses required `${VAR:?run zeroship dev init}` interpolation for these
values rather than built-in weak defaults. One generated value therefore moves
every consumer together. In particular, changing `ZEROSHIP_CONTROL_KEY` cannot
move auth alone while leaving the other four services behind.

### Database DSNs an operator may override

`.env` names on the left, canonical container name on the right. All default to
the in-compose Postgres:

| `.env` name | Container variable |
| --- | --- |
| `CONTROL_DATABASE_URL` | `ZEROSHIP_CONTROL_DATABASE_URL` |
| `GATEWAY_DATABASE_URL` | `ZEROSHIP_GATEWAY_DATABASE_URL` |
| `WORKER_DATABASE_URL` | `ZEROSHIP_WORKER_DATABASE_URL` |
| `AUTH_DB_URL` | `ZEROSHIP_AUTH_DATABASE_URL` |
| `MIGRATED_DATABASE_URL` | `ZEROSHIP_MIGRATED_DATABASE_URL` |
| `PROVISION_DATABASE_URL` | `ZEROSHIP_MIGRATED_PROVISION_DATABASE_URL` |
| `WORKER_KV_URL` | `ZEROSHIP_WORKER_KV_URL` |
| `STRIPE_WEBHOOK_SECRET` | `ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET` |

`PROVISION_DATABASE_URL` is the odd one and the only one that must be
privileged: `migrated` uses it to `CREATE SCHEMA "<app_id>"` and
`CREATE ROLE migrator_<app_id>` for each deployed app.

Optional: `OPENAI_API_KEY` (defaults empty), `ZEROSHIP_MIGRATE_DSN` (the platform
migration one-shot).

**These eight rows are the one place the deployment still spells a name twice.**
Section 4.3 of `docs/proposals/2026-08-11-config-name-alignment.md` wants
`LEFT == RIGHT` for any container value whose whole scalar is one interpolation,
which would make this table unrepresentable. It is not enforced today; see the
"not yet armed" note at the bottom of this file.

### Operational literals not supplied by the generator

These are literals in `deploy/compose/docker-compose.yml` and ignore `.env`:

| Service(s) | Variable | Value |
| --- | --- | --- |
| control | `SANDBOX_URL`, `SANDBOX_TOKEN` | `http://sandbox:9091`, a dev token |
| control | `ZEROSHIP_CONTROL_URL` | `http://control:9090` |
| postgres | `POSTGRES_PASSWORD`, `POSTGRES_DB` | `zeroship`, `zeroship` |

### Not environment variables at all

Seven secrets are read from files under `${ZEROSHIP_SECRETS_DIR}`. The stack
will not boot without them:

`control-signing.pem` `gateway-signing.pem` `auth-signing.pem` `broker-secret`
`pairwise-salt` `refresh-hash-key` `refresh-idem-key`

`refresh-hash-key` is a versioned keyring. Each nonempty line is
`version:hex-or-base64url-key` and decodes to at least 32 bytes; the generator
starts with `1:` followed by 48 random bytes encoded as hex.

Generation recipes are in `docs/runbooks/auth-deploy.md`; the deploy walkthrough
is in `docs/runbooks/deploy-server.md`.

---

## Hand-maintained: declared reads outside the contract

Not every environment read is a server SETTING. The creator CLI, the
single-tenant runtime, the data plugins and the test suites all read names that
no `ConfigSpec` declares, and they are not invented: each goes through a typed
declared key whose class the raw-environment gate checks. Re-derive the current
census with

```bash
cargo run -p zeroship-config-contract -- raw-env 2>&1 >/dev/null | grep class
```

**creator CLI** (`CliEnv`): `ZEROSHIP_TOKEN` `ZEROSHIP_CONTROL_URL`
`ZEROSHIP_CONFIG_HOME` `ZEROSHIP_KV_PATH` `ZEROSHIP_KV_URL`
`ZEROSHIP_STORAGE_URL` `ZEROSHIP_HEAP_LIMIT_MB` `ZEROSHIP_WORKFLOW_SQLITE_PATH`
`ZEROSHIP_LOG_FORMAT` `ZEROSHIP_DIE_WITH_PARENT`

**dev-only** (`DevEnv`): `ZEROSHIP_DEV` `ZEROSHIP_DEV_AUTH_SECRET`
`ZEROSHIP_DEV_INSECURE`

**platform-internal**, read by a library rather than parsed as a setting:
`ZEROSHIP_LOG` `ZEROSHIP_NET_GLOBAL_MAX_SOCKETS` `ZEROSHIP_NET_RESOLVE_TIMEOUT_MS`
`ZEROSHIP_STREAM_GLOBAL_CAP` `ZEROSHIP_STORAGE_MAX_OBJECT_BYTES`
`ZEROSHIP_STORAGE_MAX_STREAM_BYTES` `ZEROSHIP_STORAGE_MAX_LIVE_GET_STREAMS`
`ZEROSHIP_STORAGE_UPLOAD_CONCURRENCY` `CONTROL_USAGE_OUTBOX_WAL_PATH`
`CONTROL_DEPLOY_RETENTION_BATCH_SIZE` `CONTROL_DEPLOY_RETENTION_GRACE_WINDOW_MS`
`CONTROL_WORKFLOW_RETENTION_BATCH_SIZE` `CONTROL_WORKFLOW_RETENTION_WINDOW_MS`
`ZEROSHIP_COLUMN_KEY_<COLLECTION>` (a family, one name per collection)

**creator app** (`ZEROSHIP_DEPLOY_ID`), and the ambient/external names the
process inherits rather than owns: `PATH` `HOME` `HOSTNAME` `PORT` `CI`
`RUST_LOG` `XDG_CONFIG_HOME` `SSL_CERT_FILE` `SSL_CERT_DIR`
`AWS_ACCESS_KEY_ID` `AWS_SECRET_ACCESS_KEY` `AWS_SESSION_TOKEN`
`REDPANDA_BROKERS` `REDPANDA_PRODUCER_GROUP_ID` `USAGE_EVENTS_TOPIC`
`USAGE_OUTBOX_WAL_PATH` `DATABASE_URL` (the `dev-provision` dev tool only).

---

## Hand-maintained: external services

Names owned by somebody else's product. Nothing in this repo can generate them,
and changing one here changes nothing.

- **Stripe**: `STRIPE_SECRET_KEY` `STRIPE_WEBHOOK_SECRET` (the platform's own
  reads are the canonical `control.stripe_*` rows in the generated table; these
  are the spellings Stripe's own tooling and the live E2E suites use).
- **Supabase / GoTrue** (alternate auth provider):
  `SUPABASE_JWT_SECRET` `SUPABASE_SERVICE_ROLE_KEY` `SUPABASE_ANON_KEY`
  `GOTRUE_*`.
- **Lago / OpenMeter** (alternate billing providers): configured through
  `control.provider_config`, not through per-vendor variables.
- **AWS / S3**: `AWS_ACCESS_KEY_ID` `AWS_SECRET_ACCESS_KEY` `AWS_SESSION_TOKEN`,
  read by `compio-s3` through the declared external class.

---

## Hand-maintained: JavaScript packages

### @zeroship/vite-plugin

`ZEROSHIP_BIN` (override the `zeroship` binary the dev bootstrap spawns)
`ZEROSHIP_ENTRY` `ZEROSHIP_DEV` `ZEROSHIP_VITE_ORIGIN`
`ZEROSHIP_RUNTIME_DESCRIPTOR` `ZEROSHIP_DIE_WITH_PARENT`
`NAPI_RS_NATIVE_LIBRARY_PATH` `DATABASE_URL` `OPENAI_API_KEY` `NODE_ENV`

`ZEROSHIP_BIN` is worth knowing: `pnpm dev` spawns a RELEASE binary, so a stale
`target/release/zeroship` reproduces already-fixed bugs. This is the override.

### Other packages

`@zeroship/migrate`: `ZEROSHIP_MIGRATE_NATIVE` `GEN_IR_OUT`
`GEN_DIALECT_TS_OUT` `GEN_DIALECT_RUST_OUT`

`@zeroship/mcp`: `ZEROSHIP_CONTROL_URL` `ZEROSHIP_TOKEN`

`@zeroship/ui`: `STORYBOOK_URL` `STORYBOOK_DEV_URL` `CHROMIUM_PATH`
`THEME_EVIDENCE_DIR` `NODE_ENV`

Examples: `OPENAI_API_KEY` `ANTHROPIC_API_KEY` `BASE_URL` `CHROMIUM_PATH`
`PLAYWRIGHT_PATH` `PLAYWRIGHT_BROWSERS_PATH` and a per-example dev port
(`STARTER_API_PORT`, `DB_TODOS_API_PORT`, `DB_E2E_API_PORT`,
`DB_E2E_VITE_PORT`, `DB_CHAT_API_PORT`, `CSR_TODO_API_PORT`,
`HR_SYSTEM_API_PORT`, `STORAGE_GALLERY_API_PORT`).

---

## Hand-maintained: test-only

Reachable only from test code. Setting them in a deployment does nothing.

`CONTROL_TEST_DB` `MIGRATED_TEST_DB` `PG_TEST_URL` `REDIS_TEST_URL`
`LIVE_DB_TEST_URL` `ZEROSHIP_SCHEDULER_TEST_DB` `ZERO_MIGRATE_TEST_PG_URL`
`GATEWAY_ANCHORS_DB_URL` `GATEWAY_POOL_SMOKE_URL` `DRAGONFLY_CLUSTER_SEEDS`
`AUTH_TEST_SMTP_SINK` `KV_REQUIRE_REDIS` `ZEROSHIP_REQUIRE_LIVE_BACKENDS`
`ZEROSHIP_SESSION_SECRET` `ZEROSHIP_SESSION_SECRET_PREV`
`ZEROSHIP_SESSION_NONCE_CAPACITY` `ZEROSHIP_DW_E2E*` `ZEROSHIP_DW23_BENCH_*`
`ZEROSHIP_NET_TEST_DNS_HANG_HOST` `ZEROSHIP_NET_TEST_DNS_HANG_MS`

CI also sets `PG_CONTAINER` `PG_HOST` `PG_PORT` `PG_USER` `PG_PASS`
`POSTGRES_USER` `POSTGRES_PASSWORD` `REDPANDA_BROKERS` `ZS_FRESHNESS_STRICT`.

---

## Regenerating and auditing

```bash
# Rewrite the generated region above from the COMPILED contract.
cargo run -p zeroship-config-contract -- env-vars-doc

# Verify it, exactly as CI does.
cargo run -p zeroship-config-contract -- env-vars-doc --check

# THE AUDIT. Re-derive every projection a SECOND time by parsing the source
# text with syn, and require the two sets to be equal in both directions. The
# extraction is no longer the source of truth for this document; its job is to
# disagree with the compiled contract if either one is wrong.
cargo run -p zeroship-config-contract -- audit

# The declared non-contract census behind the hand-maintained sections.
cargo run -p zeroship-config-contract -- raw-env 2>&1 >/dev/null | grep class

# The compose knobs an operator can set.
grep -ohE '\$\{[A-Z_0-9]+(:-[^}]*)?\}' deploy/compose/docker-compose.yml | sort -u

# JavaScript, which no Rust registry can see.
grep -rEo 'process\.env\.[A-Z][A-Z_0-9]*' sdks/ examples/ \
  --exclude-dir=node_modules --exclude-dir=dist | sed 's/process\.env\.//' | sort -u
```

Run these under `bash`, not `zsh`: zsh does not word-split unquoted variables,
which silently collapses a multi-root grep into one bogus path and reports a
confident zero.

---

## Gates that are NOT armed, and why

Recorded here so their absence is a stated position rather than an oversight.

**Compose alias equality** (proposal Section 4.3). The rule is that a container
value whose whole scalar is one interpolation must satisfy `LEFT == RIGHT`.
Measured 2026-08-13 against `deploy/compose/docker-compose.yml`: eight pairs
violate it, listed in the "Database DSNs" table above. Arming the rule as
written would fail the build on the first run, so the deployment must be
rewritten to canonical `.env` names first. What IS armed is the other half of
that section - every explicit container variable on a platform service must be a
name that exact binary declares.

**Set-but-unread at process startup** (proposal Section 4.4). Nothing enumerates
the ambient `ZEROSHIP_*` names at `bootstrap` and rejects the ones the current
binary does not consume. It is not simply the contract set: `ZEROSHIP_LOG`,
`ZEROSHIP_NET_*`, `ZEROSHIP_STORAGE_*` and the `ZEROSHIP_COLUMN_KEY_*` family are
legitimately read by libraries inside the same process without being settings,
so a naive prefix rule would refuse to start a correct deployment. The allowed
set has to be the contract union the declared per-consumer reads, and that union
does not exist yet.
