# Auth deployment runbook

Operator guide for deploying `crates/auth` + `oryd/hydra` in production
(or any environment beyond the docker-compose dev stack).

For the architectural picture (sequence diagrams, cookies, SDK surface)
see [`docs/reference/auth.md`](../reference/auth.md). For the design
rationale see [`docs/proposals/auth-server.md`](../proposals/auth-server.md).

## 1 · Topology

```
End user → gateway → ┬── hydra        (OIDC kernel,        public :4444 / admin :4445)
                     └── crates/auth  (login UI + flows,            public :9092)
                          │
                          ▼
                       Postgres (shared: hydra_* schema + auth.* schema)
```

Each "auth pod" runs two processes:

- `oryd/hydra` v25.4.x — OIDC/OAuth 2.1 protocol kernel. Issues ID
  tokens, access tokens, and refresh tokens. Owns its own `hydra_*`
  schema in Postgres.
- `crates/auth` — login UI (`/login`, `/signup`, `/consent`), identity
  flows (password, Google, GitHub, magic-link), email verification +
  password reset, mailer driver, audit log, hydra-admin client. Owns
  the `auth.*` schema in the same Postgres database.

`crates/auth` reaches hydra over loopback (`http://127.0.0.1:4445`)
inside the pod. The hydra admin port MUST NOT be exposed externally —
it is a privileged management API.

## 2 · Required environment variables

Flag names match `crates/auth/src/config.rs` (Clap `#[arg(long, env = …)]`).
Every flag has an equivalent env var.

### hydra (sidecar)

| Env var | Required? | Notes |
|---|---|---|
| `DSN` | yes | Postgres URL, e.g. `postgres://user:pw@host:5432/zeroship?sslmode=verify-full`. |
| `SECRETS_SYSTEM` | yes | ≥16 chars. Encrypts JWK material at rest. **Identical across all hydra replicas in a deployment.** Rotate by prepending; never replace. |
| `SECRETS_COOKIE` | yes | ≥16 chars. Signs hydra session cookies. Same rotation rule as `SECRETS_SYSTEM`. |
| `TRACING_PROVIDERS_OTLP_SERVER_URL` | optional | OTLP collector URL if you ingest hydra spans. |

Config file: `ops/hydra.yaml` (mounted at `/etc/config/hydra/hydra.yaml`).
Issuer URL, TTLs, cookie domain, and the EdDSA/JWT strategy live there.

### crates/auth — core + DB + bootstrap

| Env var | Default | Required? | What it controls |
|---|---|---|---|
| `AUTH_ADDR` | `0.0.0.0:9092` | no | Bind address. |
| `AUTH_PUBLIC_URL` | `http://localhost:9092` | **yes in prod** | Externally-reachable origin (scheme + host + optional port). Used to construct absolute URLs in outbound email (magic-link, verify, reset). Distinct from `AUTH_ADDR`. |
| `AUTH_DB_URL` | — | yes | Postgres DSN. Same database as hydra. |
| `AUTH_HYDRA_ADMIN` | `http://127.0.0.1:4445` | yes | Hydra admin API base URL. Loopback in prod. |
| `AUTH_HYDRA_PUBLIC` | `https://auth.zeroship.ai` | yes | Hydra public base URL (issuer). |
| `AUTH_CLIENTS_CONFIG` | `/etc/zeroship/auth-clients.toml` | yes | Path to the declarative OIDC client registry. |
| `AUTH_BOOTSTRAP` | unset | first boot only | Boolean — set to `true` on first boot to allow JWK + client creation. Drop on subsequent restarts. Without it, an empty `hydra_jwk` set is a fatal startup error. |
| `AUTH_INSECURE_DEV` | unset | dev only | Drops the `Secure` flag on cookies. **Never set in production.** |
| `AUTH_STASH_SIGNING_KEY` | dev default | **yes in prod** | HMAC key (≥32 bytes) signing the federation stash cookies. The dev default is loud-warned at boot; a weak value lets an attacker forge stash cookies and bypass OAuth state/PKCE checks. |

### crates/auth — mailer

| Env var | Default | Required? | What it controls |
|---|---|---|---|
| `AUTH_MAILER` | `stdout` | **yes in prod** | Driver: `stdout` (dev) \| `smtp` \| `resend`. `stdout` swallows transactional mail; setting `smtp` or `resend` is mandatory for any deployment with real users. |
| `AUTH_MAIL_FROM_EMAIL` | `auth@zeroship.ai` | yes | `From` address. |
| `AUTH_MAIL_FROM_NAME` | `zeroship` | yes | `From` display name. |
| `AUTH_SMTP_HOST` | — | when `AUTH_MAILER=smtp` | SMTP relay hostname. |
| `AUTH_SMTP_PORT` | `587` | no | 587 (STARTTLS) or 465 (implicit TLS). |
| `AUTH_SMTP_USERNAME` | — | optional | SMTP username (if relay requires auth). |
| `AUTH_SMTP_PASSWORD` | — | optional | Paired with `AUTH_SMTP_USERNAME`. |
| `AUTH_SMTP_STARTTLS` | `true` | no | `true` = STARTTLS on 587, `false` = implicit SMTPS on 465. |
| `AUTH_RESEND_API_KEY` | — | when `AUTH_MAILER=resend` | Resend HTTP API key. |
| `AUTH_POSTMARK_WEBHOOK_USER` | unset | when using Postmark | HTTP Basic-auth user Postmark presents on `/webhooks/postmark`. Unset = handler returns 401. |
| `AUTH_POSTMARK_WEBHOOK_PASSWORD` | unset | paired | Paired with the above. |

The SES-SNS webhook (`/webhooks/ses-sns`) verifies SNS RSA-SHA1 signatures
against AWS-published certs — no env vars to configure.

### crates/auth — OAuth providers (optional; routes registered only when set)

| Env var | Required? | Notes |
|---|---|---|
| `AUTH_GOOGLE_CLIENT_ID` | optional | Set to enable `/oauth/google/*`. |
| `AUTH_GOOGLE_CLIENT_SECRET` | with id | — |
| `AUTH_GOOGLE_REDIRECT_URI` | with id | Default `https://auth.zeroship.ai/oauth/google/callback`. Must match what's configured in Google Cloud Console. |
| `AUTH_GITHUB_CLIENT_ID` | optional | Set to enable `/oauth/github/*`. |
| `AUTH_GITHUB_CLIENT_SECRET` | with id | — |
| `AUTH_GITHUB_REDIRECT_URI` | with id | Default `https://auth.zeroship.ai/oauth/github/callback`. |

The `AUTH_GOOGLE_AUTH_URL` / `AUTH_GOOGLE_TOKEN_URL` / `AUTH_GOOGLE_JWKS_URL` /
`AUTH_GOOGLE_ISSUER` and equivalent `AUTH_GITHUB_*_URL` variables exist for
e2e tests pointing at a mock provider. Production deployments leave them
at their defaults.

### crates/auth — cron timing

| Env var | Default | Notes |
|---|---|---|
| `AUTH_JWK_ROTATION_DAYS` | `90` | Days between JWK rotations. |
| `AUTH_JWK_RETAIN_DAYS` | `31` | Days to keep the outgoing key after rotation. `90 + 31` > refresh-token max lifetime (720 h). |
| `AUTH_CRON_TICK_SECS` | `86400` | JWK-rotation cron tick (24 h). |
| `AUTH_AUDIT_RETENTION_CHECK_SECS` | `3600` | Audit-retention sweeper tick (1 h). |

## 3 · First-boot sequence

1. **Postgres up + reachable.** Both processes share one database; verify
   `psql "$AUTH_DB_URL" -c 'select 1'` succeeds from inside the pod.
2. **Migrate hydra schema** — one-shot job, `oryd/hydra:v25.4.x`:

   ```bash
   docker run --rm \
     -e DSN="$AUTH_DB_URL" \
     oryd/hydra:v25.4.0 migrate sql up -e --yes
   ```

3. **Start hydra** with `ops/hydra.yaml` mounted at
   `/etc/config/hydra/hydra.yaml` and `SECRETS_SYSTEM` / `SECRETS_COOKIE`
   injected:

   ```bash
   docker run -d --name hydra \
     -e DSN="$AUTH_DB_URL" \
     -e SECRETS_SYSTEM="$SECRETS_SYSTEM" \
     -e SECRETS_COOKIE="$SECRETS_COOKIE" \
     -v ./ops/hydra.yaml:/etc/config/hydra/hydra.yaml:ro \
     -p 127.0.0.1:4444:4444 -p 127.0.0.1:4445:4445 \
     oryd/hydra:v25.4.0 serve all --config /etc/config/hydra/hydra.yaml
   ```

   Generate `SECRETS_SYSTEM` + `SECRETS_COOKIE` (each ≥16 chars) once
   and store in your secret manager. See §7 for rotation.

4. **Bootstrap `crates/auth`** with `AUTH_BOOTSTRAP=true`:

   ```bash
   AUTH_BOOTSTRAP=true \
   AUTH_DB_URL=… AUTH_HYDRA_ADMIN=http://127.0.0.1:4445 \
   AUTH_HYDRA_PUBLIC=https://auth.zeroship.ai \
   AUTH_PUBLIC_URL=https://auth.zeroship.ai \
   AUTH_STASH_SIGNING_KEY=… AUTH_CLIENTS_CONFIG=/etc/zeroship/auth-clients.toml \
   AUTH_MAILER=smtp AUTH_SMTP_HOST=… \
   ./zeroship-auth
   ```

   `--bootstrap` (or `AUTH_BOOTSTRAP=true`) authorises three first-time
   side effects:
   - generate EdDSA + RS256 keys in `hydra.openid.id-token` if the key set is empty;
   - generate EdDSA in `hydra.jwt.access-token` if empty;
   - reconcile OIDC clients from `auth-clients.toml` against hydra's admin API (upsert; never deletes).

5. **Verify**:

   ```bash
   curl -sf http://<auth-host>:9092/healthz
   curl -sf http://<auth-host>:9092/readyz
   curl -sf http://<hydra-public>:4444/.well-known/openid-configuration | jq .issuer
   curl -sf http://<hydra-admin>:4445/admin/keys/hydra.openid.id-token | jq '.keys | length'
   ```

6. **Drop `AUTH_BOOTSTRAP` for subsequent restarts.** The bootstrap pass
   is idempotent but the flag gates accidental key regeneration on a
   clean DB clone.

## 4 · Healthchecks

`crates/auth`:

- `GET /healthz` — process up. k8s liveness probe (10s interval, 3 failures = restart).
- `GET /readyz` — process ready. k8s readiness probe (5s interval, 2 failures = mark unready).

hydra:

- `GET :4445/health/alive` — process up (admin port).
- `GET :4445/health/ready` — DB reachable.

The hydra admin port is not exposed externally, so probe it from a
sidecar or shared-network neighbor (k8s readiness probes do this on the
pod-local interface).

## 5 · Logs

Both processes emit structured JSON to stdout — pipe to your log
aggregator. Notable targets:

- `crates/auth` audit events use the `auth.audit` tracing target. Payloads
  are structured JSON objects (event kind + actor + subject + outcome).
  Ingest into your SIEM by filtering on `target == "auth.audit"`. The
  same events are persisted in `auth.audit_events` for retention.
- hydra emits OIDC-protocol-level events (auth, token, revoke) with
  `log.format: json` per `ops/hydra.yaml`.
- Bounce/complaint events from `POST /webhooks/postmark` and
  `POST /webhooks/ses-sns` land as `mailer_bounce` / `mailer_complaint`
  audit events AND as stdout JSON. The suppression list is `auth.email_suppressions`.

## 6 · Failure modes + responses

| Symptom | Likely cause | Action |
|---|---|---|
| `/login` returns 500 | DB unreachable | Restart auth; check PG load + `auth.audit_events` row count for truncation. |
| hydra 500 on `/oauth2/token` | hydra DB unreachable OR JWK set empty | Inspect hydra logs; `GET /admin/keys/hydra.openid.id-token` — non-empty? |
| New users can't sign up | `AUTH_MAILER=stdout` in production | Set `AUTH_MAILER=smtp` or `resend` + provider creds. |
| `/login` p99 jumps | Argon2 contention OR PG slow | Check CPU pressure; inspect `auth.rate_limits` row count; investigate PG indices. |
| Magic-link / verify emails not arriving | Recipient on `auth.email_suppressions` (prior bounce) | `DELETE` the row if the recipient resolved the issue; audit the action. |
| `/webhooks/postmark` 401s | Postmark Basic-auth creds wrong | Verify `AUTH_POSTMARK_WEBHOOK_USER` + `_PASSWORD` match the dashboard. |
| `AUTH_STASH_SIGNING_KEY is using the dev default` in logs | dev value in production | Set a strong (≥32 bytes) value and restart. |
| JWK rotation cron noisy in logs | Daily tick on schedule | Expected — `cron tasks spawned` plus occasional `key rotation: prepending`. |
| hydra unreachable for >5 s | hydra down OR hydra DB down | All logins go dark. Auth returns 503; gateway-cached app sessions keep working until access tokens expire (1 h). Mitigate with hydra HA + hot-standby PG. |

## 7 · Backups + restore

- `pg_dump` covers BOTH the `hydra_*` and `auth.*` schemas — one dump.
- **Critical:** back up `SECRETS_SYSTEM` and `SECRETS_COOKIE` separately
  (NOT in the same blob as the DB dump). Hydra encrypts JWK material at
  rest with `SECRETS_SYSTEM`; without it the `hydra_jwk` rows are
  unrecoverable and you must regenerate keys (which invalidates every
  outstanding access token, refresh token, and ID token).
- Restore order: PG first; start hydra with the same `SECRETS_SYSTEM`
  and `SECRETS_COOKIE`; start `crates/auth` (no `AUTH_BOOTSTRAP` needed
  unless the keys were lost in the restore).
- Test the restore quarterly. A backup you have never restored is not a
  backup.

## 8 · Capacity

Measured baseline (`AUTH_LOAD_TEST=1 cargo test -p zeroship-auth --test load_test --release`,
32-core dev workstation, OWASP-2026 Argon2id params):

- **~28 RPS** sustained `/login` POST (Argon2id verify dominates; CPU-pegged).
- **p99 ~1.75 s** at N=50 parallel logins.
- Throughput scales linearly with available cores — Argon2id is CPU-bound.

Per-pod rough sizing:

- **4 vCPU + 8 GB RAM** — handles ~100 logins/min sustained, bursts of ~500/min.
- Beyond ~1000 logins/min sustained: scale `crates/auth` horizontally
  (stateless — sessions live in PG, rate-limit buckets in PG). Hydra
  scales the same way against the same Postgres.

`crates/auth` replicas: any number. Bootstrap is idempotent.
Hydra replicas: any number, but launch with `replicas=1` until the JWK
set is populated, then scale.

If CPU is the binding cost and Argon2id verification dominates, the
OWASP-2026 "second-recommended" Argon2id parameter set (m = 12 MiB,
t = 3, p = 1) is the floor — see `docs/proposals/auth-server.md` §8.

## 9 · Operational tasks

### Rotating client secrets

OIDC clients live in `ops/auth-clients.toml`; the reconciliation pass at
each boot upserts the file into hydra. To rotate: update `client_secret`,
distribute to the RP, restart `crates/auth`. For zero-downtime, add a
parallel client via the admin API first, migrate the RP, then retire the
old client.

### Removing an OAuth provider

Unset the provider's client-id env var (e.g. `AUTH_GOOGLE_CLIENT_ID=`),
restart `crates/auth`. Federation routes deregister at boot. Existing
`auth.identities` rows stay — users can re-link via `/me` once re-enabled,
or fall back to password recovery.

### Suppression list maintenance

The `auth.email_suppressions` table holds bounced/complained addresses;
the mailer silently no-ops sends to suppressed recipients (audit event
emitted). To re-enable:

```sql
DELETE FROM auth.email_suppressions WHERE email = 'user@example.com';
```

Audit the deletion — undoing a suppression is a trust decision.

### Adding a new OIDC client

Edit `ops/auth-clients.toml`, restart `crates/auth`. The reconciler
creates the client at boot. Ad-hoc `POST /admin/clients` on hydra's
admin port also works but is lost on the next reconcile unless mirrored
in the TOML.

### JWK rotation cadence

- Default: 90-day prepend, 31-day retention (`AUTH_JWK_ROTATION_DAYS` / `AUTH_JWK_RETAIN_DAYS`).
- Manual: `POST /admin/keys/hydra.openid.id-token { "alg": "EdDSA" }` (or `RS256`) on the hydra admin port — hydra prepends and signs with the head.
- Verify: `curl -sf http://<hydra-admin>:4445/admin/keys/hydra.openid.id-token | jq '.keys | length'` — after retention sweeps, expect 2 for id-token (EdDSA + RS256), 1 for access-token (EdDSA).

## 10 · Reference

- Design proposal — `docs/proposals/auth-server.md` (§16 covers operational concerns end-to-end).
- Architecture summary — `docs/reference/auth.md`.
- Phase plans — `docs/superpowers/plans/2026-05-{26,27}-auth-server-phase-{1..6}.md`.
- Hydra config — `ops/hydra.yaml`.
- Example client registry — `ops/auth-clients.example.toml`.
- Local dev stack — `docs/runbooks/docker-compose.md` (mounts the same `ops/hydra.yaml`).
