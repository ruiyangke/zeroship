# 2026-05-26 Sandbox Local Harness

## Status

Accepted pre-launch.

## Context

The builder e2e suite needs a faithful local `zeroship-sandbox` controller instead
of a mock backend. The docker backend already creates a real container per
sandbox, bind-mounts the host workspace at `/workspace`, and reaches the in-
container agent at `http://<container-ip>:7777` for preview and signed RPC paths.

No checked-in base image existed for the docker backend. Host-built agent binaries
are also not safe to copy into a Debian image from a Nix shell, because they can
link against `/nix/store` dynamic loader paths.

## Decision

Add `crates/sandbox/docker/Dockerfile.sandbox-base`.

The image builds `zeroship-sandbox-agent` inside `rust:1.94-bookworm`, then copies
that Debian-compatible binary into `debian:12-slim`. The final image keeps only
the runtime shell/coreutils surface the docker backend needs for `docker exec`
commands (`sh`, `timeout`) plus CA certificates. It creates `/workspace`,
`/home/u`, and `/run/keys`, then starts the real agent as the OCI entrypoint.

The docker backend passes the per-sandbox id through
`SANDBOX_AGENT_SANDBOX_ID`. This matches the current agent boot contract, which
requires either that env var or `/run/keys/sandbox-id` before serving. The backend
continues to mount `/run/keys/controller-pubkey` read-only and continues to append
`sleep infinity` after the image name; because the image uses the agent as
ENTRYPOINT, those words are inert argv to the Rust binary.

The docker backend also launches sandbox containers with
`--security-opt seccomp=unconfined`. The real agent uses the same compio/io_uring
runtime as the rest of zeroship; Docker's default seccomp profile denies the
needed setup call and the agent exits before binding `:7777`.

## Local Harness

Add:

- `tests/sandbox_up.sh`
- `tests/sandbox_down.sh`

`sandbox_up.sh` is idempotent and does the full local bring-up:

1. Starts the repo's compose Postgres service.
2. Creates the `zeroship_sandbox` database when missing.
3. Ensures the docker network `zeroship-sandbox-net` exists.
4. Builds `zeroship/sandbox-base:latest` from the new Dockerfile.
5. Builds `target/debug/zeroship-sandbox`.
6. Starts the controller on `:9091` with docker backend, pg migrations enabled,
   an explicit stable `SANDBOX_HOST_ID`, and local state under
   `.zeroship/sandbox-harness`.
7. Waits for `GET /readyz` and prints `SANDBOX_URL` and `SANDBOX_TOKEN`.

`sandbox_down.sh` stops the controller process recorded by the harness and removes
any docker containers labeled `zeroship.sandbox`. It leaves the shared compose
Postgres running by default; pass `--postgres` to stop it too.

## Gotchas

- The token must be at least 32 bytes. The harness default is long enough.
- The agent image must be rebuilt through Docker, not copied from the host.
- The docker backend still uses direct host file IO and `docker exec` for the file
  and exec endpoints. The agent is still booted for the signed preview/RPC surface
  expected by the backend contract.
- The harness sets `SANDBOX_HOST_ID` instead of relying on the pg layer's
  `<SANDBOX_PERSIST_DIR>/state/host_id` file. That file is production-hardened to
  require root ownership on read, which is not a good fit for an unprivileged local
  script.
