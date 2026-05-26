# 2026-05-26 Sandbox Docker Seccomp

## Status

Accepted pre-launch.

## Context

The Docker sandbox backend temporarily launched every sandbox container with
`--security-opt seccomp=unconfined` because the Rust agent uses compio/io_uring.
Docker's default seccomp profile denies the io_uring setup path, so the agent
could exit before binding `:7777`.

Unconfined seccomp gives untrusted creator code every syscall the host kernel
exposes. That is too broad for the local Docker backend now that the real agent
path is exercised by the builder e2e suite.

## Decision

Add `crates/sandbox/docker/seccomp-io-uring.json`.

The profile starts from Moby's default Docker seccomp profile and adds one
unconditional allow rule:

- `io_uring_setup` — creates the io_uring instance used by compio.
- `io_uring_enter` — submits and waits for queued completions.
- `io_uring_register` — registers buffers/files/eventfd resources used by the
  io_uring runtime as it evolves.

No other Docker-default-denied syscalls are allowed. The agent image already
uses ordinary process, file, socket, futex, timer, and epoll syscalls covered by
Docker's default allowlist. The live B1 sandbox path is the compatibility test:
if compio starts, serves signed RPC on `:7777`, and file/exec operations pass,
the scoped delta is sufficient.

The Docker backend passes:

```text
--security-opt seccomp=<resolved profile path>
```

instead of `seccomp=unconfined`.

## Path Resolution

`crates/sandbox/src/backend/docker.rs` resolves the profile at container launch:

1. `SANDBOX_DOCKER_SECCOMP_PROFILE`, canonicalized and required to be a file.
2. The crate source path from `CARGO_MANIFEST_DIR`:
   `docker/seccomp-io-uring.json`.
3. A package-style path next to the running binary:
   `../share/zeroship/sandbox/seccomp-io-uring.json`.
4. `/usr/local/share/zeroship/sandbox/seccomp-io-uring.json`.
5. `/etc/zeroship/sandbox/seccomp-io-uring.json`.

Missing profiles fail before `docker run`, with the env override named in the
error. Source-tree runs need no env var; packaged controllers should either
install the JSON in one of the share paths or set `SANDBOX_DOCKER_SECCOMP_PROFILE`.

## Consequences

Docker sandbox containers keep the default seccomp protections for syscalls such
as `keyctl`, module loading, and other non-allowlisted kernel surfaces while the
agent's io_uring runtime can boot normally.
