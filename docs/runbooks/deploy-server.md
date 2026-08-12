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

Six files, generated **on the host** and never in the repo. Formats are from
`auth-deploy.md`:

```bash
S=/opt/zeroship-deploy/secrets
mkdir -p $S && chmod 700 $S
openssl genpkey -algorithm ed25519 -out $S/auth-signing.pem
openssl genpkey -algorithm ed25519 -out $S/gateway-signing.pem
openssl rand -base64 48 > $S/broker-secret
openssl rand -base64 48 > $S/refresh-hash-key
openssl rand -base64 48 > $S/refresh-idem-key
chmod 600 $S/*
```

`broker-secret` is deliberately ONE file read by both gateway
(`GATEWAY_BROKER_SECRET_FILE`) and auth (`AUTH_BROKER_SECRET_FILE`). Same bytes
is the requirement, not a coincidence.

`pairwise-salt` is the seventh and is NOT independent: auth reads it from a file
while control and gateway read `PAIRWISE_SALT` from env, and all three derive the
same per-app `pws_`. Write the file from the env value so they cannot drift:

```bash
printf '%s' "$(grep -E '^PAIRWISE_SALT=' /opt/zeroship-deploy/compose/.env | cut -d= -f2-)" \
  > $S/pairwise-salt
chmod 600 $S/pairwise-salt
```

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

Check the merge rather than the intent, since a missed spot fails as a 404 and
not as an error:

```bash
docker compose config | grep -E 'AUTH_PUBLIC_URL|app-base-domain|ZEROSHIP_(DOMAIN|ORIGIN_SCHEME):'
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
GATEWAY_OIDC_SECRET=<openssl rand -hex 32>
STASH_SIGNING_KEY=<openssl rand -hex 32>
PAIRWISE_SALT=<openssl rand -hex 32>
```

`chmod 600 .env`.

**Do NOT set `ZEROSHIP_CONTROL_KEY` here.** It is hardcoded to `platform-key` in
control, gateway and worker, and `${VAR}`-indirected only in auth. Setting it
moves auth alone and desyncs it from the other three. Changing it for real means
parameterizing all four in the compose file first.

### Control plane access

The base compose file does not publish control. `deploy/ops/Caddyfile` contains
a fully commented `control.<domain>` route, but it must stay disabled while
control still runs with `--dev-insecure` and the stack still uses the hardcoded
`platform-key`. Enabling it now would put relaxed control-plane auth and a known
credential on the public edge.

Until both prerequisites are removed, add a **server-only**, loopback-only
publication for the SSH fallback:

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

```bash
ssh root@<host>
printf '%s' "$GHCR_PAT" | docker login ghcr.io -u <owner> --password-stdin   # once
cd /opt/zeroship-deploy/compose
docker compose pull
docker compose up -d
```

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

# control, loopback only
curl -sS -o /dev/null -w '%{http_code}\n' http://127.0.0.1:9090/health   # 200

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

The compose file is labelled DEV ONLY at the top and means it. Before treating
any of this as production:

- `--dev-insecure` on control, gateway and auth relaxes admin and internal auth.
- `ZEROSHIP_CONTROL_KEY` / `ZEROSHIP_MASTER_KEY` are hardcoded weak values.
- Caddy speaks plain HTTP; TLS lives entirely in Cloudflare, so the origin is
  only as private as its IP. Anything that reaches the host directly on `:80`
  skips TLS.

Loopback-binding control keeps the weak credentials off the internet. It does
not make them strong.
