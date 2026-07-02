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
secret material.

## Required Configuration

Flag names match `crates/auth/src/config.rs`; every flag has an equivalent env
var.

### Core Native OP

| Env var | Default | Required? | Notes |
|---|---|---|---|
| `AUTH_ADDR` | `127.0.0.1:9092` | no | Bind address. Keep loopback unless a reverse proxy or orchestrator needs a pod/network bind. |
| `AUTH_DB_URL` | unset | yes | DSN for the `zeroship_auth` role against the migrated platform database. |
| `AUTH_PUBLIC_URL` | `http://localhost:9092` | yes in prod | Public auth origin. The issuer is `${AUTH_PUBLIC_URL}/oauth2`. |
| `AUTH_SIGNING_KEY_FILE` | unset | yes in prod | Ed25519 private key, PEM/PKCS#8 or DER. Public JWK metadata is published to Postgres at boot. |
| `AUTH_PAIRWISE_SALT_FILE` | unset | yes in prod | Permanent pairwise-subject salt. Do not rotate without a migration. |
| `AUTH_BROKER_SECRET_FILE` | unset | yes in prod | Master secret used to derive per-client broker secrets. |
| `AUTH_BROKER_SECRET_PREVIOUS_FILE` | unset | rotation only | Previous broker secret during rolling rotation. |
| `REFRESH_HASH_KEY_FILE` | unset | yes in prod | HMAC keyring for refresh-token verifiers. |
| `REFRESH_IDEM_KEY_FILE` | unset | yes in prod | AEAD key for refresh idempotency cache rows. |
| `AUTH_STASH_SIGNING_KEY` | dev default | yes in prod | HMAC key for auth-origin stash cookies. Use at least 32 bytes. |
| `AUTH_TOTP_ENC_KEY` | dev default | yes in prod | TOTP secret encryption key. |
| `AUTH_REFRESH_POOL_SIZE` | `4` | no | Dedicated refresh-family DB session pool size per process. |
| `ZEROSHIP_CONFIG` | unset | optional | Shared TOML overlay path for non-secret auth config and secret references. |
| `ZEROSHIP_DEV_INSECURE` / `--dev-insecure` | unset | never in prod | Relaxes cookie/secret guards for local development only. |

### Mailer

| Env var | Default | Required? | Notes |
|---|---|---|---|
| `AUTH_MAILER` | `stdout` | yes in prod | Use `smtp` or `resend` for real users. |
| `AUTH_MAIL_FROM_EMAIL` | `auth@zeroship.ai` | yes | Sender address. |
| `AUTH_MAIL_FROM_NAME` | `zeroship` | yes | Sender display name. |
| `AUTH_SMTP_HOST` | unset | when SMTP | SMTP relay host. |
| `AUTH_SMTP_PORT` | `587` | no | 587 STARTTLS or 465 implicit TLS. |
| `AUTH_SMTP_USERNAME` | unset | optional | SMTP username. |
| `AUTH_SMTP_PASSWORD` | unset | paired | SMTP password. |
| `AUTH_SMTP_TLS` | `starttls` | no | `starttls`, `implicit`, or dev/test `plaintext`. |
| `AUTH_RESEND_API_KEY` | unset | when Resend | Resend API key. |
| `AUTH_POSTMARK_WEBHOOK_USER` | unset | when Postmark | Basic-auth user for `/webhooks/postmark`. |
| `AUTH_POSTMARK_WEBHOOK_PASSWORD` | unset | paired | Basic-auth password. |

The SES-SNS webhook verifies AWS-published SNS signatures and has no shared
secret variable.

### Federated Providers

Routes are registered only when provider client IDs are present.

| Env var | Required? | Notes |
|---|---|---|
| `AUTH_GOOGLE_CLIENT_ID` | optional | Enables `/oauth/google/*`. |
| `AUTH_GOOGLE_CLIENT_SECRET` | with Google ID | Google client secret. |
| `AUTH_GOOGLE_REDIRECT_URI` | with Google ID | Defaults to `https://auth.zeroship.ai/oauth/google/callback`. |
| `AUTH_GITHUB_CLIENT_ID` | optional | Enables `/oauth/github/*`. |
| `AUTH_GITHUB_CLIENT_SECRET` | with GitHub ID | GitHub client secret. |
| `AUTH_GITHUB_REDIRECT_URI` | with GitHub ID | Defaults to `https://auth.zeroship.ai/oauth/github/callback`. |

Provider URL override variables exist for e2e tests with mock providers.
Production should normally use the defaults.

## Secret Generation

Generate the OP signing key once per environment:

```bash
openssl genpkey -algorithm ed25519 -out auth-signing.pem
chmod 0600 auth-signing.pem
```

Generate the other file-backed secrets from at least 32 random bytes each and
store them in your secret manager:

```bash
openssl rand -base64 48 > auth-pairwise-salt
openssl rand -base64 48 > auth-broker-secret
openssl rand -base64 48 > refresh-hash-key
openssl rand -base64 48 > refresh-idem-key
chmod 0600 auth-pairwise-salt auth-broker-secret refresh-hash-key refresh-idem-key
```

`AUTH_STASH_SIGNING_KEY` and `AUTH_TOTP_ENC_KEY` may also come from files through
your process manager's secret injection, but the binary accepts them as env/CLI
values today.

## First Boot

1. **Run platform migrations** before any service starts:

   ```bash
   zeroship-migrate migrate \
     --dir ./db/migrations \
     --database-url "$PLATFORM_ADMIN_DATABASE_URL" \
     --profile platform \
     --yes
   ```

2. **Verify the auth role can connect**:

   ```bash
   psql "$AUTH_DB_URL" -c 'select 1'
   ```

3. **Start `zeroship-auth`** behind your reverse proxy:

   ```bash
   AUTH_DB_URL=postgres://zeroship_auth:...@db:5432/zeroship \
   AUTH_PUBLIC_URL=https://auth.zeroship.ai \
   AUTH_SIGNING_KEY_FILE=/run/secrets/auth-signing.pem \
   AUTH_PAIRWISE_SALT_FILE=/run/secrets/auth-pairwise-salt \
   AUTH_BROKER_SECRET_FILE=/run/secrets/auth-broker-secret \
   REFRESH_HASH_KEY_FILE=/run/secrets/refresh-hash-key \
   REFRESH_IDEM_KEY_FILE=/run/secrets/refresh-idem-key \
   AUTH_STASH_SIGNING_KEY="$AUTH_STASH_SIGNING_KEY" \
   AUTH_TOTP_ENC_KEY="$AUTH_TOTP_ENC_KEY" \
   AUTH_MAILER=smtp AUTH_SMTP_HOST=smtp.example.com \
   zeroship-auth --addr 0.0.0.0:9092
   ```

On boot, the service loads the signing key, publishes the matching public JWK
metadata, initializes the refresh-token key material, and serves discovery at
`${AUTH_PUBLIC_URL}/oauth2/.well-known/openid-configuration`.

## Healthchecks

- `GET /healthz` checks process liveness.
- `GET /readyz` checks service readiness.
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
| `/login` returns 500 | DB unavailable or auth role missing grants | Check `AUTH_DB_URL`, migrations, and database health. |
| `/oauth2/token` returns `invalid_client` | Client row missing or broker secret mismatch | Check `zeroship.oauth_clients`, app client provisioning, and broker secret rollout. |
| `/oauth2/token` returns `invalid_grant` | Code expired/consumed, PKCE mismatch, consent revoked, or refresh family revoked | Retry the auth flow; inspect audit logs for revocation or reuse detection. |
| JWKS is empty | Signing key failed to load or publish | Check `AUTH_SIGNING_KEY_FILE` permissions and boot logs. |
| New users cannot sign up | Mailer still set to `stdout` or provider credentials invalid | Set `AUTH_MAILER=smtp` or `resend`; verify provider logs. |
| `/webhooks/postmark` returns 401 | Basic-auth mismatch | Verify `AUTH_POSTMARK_WEBHOOK_USER` and `AUTH_POSTMARK_WEBHOOK_PASSWORD`. |
| Login p99 jumps | Argon2id CPU pressure or slow DB | Check CPU saturation, DB latency, and rate-limit table health. |
| Magic-link or verification email missing | Recipient suppressed after prior bounce/complaint | Review `zeroship.email_suppressions` and audit before deleting. |

Gateway-cached app sessions remain valid until their own expiry even if the auth
service is temporarily unavailable, but new login, refresh, revoke, and
introspection flows fail until auth recovers.

## Backups and Restore

Back up the platform database and all auth secret material together:

- PostgreSQL dump or snapshot of the `zeroship` schema.
- `AUTH_SIGNING_KEY_FILE`
- `AUTH_PAIRWISE_SALT_FILE`
- `AUTH_BROKER_SECRET_FILE` and any previous broker secret during rollout
- `REFRESH_HASH_KEY_FILE`
- `REFRESH_IDEM_KEY_FILE`
- `AUTH_STASH_SIGNING_KEY`
- `AUTH_TOTP_ENC_KEY`

Restore order:

1. Restore PostgreSQL.
2. Restore the same secret files and env secrets.
3. Start `zeroship-auth`.
4. Verify discovery, JWKS, `/readyz`, login, refresh, and logout.

Losing the signing key invalidates outstanding ID/access tokens. Losing refresh
key material invalidates refresh families. Losing the pairwise salt rekeys every
app-facing user subject and requires an explicit migration plan.

## Capacity

Password login throughput is CPU-bound by Argon2id verification. Scale auth
replicas horizontally for login bursts; sessions, rate limits, refresh families,
and consent state live in PostgreSQL.

Rough sizing guidance:

- Start with 4 vCPU / 8 GB RAM per auth replica.
- Keep `AUTH_REFRESH_POOL_SIZE` small unless refresh traffic is demonstrably
  waiting on the dedicated pool.
- Scale replicas before weakening Argon2id parameters.

## Operational Tasks

### Rotate Broker Secret

1. Write the new secret to `AUTH_BROKER_SECRET_FILE`.
2. Move the old secret to `AUTH_BROKER_SECRET_PREVIOUS_FILE`.
3. Roll auth replicas.
4. Roll gateway/control components that need to present derived broker secrets.
5. Remove `AUTH_BROKER_SECRET_PREVIOUS_FILE` after every dependent has rolled.

### Rotate OP Signing Key

The current implementation loads one active Ed25519 private key from
`AUTH_SIGNING_KEY_FILE` and publishes its public JWK at boot.

1. Generate the new key.
2. Roll auth with the new key.
3. Keep old app sessions and tokens within their configured TTL expectations.
4. Verify discovery and JWKS after rollout.

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
