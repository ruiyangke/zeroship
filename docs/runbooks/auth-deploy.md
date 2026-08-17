# Auth Deployment Runbook

Operator guide for deploying `zeroship-auth`, the native zeroship OIDC OP, in
production or any environment beyond the docker-compose dev stack.

For the architectural picture, see [Auth](../reference/auth.md).

## Topology

```text
End user -> gateway -> zeroship-auth (native OIDC OP + login UI, :9092)
                              |
                              v
                         Postgres (zeroship schema)
```

`zeroship-auth` is stateless apart from PostgreSQL and its configured secret
files. Run any number of replicas behind the auth host after the platform
migrations have completed. All replicas for one environment must use the same
issuer URL and the same signing, broker, pairwise, refresh, stash, and TOTP
secret material. They must also use the same platform mint key as the control
replicas that call the internal mint. Never deliver that key to worker,
gateway, migrated, or creator-app processes.

## Required Configuration

Flag names match `crates/auth/src/config.rs`; every flag has an equivalent env
var.

### Core Native OP

| Env var | Default | Required? | Notes |
|---|---|---|---|
| `ZEROSHIP_AUTH_ADDR` | `127.0.0.1:9092` | no | Bind address. Keep loopback unless a reverse proxy or orchestrator needs a pod/network bind. |
| `ZEROSHIP_AUTH_DATABASE_URL` | unset | yes | DSN for the `zeroship_auth` role against the migrated platform database. |
| `ZEROSHIP_AUTH_PUBLIC_URL` | `http://localhost:9092` | yes in prod | Public auth origin. The issuer is `${ZEROSHIP_AUTH_PUBLIC_URL}/oauth2`. |
| `ZEROSHIP_AUTH_SIGNING_KEY_FILE` | unset | yes in prod | Ed25519 private key, PEM/PKCS#8 or DER. Public JWK metadata is published to Postgres at boot. |
| `ZEROSHIP_AUTH_PAIRWISE_SALT_FILE` | unset | yes in prod | Permanent pairwise-subject salt. Do not rotate without a migration. |
| `ZEROSHIP_AUTH_BROKER_SECRET_FILE` | unset | yes in prod | Master secret used to derive per-client broker secrets. |
| `ZEROSHIP_AUTH_BROKER_SECRET_PREVIOUS_FILE` | unset | rotation only | Previous broker secret during rolling rotation. |
| `ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE` | unset | yes in prod | HMAC keyring for refresh-token verifiers. |
| `ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE` | unset | yes in prod | AEAD key for refresh idempotency cache rows. |
| `ZEROSHIP_AUTH_STASH_SIGNING_KEY` | unset | yes | HMAC key for auth-origin stash cookies. Use at least 32 bytes. |
| `ZEROSHIP_AUTH_TOTP_ENC_KEY` | unset | yes | TOTP secret encryption key. |
| `ZEROSHIP_AUTH_REFRESH_POOL_SIZE` | `4` | no | Dedicated refresh-family DB session pool size per process. |
| `ZEROSHIP_CONFIG` | unset | optional | Shared TOML overlay path for non-secret auth config and secret references. |

### Mailer

| Env var | Default | Required? | Notes |
|---|---|---|---|
| `ZEROSHIP_AUTH_MAILER` | `stdout` | yes in prod | Use `smtp` or `resend` for real users. |
| `ZEROSHIP_AUTH_MAIL_FROM_EMAIL` | `auth@zeroship.ai` | yes | Sender address. |
| `ZEROSHIP_AUTH_MAIL_FROM_NAME` | `zeroship` | yes | Sender display name. |
| `ZEROSHIP_AUTH_SMTP_HOST` | unset | when SMTP | SMTP relay host. |
| `ZEROSHIP_AUTH_SMTP_PORT` | `587` | no | 587 STARTTLS or 465 implicit TLS. |
| `ZEROSHIP_AUTH_SMTP_USERNAME` | unset | optional | SMTP username. |
| `ZEROSHIP_AUTH_SMTP_PASSWORD` | unset | paired | SMTP password. |
| `ZEROSHIP_AUTH_SMTP_TLS` | `starttls` | no | `starttls`, `implicit`, or dev/test `plaintext`. |
| `ZEROSHIP_AUTH_RESEND_API_KEY` | unset | when Resend | Resend API key. |
| `ZEROSHIP_AUTH_POSTMARK_WEBHOOK_USER` | unset | when Postmark | Basic-auth user for `/webhooks/postmark`. |
| `ZEROSHIP_AUTH_POSTMARK_WEBHOOK_PASSWORD` | unset | paired | Basic-auth password. |

The SES-SNS webhook verifies AWS-published SNS signatures and has no shared
secret variable.

### Federated Providers

Routes are registered only when provider client IDs are present.

| Env var | Required? | Notes |
|---|---|---|
| `ZEROSHIP_AUTH_GOOGLE_CLIENT_ID` | optional | Enables `/oauth/google/*`. |
| `ZEROSHIP_AUTH_GOOGLE_CLIENT_SECRET` | with Google ID | Google client secret. |
| `ZEROSHIP_AUTH_GOOGLE_REDIRECT_URI` | with Google ID | Defaults to `https://auth.zeroship.ai/oauth/google/callback`. |
| `ZEROSHIP_AUTH_GITHUB_CLIENT_ID` | optional | Enables `/oauth/github/*`. |
| `ZEROSHIP_AUTH_GITHUB_CLIENT_SECRET` | with GitHub ID | GitHub client secret. |
| `ZEROSHIP_AUTH_GITHUB_REDIRECT_URI` | with GitHub ID | Defaults to `https://auth.zeroship.ai/oauth/github/callback`. |

Provider URL override variables exist for e2e tests with mock providers.
Production should normally use the defaults.

## Secret Generation

For the local compose stack, provision the complete platform secret set from the
repository root:

```bash
zeroship dev init
```

The defaults are the gitignored `deploy/compose/secrets` directory and its
sibling `deploy/compose/.env`. Override both paths when provisioning another
layout:

```bash
zeroship dev init \
  --secrets-dir=/path/to/secrets \
  --env-file=/path/to/.env
```

The command creates exactly these eight files, with a mode of 0600 on Unix (and
0700 on the directory):

`gateway-signing.pem` `auth-signing.pem` `broker-secret`
`pairwise-salt` `refresh-hash-key` `refresh-idem-key`

It also adds eight 32-byte random hex values to the env overlay:

`ZEROSHIP_CONTROL_KEY` `ZEROSHIP_CONTROL_MASTER_KEY` `ZEROSHIP_WORKER_KEY`
`ZEROSHIP_MIGRATED_POLICY_SEAL_KEY` `ZEROSHIP_GATEWAY_STASH_SIGNING_KEY`
`ZEROSHIP_PAIRWISE_SALT` `ZEROSHIP_AUTH_STASH_SIGNING_KEY`
`ZEROSHIP_AUTH_TOTP_ENC_KEY`

The generated overlay is convenient for local compose, but production must
not mount or export it wholesale.

Generation is idempotent. A rerun validates and keeps every existing value,
creates only missing entries, and refuses to replace invalid or mismatched
material. It does not rotate secrets implicitly.

For manual provisioning, use the same formats:

```bash
S=/path/to/secrets

umask 077
mkdir -p "$S"
chmod 0700 "$S"
openssl genpkey -algorithm ed25519 -out "$S/auth-signing.pem"
openssl genpkey -algorithm ed25519 -out "$S/gateway-signing.pem"
openssl rand -base64 48 > "$S/broker-secret"
printf '1:%s\n' "$(openssl rand -hex 48)" > "$S/refresh-hash-key"
openssl rand -base64 48 > "$S/refresh-idem-key"
ZEROSHIP_PAIRWISE_SALT="$(openssl rand -hex 32)"
printf '%s' "$ZEROSHIP_PAIRWISE_SALT" > "$S/pairwise-salt"
chmod 0600 "$S"/*
```

`refresh-hash-key` is a keyring, not an unadorned random string. Each nonempty
line is `version:hex-or-base64url-key`; the recipe starts version 1 with 48
random bytes. The `pairwise-salt` file is different: its bytes must exactly
equal the `ZEROSHIP_PAIRWISE_SALT` env value used by control and gateway.
`printf '%s'` is load-bearing because a trailing newline would change auth's
derived `pws_`.

`broker-secret` is one physical file read without normalization by both auth and
gateway. Point `ZEROSHIP_AUTH_BROKER_SECRET_FILE` and `ZEROSHIP_GATEWAY_BROKER_SECRET_FILE` at that
same file rather than generating one per service.

`ZEROSHIP_AUTH_STASH_SIGNING_KEY` and `ZEROSHIP_AUTH_TOTP_ENC_KEY` may also come from files through
your process manager's secret injection, but the binary accepts them as env/CLI
values today. Use at least 32 random bytes for each; the TOTP value must be hex
or base64url encoded.

## First Boot

1. **Run platform migrations** before any service starts:

   ```bash
   cargo build --release -p zeroship-migrate-adapter --features platform-cli --bin zeroship-platform-migrate
   ./target/release/zeroship-platform-migrate \
     --database-url "$PLATFORM_ADMIN_DATABASE_URL" \
     --migrations-dir ./db/migrations-ts \
     --project-schema zeroship \
     --project-id zeroship
   ```

2. **Verify the auth role can connect**:

   ```bash
   psql "$ZEROSHIP_AUTH_DATABASE_URL" -c 'select 1'
   ```

3. **Start `zeroship-auth`** behind your reverse proxy:

   ```bash
   ZEROSHIP_AUTH_DATABASE_URL=postgres://zeroship_auth:...@db:5432/zeroship \
   ZEROSHIP_AUTH_PUBLIC_URL=https://auth.zeroship.ai \
   ZEROSHIP_AUTH_SIGNING_KEY_FILE=/run/secrets/auth-signing.pem \
   ZEROSHIP_AUTH_PAIRWISE_SALT_FILE=/run/secrets/pairwise-salt \
   ZEROSHIP_AUTH_BROKER_SECRET_FILE=/run/secrets/broker-secret \
   ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE=/run/secrets/refresh-hash-key \
   ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE=/run/secrets/refresh-idem-key \
   ZEROSHIP_AUTH_STASH_SIGNING_KEY="$ZEROSHIP_AUTH_STASH_SIGNING_KEY" \
   ZEROSHIP_AUTH_TOTP_ENC_KEY="$ZEROSHIP_AUTH_TOTP_ENC_KEY" \
   ZEROSHIP_AUTH_MAILER=smtp ZEROSHIP_AUTH_SMTP_HOST=smtp.example.com \
   zeroship-auth --addr 0.0.0.0:9092
   ```

No secret is shared between auth and control any more: the dedicated mint
bearer went with the endpoint it authorized.

On boot, the service loads the signing key, publishes the matching public JWK
metadata, initializes the refresh-token key material, and serves discovery at
`${ZEROSHIP_AUTH_PUBLIC_URL}/oauth2/.well-known/openid-configuration`.

## Healthchecks

- `GET /healthz` checks process liveness: a constant 200 that touches no
  dependency, so a Postgres blip does not get the container restarted.
- `GET /readyz` checks readiness: 200 only when Postgres answers (no login,
  token or consent route works without it), 503 otherwise. The probe is
  bounded and its result cached for ~2s, so probe traffic cannot become
  database traffic; the body is `{"ready":true|false}` and never names the
  DSN or the driver error. Point your orchestrator's readiness gate here and
  its liveness gate at `/healthz`.
- `GET /oauth2/.well-known/openid-configuration` should return the issuer.
- `GET /oauth2/.well-known/jwks.json` should return at least one key.

Example:

```bash
curl -sf https://auth.zeroship.ai/healthz
curl -sf https://auth.zeroship.ai/readyz
curl -sf https://auth.zeroship.ai/oauth2/.well-known/openid-configuration | jq .issuer
curl -sf https://auth.zeroship.ai/oauth2/.well-known/jwks.json | jq '.keys | length'
```

## Logs

`zeroship-auth` emits structured logs to stdout when configured with the JSON log
format. Notable streams:

- `auth.audit` tracing target for security/audit events.
- Mailer bounce and complaint events from Postmark or SES-SNS webhooks.
- OP token, refresh, revoke, device, consent, and logout events from the auth
  service itself.

Persisted audit events live in `zeroship.audit_events`.

## Failure Modes

| Symptom | Likely cause | Action |
|---|---|---|
| `/login` returns 500 | DB unavailable or auth role missing grants | Check `ZEROSHIP_AUTH_DATABASE_URL`, migrations, and database health. |
| `/oauth2/token` returns `invalid_client` | Client row missing or broker secret mismatch | Check `zeroship.oauth_clients`, app client provisioning, and broker secret rollout. |
| `/oauth2/token` returns `invalid_grant` | Code expired/consumed, PKCE mismatch, consent revoked, or refresh family revoked | Retry the auth flow; inspect audit logs for revocation or reuse detection. |
| JWKS is empty | Signing key failed to load or publish | Check `ZEROSHIP_AUTH_SIGNING_KEY_FILE` permissions and boot logs. |
| New users cannot sign up | Mailer still set to `stdout` or provider credentials invalid | Set `ZEROSHIP_AUTH_MAILER=smtp` or `resend`; verify provider logs. |
| `/webhooks/postmark` returns 401 | Basic-auth mismatch | Verify `ZEROSHIP_AUTH_POSTMARK_WEBHOOK_USER` and `ZEROSHIP_AUTH_POSTMARK_WEBHOOK_PASSWORD`. |
| Login p99 jumps | Argon2id CPU pressure or slow DB | Check CPU saturation, DB latency, and rate-limit table health. |
| Magic-link or verification email missing | Recipient suppressed after prior bounce/complaint | Review `zeroship.email_suppressions` and audit before deleting. |

Gateway-cached app sessions remain valid until their own expiry even if the auth
service is temporarily unavailable, but new login, refresh, revoke, and
introspection flows fail until auth recovers.

## Backups and Restore

Back up the platform database and all auth secret material together:

- PostgreSQL dump or snapshot of the `zeroship` schema.
- `ZEROSHIP_AUTH_SIGNING_KEY_FILE`
- `ZEROSHIP_AUTH_PAIRWISE_SALT_FILE`
- `ZEROSHIP_AUTH_BROKER_SECRET_FILE` and any previous broker secret during rollout
- `ZEROSHIP_AUTH_REFRESH_HASH_KEY_FILE`
- `ZEROSHIP_AUTH_REFRESH_IDEM_KEY_FILE`
- `ZEROSHIP_AUTH_STASH_SIGNING_KEY`
- `ZEROSHIP_AUTH_TOTP_ENC_KEY`

Restore order:

1. Restore PostgreSQL.
2. Restore the same secret files and env secrets, including the platform mint
   key on auth and control only.
3. Start `zeroship-auth` and control with the same restored mint key.
4. Verify discovery, JWKS, `/readyz`, login, refresh, logout, and the CLI
   device-token exchange.

Losing the signing key invalidates outstanding ID/access tokens. Losing refresh
key material invalidates refresh families. Losing the pairwise salt rekeys every
app-facing user subject and requires an explicit migration plan.

## Capacity

Password login throughput is CPU-bound by Argon2id verification. Scale auth
replicas horizontally for login bursts; sessions, rate limits, refresh families,
and consent state live in PostgreSQL.

Rough sizing guidance:

- Start with 4 vCPU / 8 GB RAM per auth replica.
- Keep `ZEROSHIP_AUTH_REFRESH_POOL_SIZE` small unless refresh traffic is demonstrably
  waiting on the dedicated pool.
- Scale replicas before weakening Argon2id parameters.

## Operational Tasks

### Rotate Broker Secret

1. Write the new secret to `ZEROSHIP_AUTH_BROKER_SECRET_FILE`.
2. Move the old secret to `ZEROSHIP_AUTH_BROKER_SECRET_PREVIOUS_FILE`.
3. Roll auth replicas.
4. Roll gateway/control components that need to present derived broker secrets.
5. Remove `ZEROSHIP_AUTH_BROKER_SECRET_PREVIOUS_FILE` after every dependent has rolled.

### Rotate OP Signing Key

The current implementation loads one active Ed25519 private key from
`ZEROSHIP_AUTH_SIGNING_KEY_FILE` and publishes its public JWK at boot.

1. Generate the new key.
2. Roll auth with the new key.
3. Verify discovery and JWKS show the new `active` key and the old `retiring`
   key. Existing old-key processes may finish issuing while they drain; each
   issuance advances the old key's persisted maximum expiry before returning.
4. Drain every process holding the old private key. A restarted old-key
   process is refused once its row is `retiring`.
5. Let the hourly signing-key retention cron move the old row to terminal
   `retired`. It waits through the exact maximum issued expiry plus 5 minutes
   of JWKS freshness, 5 minutes of stale-while-revalidate, and 2 minutes of
   clock skew. If no expiry was recorded, the conservative total from
   `retiring_at` is 43,920 seconds (12 hours 12 minutes).
6. Verify the retired key is absent from JWKS. Keep the `retired` database row
   as the rotation audit record.

### Remove a Federated Provider

Unset the provider client ID and secret, then restart auth. The route is not
registered when the client ID is absent. Existing linked identities remain in
Postgres and can be used again if the provider is re-enabled.

### Suppression List Maintenance

Only remove a suppression after confirming the recipient can accept mail again:

```sql
DELETE FROM zeroship.email_suppressions WHERE email = 'user@example.com';
```

Record the operator action in the incident/audit trail.

## Reference

- [Auth](../reference/auth.md)
- [Auth dev tier](../reference/auth-dev-tier.md)
- [Docker Compose runbook](docker-compose.md)
- [Database migrations](db-migrations.md)
- [Historical auth-server design](../archive/auth-server.md)
