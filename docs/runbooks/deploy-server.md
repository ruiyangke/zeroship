# Server deploy runbook (image-based)

How to run the platform on a remote host **without putting the source tree on
it**. The host gets a built image plus about 40 KB of config; nothing else.

This is the counterpart to `docker-compose.md`, which covers the local dev
stack. Same compose file, different inputs: locally you `build`, on a server you
`pull`.

Walked end to end on 2026-08-11 against a Hostinger VPS (Ubuntu 24.04, 8 vCPU,
31 GB RAM) fronted by Cloudflare. Every command below was run; the traps in
[Things that will bite you](#things-that-will-bite-you) were all hit for real.

## Why not build on the server

Building on the target host means the source tree lives there, and the build
needs a full Rust + Node toolchain, roughly 18 GB of layer cache, and more RAM
than a small VPS wants to give. It also makes "what is deployed" unanswerable:
a working tree drifts, a git checkout does not pin what actually compiled.

Build once, somewhere you control, and ship the artifact. The image is the
deploy unit and the commit it was built from is the version.

## Topology

```
Cloudflare (terminates TLS)
  -> host:80  Caddy            deploy/ops/Caddyfile, host-based routing
       -> gateway:8000         creator apps, {app}.<domain>
       -> auth:9092            OIDC provider, auth.<domain>
       # control:9090          STAGED, disabled until security prerequisites
  host:9090 -> control:9090    LOOPBACK override, SSH fallback only
```

Every base host publication except Caddy's `:80` binds to loopback. Control has
no host publication in the base file. Caddy reaches gateway and auth by compose
service name over the internal network, so publishing their ports would only
add a second entrance that bypasses the edge (and therefore bypasses TLS).
`tests/compose_port_exposure_gate.sh` enforces this.

## One-time: build and push

Requires Docker, and Rust/Node are NOT needed on the host doing this - it all
happens inside the build stages.

```bash
cd <repo root>
SHA=$(git rev-parse --short HEAD)
REG=ghcr.io/<owner>/zeroship-platform

# --target runtime is deliberate. The default (last) stage is `frontend`,
# which adds Node + git for the AI builder's vite dev server. The five
# platform services do not need either.
docker build --target runtime -t "$REG:$SHA" -t "$REG:latest" -f deploy/Dockerfile .
```

Verify before pushing. An image that builds is not an image that is correct:

```bash
docker run --rm --entrypoint sh "$REG:$SHA" -c 'ls /usr/local/bin; ls /db/migrations-ts | wc -l'
# expect 6 binaries: zeroship zeroship-auth zeroship-control zeroship-gate
#                    zeroship-platform-migrate zeroship-worker
# expect 16 (or however many db/migrations-ts/*.ts you have)
docker run --rm --entrypoint zeroship "$REG:$SHA" --version
```

Push. GHCR needs a PAT with `write:packages` (a `gh auth token` will usually
NOT have that scope):

```bash
printf '%s' "$GHCR_PAT" | docker login ghcr.io -u <owner> --password-stdin
docker push "$REG:$SHA"
docker push "$REG:latest"
```

New GHCR packages default to **private**. Confirm rather than assume:

```bash
curl -sS -H "Authorization: token $GHCR_PAT" \
  https://api.github.com/user/packages/container/zeroship-platform | grep visibility
```

Reference size: about 525 MB built, about 711 MB unpacked on the host.

## One-time: prepare the host

Docker is the only prerequisite. Ship config only:

```bash
tar -C deploy -cf - \
    compose/docker-compose.yml \
    ops/Caddyfile ops/zeroship.toml ops/postgres-init.sql \
    verdaccio/config.yaml \
  | ssh root@<host> 'mkdir -p /opt/zeroship-deploy && tar -C /opt/zeroship-deploy -xf -'
```

Resulting layout (the compose file uses `../ops/...` and `../verdaccio/...`
relative paths, so these directory names are load-bearing):

```
/opt/zeroship-deploy/
  compose/   docker-compose.yml, docker-compose.override.yml, .env
  ops/       Caddyfile, zeroship.toml, postgres-init.sql
  verdaccio/ config.yaml
  secrets/   key material, mode 0600, never copied from the repo
```

### Secrets

Provision the secrets and scalar overlay once on the host. When `zeroship` is
installed directly, run:

```bash
zeroship dev init \
  --secrets-dir=/opt/zeroship-deploy/secrets \
  --env-file=/opt/zeroship-deploy/compose/.env
```

The compose invocation below automatically loads that `.env` beside its
compose file. If you choose different custom paths, pass Compose
`--env-file=/path/to/.env` and set `ZEROSHIP_SECRETS_DIR=/path/to/secrets` in
that file (or export it) so the bind mount follows the generator.

If the matching CLI exists only in the deployment image, authenticate to the
registry, pull that image, and run the same command from it:

```bash
IMAGE=ghcr.io/<owner>/zeroship-platform:<sha>
docker pull "$IMAGE"
docker run --rm \
  -v /opt/zeroship-deploy:/opt/zeroship-deploy \
  --entrypoint zeroship "$IMAGE" dev init \
  --secrets-dir=/opt/zeroship-deploy/secrets \
  --env-file=/opt/zeroship-deploy/compose/.env
```

The explicit paths are necessary because the CLI's repository-local defaults
are `deploy/compose/secrets` and `deploy/compose/.env`. The command creates
exactly seven files:

`control-signing.pem` `gateway-signing.pem` `auth-signing.pem` `broker-secret`
`pairwise-salt` `refresh-hash-key` `refresh-idem-key`

It also appends the generated scalar values described below to `.env`. The
directory is mode 0700 and the files are mode 0600 on Unix. Rerunning is safe:
existing valid material is kept byte-for-byte, missing entries are created, and
invalid or conflicting material causes an error instead of an implicit rotation.

`broker-secret` is deliberately ONE file read by both gateway
(`ZEROSHIP_GATEWAY_BROKER_SECRET_FILE`) and auth
(`ZEROSHIP_AUTH_BROKER_SECRET_FILE`). Same bytes is the requirement, not a
coincidence.

`pairwise-salt` is not independent: auth reads the file (via
`ZEROSHIP_AUTH_PAIRWISE_SALT_FILE`) while control and gateway read
`ZEROSHIP_PAIRWISE_SALT` from `.env`, and all three derive the same per-app
`pws_`. The generator writes exactly the env value to the file with NO trailing
newline and refuses a byte mismatch on later runs. A per-service generator or a
plain `echo` would silently break this identity invariant.

Do not export a different `ZEROSHIP_PAIRWISE_SALT` in the shell that launches
Compose. Host environment values take precedence over the compose `.env` file
and can therefore override control/gateway without changing the file auth
reads.

The refresh verifier file is also structured. Its first line has this form:

```bash
printf '1:%s\n' "$(openssl rand -hex 48)" > refresh-hash-key
```

Each nonempty `refresh-hash-key` line is
`version:hex-or-base64url-key`, with at least 32 decoded bytes. An unadorned
`openssl rand -base64 48` output is not a valid keyring line.

### Domain

The domain is a variable, not a constant. `ZEROSHIP_DOMAIN` drives the gateway's
`--app-base-domain`, the OIDC issuer, the gateway/auth public URLs, Caddy's
network aliases, and every site block in `deploy/ops/Caddyfile` (Caddy
substitutes `{$ZEROSHIP_DOMAIN}` at config-adapt time, which is why the compose
`caddy` service passes the variable into its environment - without that the
Caddyfile silently falls back to its own default and every host 404s).

`ZEROSHIP_ORIGIN_SCHEME` is separate because the origin does not serve what the
browser sees: behind a TLS-terminating proxy Caddy speaks plain http while the
issuer must advertise `https`. Defaults are `zeroship.localhost` + `http`,
which is a working local-dev stack with no DNS. This topology setting does not
enable or disable a security check.

**What the browser must see is `https` or `localhost`, and there is no setting
that relaxes it.** Every auth cookie is issued `Secure` with a `__Host-` prefix
unconditionally. Two configurations work: a real `https` origin at the browser,
or the default `*.localhost` domain, which browsers treat as a trustworthy
origin and therefore accept `Secure` cookies over plain `http`. A third does
not: a custom `ZEROSHIP_DOMAIN` (say `zeroship.lan`, or a bare IP) reached over
plain `http`. There the server sets the cookies and the browser silently
discards them, so login loops back to the form with nothing logged server-side.
Diagnose it by looking for `Set-Cookie` on the response while the next request
carries no `Cookie`. The fix is TLS at the edge, not a config change.

Check the merge rather than the intent, since a missed spot fails as a 404 and
not as an error:

```bash
docker compose config | grep -E 'ZEROSHIP_AUTH_PUBLIC_URL|app-base-domain|ZEROSHIP_(DOMAIN|ORIGIN_SCHEME):'
```

### .env

```bash
# Pin the exact build. Prefer the commit sha tag over `latest` so the running
# version is answerable, and record the digest for auditability.
ZEROSHIP_IMAGE=ghcr.io/<owner>/zeroship-platform:<sha>

ZEROSHIP_SECRETS_DIR=/opt/zeroship-deploy/secrets

# The domain this deployment serves, and the scheme its PUBLIC urls advertise.
ZEROSHIP_DOMAIN=<your domain>
ZEROSHIP_ORIGIN_SCHEME=https

# Safe to set: these are ${VAR}-indirected in EVERY service that reads them.
ZEROSHIP_WORKER_KEY=<openssl rand -hex 32>
ZEROSHIP_GATEWAY_STASH_SIGNING_KEY=<openssl rand -hex 32>
ZEROSHIP_AUTH_STASH_SIGNING_KEY=<openssl rand -hex 32>
ZEROSHIP_PAIRWISE_SALT=<openssl rand -hex 32>
```

`zeroship dev init` already added these generated values to the same file:

`ZEROSHIP_CONTROL_KEY` `ZEROSHIP_CONTROL_MASTER_KEY` `ZEROSHIP_WORKER_KEY`
`ZEROSHIP_MIGRATED_POLICY_SEAL_KEY` `ZEROSHIP_GATEWAY_STASH_SIGNING_KEY`
`ZEROSHIP_PAIRWISE_SALT` `ZEROSHIP_AUTH_STASH_SIGNING_KEY`
`ZEROSHIP_AUTH_TOTP_ENC_KEY`

`ZEROSHIP_CONTROL_STRIPE_WEBHOOK_SECRET` is **not** generated and **not**
required. Only Stripe can issue a value that verifies, so leave it unset
unless this deployment accepts Stripe webhooks; then set it to the
`whsec_...` your Stripe dashboard endpoint (or `stripe listen
--print-secret`) prints. While it is unset, control warns at boot and
rejects every delivery to `/internal/webhooks/stripe` with 500 - which is
the correct answer for a deployment Stripe cannot reach anyway.

Do not replace them with shared examples or per-service values. In particular,
one `ZEROSHIP_CONTROL_KEY` now supplies control, gateway, worker, migrated, and
auth together. `chmod 600 .env`; the generator applies that mode on Unix too.

### Control plane access

The base compose file does not publish a raw control port.
`deploy/ops/Caddyfile` exposes `control.<domain>` through the edge so CLI deploys
can reach it; every protected route still requires the generated control-key or
creator authentication. If an operator also needs a raw port for an SSH tunnel,
bind it to loopback with a **server-only** override:

```yaml
# /opt/zeroship-deploy/compose/docker-compose.override.yml
services:
  control:
    ports:
      - "127.0.0.1:9090:9090"
```

No `!override` tag is needed: the base service has no `ports` list to replace.

Reach control from a workstation over SSH:

```bash
ssh -L 9090:127.0.0.1:9090 root@<host>
zeroship deploy ./dist/app.zship --app=<uuid> --control=http://127.0.0.1:9090 --token=<PAT>
```

After BOTH prerequisites land, remove this loopback override, uncomment the
staged Caddy block, and deploy directly without SSH:

```bash
zeroship deploy ./dist/app.zship --app=<uuid> \
  --control=https://control.<domain> --token=<PAT>
```

### Host hygiene

Docker's default `json-file` logging is **unbounded** and the compose file sets
no `logging:` block, so container logs grow until the disk fills. Set a host
default before the stack has been up long:

```bash
cat > /etc/docker/daemon.json <<'EOF'
{ "log-driver": "json-file", "log-opts": { "max-size": "10m", "max-file": "3" } }
EOF
systemctl restart docker
```

A malformed `daemon.json` prevents dockerd from starting, so verify
`docker info` succeeds afterwards and keep a backup to restore.

This default applies to **newly created** containers only. Existing ones keep
their old config until recreated:

```bash
docker compose up -d --force-recreate
docker inspect compose-control-1 --format '{{.HostConfig.LogConfig.Config}}'
# expect map[max-file:3 max-size:10m]
```

Also worth capping the journal (`/etc/systemd/journald.conf.d/99-size-cap.conf`
with `SystemMaxUse=200M`) and clearing apt caches (`apt-get clean`,
`rm -rf /var/lib/apt/lists/*`), which is typically a few hundred MB.

## Deploy

Use the script. It does build, push, config sync, secret provisioning, render
and roll in one command, and it refuses in the cases that bit whoever did this
by hand:

```bash
deploy/scripts/deploy-remote.sh --host root@<host> \
  --registry ghcr.io/<owner>/zeroship-platform

deploy/scripts/deploy-remote.sh --host root@<host> --registry ... --dry-run
deploy/scripts/deploy-remote.sh --host root@<host> --rollback
```

It backs up `.env`, `docker-compose.yml` and the `Caddyfile` before touching
them, generates only the secrets the host is MISSING (an existing value is
never rotated, because rotating invalidates issued tokens), and refuses to
proceed when a variable the host sets looks like the old name of one the new
compose wants. That last case is why the script exists: `ZEROSHIP_SCHEME` was
renamed to `ZEROSHIP_ORIGIN_SCHEME`, the new name defaults to `http`, and
taking the default silently rewrites every public URL and the OIDC issuer to
`http://` -- compose renders, the stack boots, and the only symptom is a login
loop with a clean log.

`--image <ref>` pins an existing tag instead of building, for redeploying a
known-good image or rolling back to a prior one.

The manual sequence, for when you need to do a step by hand:

```bash
ssh root@<host>
printf '%s' "$GHCR_PAT" | docker login ghcr.io -u <owner> --password-stdin   # once
cd /opt/zeroship-deploy/compose
docker compose pull
docker compose up -d
```

Every service must come up together. `ZEROSHIP_CONTROL_KEY` is shared, so a
partial or rolling restart leaves two halves that cannot authenticate to each
other.

## Deploying a creator app

`zeroship deploy` needs the control plane, which is bound to loopback with no
Caddy route -- so from a dev machine it is unreachable, by design. Do not
publish it to fix that. Forward a port over the SSH you already have:

```bash
deploy/scripts/deploy-app.sh --host root@<host> --app <app-id> \
  --dir examples/db-todos --probe https://<app>.<domain>
```

The forward is torn down by PID from a trap. If you write your own, note that
`ControlMaster auto` in `~/.ssh/config` makes a backgrounded `ssh -L` hand the
forward to the mux master and exit immediately: the pid you captured is already
dead, your kill is a no-op, and the port outlives the script by `ControlPersist`
owned by a `[mux]` process. Pass `-o ControlMaster=no -o ControlPath=none`.

The token comes from `ZEROSHIP_TOKEN` or `--token-file`, never an argument --
arguments are visible in the process list to every user on the machine.

`migrate` runs first as a gated one-shot; control, gateway, worker and auth all
wait on `service_completed_successfully`. If migrate fails, they stay `Created`
and `up` exits non-zero. That is the design working, not a hang.

### Verify

Do not stop at `docker compose ps`. "Up" is not "serving".

```bash
docker compose ps --format 'table {{.Service}}\t{{.Status}}'
docker compose logs gateway auth control worker --since 5m | grep -iE 'ERROR|panic|FAILED'

# auth, through the edge, with the Host header Caddy routes on
curl -sS -o /dev/null -w '%{http_code}\n' -H 'Host: auth.<domain>' \
  http://127.0.0.1/oauth2/.well-known/openid-configuration     # 200
curl -sS -o /dev/null -w '%{http_code}\n' -H 'Host: auth.<domain>' \
  http://127.0.0.1/login                                       # 200

# control, loopback only. /readyz (not /healthz) is the one worth curling
# here: it answers 200 only once control can reach Postgres, so a 503 tells
# you the container is up and the database is not. /healthz is a constant
# 200 and only proves the process is alive.
curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9090/readyz   # 200

# control must NOT answer on the public interface
curl -sS -m 6 http://<public-ip>:9090/    # expect connection refused
```

Expected non-200s that are NOT failures:

- `api.<domain>` returns `{"error":"app 'api' not found"}` when no creator app
  is deployed at that host. Correct behaviour for an empty platform.
- `console.<domain>` 404s. The console was extracted to its own repo; this image
  does not ship it.
- `/.well-known/openid-configuration` at the root 404s. Discovery lives under
  `/oauth2/.well-known/openid-configuration`; the issuer is
  `https://auth.<domain>/oauth2`.

From outside, through Cloudflare:

```bash
curl -sS -o /dev/null -w '%{http_code}\n' https://auth.<domain>/login
```

A Cloudflare **522** means DNS is pointed at the origin but nothing is answering
on `:80`. It is a useful signal: 522 before the deploy and 200 after confirms
you are looking at the right origin.

## Upgrade

```bash
# build host
docker build --target runtime -t "$REG:$NEW_SHA" -f deploy/Dockerfile .
docker push "$REG:$NEW_SHA"

# server
sed -i "s|^ZEROSHIP_IMAGE=.*|ZEROSHIP_IMAGE=$REG:$NEW_SHA|" /opt/zeroship-deploy/compose/.env
docker compose pull && docker compose up -d
```

Rollback is the same edit pointing at the previous sha. This is the payoff for
tagging per commit instead of relying on `latest`.

## Things that will bite you

**The control port exists only in the server override.** The base file has no
control `ports` entry, so the loopback fallback is additive and does not use
`!override`. Verify the merge rather than the intent:

```bash
docker compose config | grep -A4 'published: "9090"'   # exactly one entry, host_ip 127.0.0.1
```

**Editing the Dockerfile after `docker build` starts does nothing.** BuildKit
reads it once at invocation. A file added mid-build is silently absent from the
result, and the build still succeeds. Always inspect the built image rather than
trusting the build exit code.

**`migrate` can fail its first run with `connect: error connecting to server`**
even with `depends_on: postgres: condition: service_healthy`. The healthcheck
passes slightly before the server is really accepting connections.
`restart: on-failure:3` absorbs it and the retry applies cleanly. Check the
container's final exit code, not the first log line:

```bash
docker inspect $(docker compose ps -aq migrate) \
  --format 'exit={{.State.ExitCode}} restarts={{.RestartCount}}'
```

**A missing build context is fine.** `docker compose up` only resolves
`build.context` when it actually builds, so the `build:` blocks can point at a
path that does not exist on the server. Just never pass `--build` there. The
failure mode is loud, which is what you want.

**`gh auth token` is usually the wrong credential.** It is scoped for git and
API work and typically lacks `write:packages`, and it may authenticate a
different account than the one owning the package namespace. Check
`x-oauth-scopes` and the `login` on `https://api.github.com/user` before
debugging a push.

**A registry PAT on the server is a stored credential.** `docker login` writes
it base64-encoded to `/root/.docker/config.json`. The host only ever needs to
pull, so use a read-only token there, not the `write:packages` one used to push.

## Security posture

The service binaries apply the same authentication, signature, cookie, and
secret-strength checks in compose as in every other deployment. Local setup
differs only in how `zeroship dev init` provisions strong, stable inputs.

Caddy still speaks plain HTTP in this topology because TLS terminates at
Cloudflare. The origin is therefore only as private as its IP; anything that
reaches the host directly on `:80` skips TLS. Generated secrets do not replace
origin-network isolation.
