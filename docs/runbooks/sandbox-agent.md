# sandbox-agent operator runbook

Audience: SRE/operators running zeroship's `sandbox-agent` PID-1 binary inside per-sandbox libkrun microVMs on Kubernetes.

## What the agent is

A single static-ish Rust binary (`sandbox-agent`, ~6 MiB stripped) that runs as PID 1 inside every libkrun microVM. It exposes an Ed25519-signed HTTP API on port 7777 that the controller uses to drive the sandbox: run shell commands (`/exec`), read/write the workspace (`/files`, `/tree`), and observe health (`/livez`, `/readyz`, `/metrics`, `/version`).

Crate: [`crates/sandbox-agent/`](../../crates/sandbox-agent/)

## Cluster prerequisites

1. **CRI-O or containerd** with `crun` configured as a runtime, and `libkrun` available as an OCI handler. The flake under `~/zsbx-cluster/` is the reference setup; production should use the same versions pinned via Nix or container-image digests.
2. **RuntimeClass `kvm-sandbox`** registered, pointing at the `crun-krun` handler. Without this the Pods are rejected.
3. **CNI with NetworkPolicy support.** k3s ships flannel which **does not** enforce NetworkPolicy — ingress/egress rules are silently ignored. For production:
   - Install Calico, Cilium, or Antrea.
   - For local k3s testing: `--flannel-backend=none --disable-network-policy=false`, then install Calico.
4. A `sandboxes` namespace where the controller creates Pods.
5. A controller namespace (default: `zeroship-control`) with the `controller` Pod, and a Prometheus instance in `monitoring` (or wherever your scrape lives).

## Pod shape

See [`crates/sandbox-agent/k8s/podtemplate.yaml`](../../crates/sandbox-agent/k8s/podtemplate.yaml). The controller stamps `metadata.name`, `metadata.labels.sandbox-id`, `image` (pinned by digest, never `:dev`), and the per-sandbox `Secret` name.

Key invariants the controller MUST preserve:
- `runtimeClassName: kvm-sandbox` and `annotations: run.oci.handler: krun` (libkrun selector).
- Pubkey mount at `/run/keys/controller-pubkey`, mode `0444`, mounted read-only. The agent reads the file at startup, parses it as either 32 raw Ed25519 bytes or base64 of the same. The file persists — pubkey is non-secret.
- `automountServiceAccountToken: false` — the agent has no business talking to kube-apiserver.
- `restartPolicy: Never` — once a sandbox dies it stays dead; the controller spawns a fresh one. (A restart would reuse the same Secret/token, which is fine, but throws away the workspace `emptyDir`.)

## /exec child hardening

Every `/exec` child has the following applied in `pre_exec` (post-fork, pre-exec, in this order):

1. **Capability bounding set drop** (`PR_CAPBSET_DROP` for every cap). Done while still root, since `CAP_SETPCAP` is required and is lost on `setuid`.
2. **`setgid` → `setgroups([])` → `setuid`** to nobody:nogroup.
3. **`PR_SET_NO_NEW_PRIVS = 1`** — irreversibly blocks setuid bits, file capabilities, and LSM transitions on any future `exec()`.
4. **`RLIMIT_NOFILE = 1024`** (per-process). Always applied.
5. **`RLIMIT_NPROC = 256`** (per-uid). Applied only when actually dropping to nobody (the limit is per-uid; the freshly-dropped nobody starts at 0 processes, so 256 is comfortable headroom).

Verifying any of this from inside a sandbox:

```sh
# (Ed25519-signed POST /exec from controller)
grep -E '^NoNewPrivs:|^CapBnd:' /proc/self/status
awk '/^Max processes|^Max open files/' /proc/self/limits
id -u  # 65534 = nobody
kill -0 1  # EPERM expected
```

## Authentication

**Ed25519 signed requests** (`auth.ed25519-v1`). Every request to an auth-gated endpoint (`/exec`, `/files/*`, `/tree`, `/shutdown`) carries:

```
X-Sbx-Timestamp: <unix seconds>
X-Sbx-Nonce:     <ascii-alnum + - / _, ≤ 64 chars>
X-Sbx-Signature: <base64 64-byte Ed25519 signature>
```

Canonical string both controller and agent compute identically:

```
<METHOD>\n<PATH>\n<TS>\n<NONCE>\n<sha256_hex(BODY)>
```

Replay protection: 5 s timestamp skew window + 30 s LRU nonce cache (10 000 entries). Bursty controllers should batch, not spam.

Query strings are rejected outright (signed canonical doesn't cover them). If the controller ever needs query params it requires a `PROTOCOL_VERSION` bump.

Unauthenticated endpoints: `/livez`, `/readyz`, `/healthz` (alias), `/version`, `/metrics`. Cluster NetworkPolicy is the only access control on these.

### Why Ed25519, not HMAC

The agent holds **only the public key**. The signing (private) key never enters any sandbox VM at any point in the lifecycle:

- An attacker who compromises a sandbox (root in libkrun, `/proc/1/mem` read, etc.) recovers a non-secret pubkey — useless for forging requests against any agent in the fleet.
- The k8s storage layer carries no secret material for this auth path. The pubkey ships as a **`ConfigMap`** (not `Secret`); etcd-at-rest encryption isn't relevant.
- The sandbox cannot generate a key the controller would trust, because the trust anchor *is* the controller's pubkey; arbitrary new keypairs aren't in the trust set.

### Key material on disk

The pubkey file at `/run/keys/controller-pubkey` is mounted **read-only** (`mountPath` `readOnly: true`, mode `0444`). The agent reads it once at startup and leaves it in place — there is no unlink and no in-memory zeroize, because there's nothing secret to scrub. Operators can `kubectl exec ... cat /run/keys/controller-pubkey` to verify which trust anchor the agent is using; the fingerprint also appears in the agent's structured `Verifier` debug output (`pubkey_fp`).

File format: 32 raw bytes, OR base64 of those 32 bytes (with optional trailing whitespace). The loader auto-detects.

### Rotation

Replace the ConfigMap (`kubectl apply` a new pubkey), then roll the Pod (delete; controller spawns a fresh one with the new trust anchor). The agent reads the file once at boot, so a hot rotation requires Pod restart — acceptable since sandbox Pods are short-lived by design.

## NetworkPolicy

Apply [`crates/sandbox-agent/k8s/networkpolicy.yaml`](../../crates/sandbox-agent/k8s/networkpolicy.yaml). Allowed traffic:
- **Ingress:** controller → 7777, prometheus → 7777.
- **Egress:** DNS to kube-dns, HTTPS/HTTP to the public internet **except** the pod CIDR, service CIDR, RFC 1918, and 169.254.0.0/16 (cloud metadata services).

Verify enforcement by trying to curl the apiserver from inside a sandbox:

```sh
kubectl exec -n sandboxes <pod> -- curl -k https://kubernetes.default.svc
# expect: connection refused / timeout
```

If the curl succeeds, your CNI is not enforcing the policy. Recheck CNI install.

## Metrics

Prometheus scrape target: `<pod-ip>:7777/metrics`, no auth. Bounded-cardinality counters/gauges:

| Metric | Type | Notes |
| --- | --- | --- |
| `sbx_agent_exec_requests_total` | counter | Every /exec attempt. |
| `sbx_agent_exec_timeouts_total` | counter | /exec invocations whose wall-clock fired. Alert if > 0% of requests over 1h. |
| `sbx_agent_exec_nonzero_exits_total` | counter | Subset of requests with non-zero exit. Don't alert on this — user code can legitimately exit non-zero. |
| `sbx_agent_auth_failures_total{reason}` | counter | Buckets: `bad_signature`, `replay`, `skew`, `other`. **Alert on any non-zero replay or bad_signature** — those are attack signals. |
| `sbx_agent_files_bytes_{read,written}_total` | counter | Useful for billing/quota. |
| `sbx_agent_uptime_seconds` | gauge | Derived from `started_at_unix`; resets on Pod restart. |
| `sbx_agent_reaper_healthy` | gauge | 1 normally; 0 means the PID 1 zombie reaper crashed. **Page on this** — sustained 0 means the VM will eventually exhaust PIDs. |

## Common operational scenarios

### "The agent reports `/readyz` 503 forever"

Two causes:
1. Pod was sent `/shutdown`. Drain is permanent; controller should delete the Pod.
2. `sbx_agent_reaper_healthy = 0`. Reaper thread died. The Pod is unsalvageable — delete and recreate.

### "Controller gets 401 on every request"

Check for clock skew. Common causes: VM host time off by >5s, controller container running with a stale system clock. The agent enforces a 5-second skew window — anything older or newer is rejected as `BadTimestamp` / `SkewTooLarge`.

```sh
date -u                                    # controller node
kubectl exec -n sandboxes <pod> -- date -u # agent VM
```

If they differ by more than a few seconds, fix NTP on whichever side drifted. The skew window is intentionally tight (5 s) — widening it weakens replay protection.

### "Exec returns `status: -1`"

Means we never observed a clean exit. Usually one of:
- Process killed by signal (e.g. `-9` for SIGKILL from the timeout path). Negative `-N` is the signal number.
- The internal `mpsc` channel between the reaper and the handler closed without delivering a code (only happens if the reaper thread crashed mid-request).

Check `sbx_agent_reaper_healthy` and recent reaper-related warnings in the structured JSON logs (`SANDBOX_AGENT_LOG=info`).

### "The image won't pull / Pod stuck in `ErrImagePull`"

The controller pushes images by **digest**. If the image isn't in the node's containerd image store you'll see `ErrImagePull`. Either:
- Mirror to your registry and push with `imagePullSecrets`, or
- Pre-load via `ctr -n k8s.io images import` if you're running an air-gapped cluster.

Never run sandbox Pods with `imagePullPolicy: Always` against a public registry — every `:dev` rebuild would re-pull a fresh layer and rate-limit you off Docker Hub.

## Upgrading the agent

Version is stamped at build time into `sbx_agent_*` `/version` (capabilities `["auth.ed25519-v1", ...]`). The protocol is currently at version 1; any wire-format change requires bumping `PROTOCOL_VERSION` and updating the controller to negotiate.

To roll out a new image:
1. Build + push the new image, get its digest.
2. Update the controller's image-digest config.
3. Old Pods continue running; new sandboxes pick up the new digest.
4. Existing Pods are not in-place upgraded — the agent has no `/upgrade` endpoint by design (a self-modifying binary in a sandbox VM is a worse attack surface than just spawning a fresh Pod).

## Local end-to-end smoke

The crate ships a runnable e2e example that exercises every endpoint against a real Pod on a local k3s cluster:

```sh
cargo run --release -p zeroship-sandbox-agent --example e2e_k3s
```

Prereqs: `kubectl` configured against a cluster with the RuntimeClass + image loaded. See [`crates/sandbox-agent/examples/e2e_k3s.rs`](../../crates/sandbox-agent/examples/e2e_k3s.rs).
