# sandbox-agent operator runbook

Audience: operators running the `sandbox-agent` binary inside libkrun-backed Kubernetes Pods.

## What it is

`crates/sandbox-agent/` builds:

- library crate `zeroship-sandbox-agent`
- binary `sandbox-agent`
- checked-in k8s assets in `crates/sandbox-agent/k8s/`
- checked-in local-cluster harness `cargo run --release -p zeroship-sandbox-agent --example e2e_k3s`

The agent serves HTTP on port `7777` and exposes:

- unauthenticated: `/livez`, `/readyz`, `/healthz`, `/version`, `/metrics`
- signed: `/exec`, `/tree`, `/files/*`, `/shutdown`

`/version` includes `protocol_version`, `capabilities`, and `pubkey_fingerprint`.

## Cluster prerequisites

1. A `RuntimeClass` named `kvm-sandbox`.
2. A CNI that enforces `NetworkPolicy` if you want policy enforcement. Stock k3s flannel does not.
3. The Pod template from `crates/sandbox-agent/k8s/podtemplate.yaml` filled in with a real image digest and ConfigMap name.
4. The policy from `crates/sandbox-agent/k8s/networkpolicy.yaml` if you want the default ingress/egress restrictions.

## Key invariants

- `runtimeClassName: kvm-sandbox`
- annotation `run.oci.handler: krun`
- `restartPolicy: Never`
- `automountServiceAccountToken: false`
- read-only pubkey file at `/run/keys/controller-pubkey`

The pubkey file is not secret material. The agent reads it at boot and reports the derived `pubkey_fingerprint` in `/version`.

## Verifying a Pod

Port-forward the Pod and inspect `/version`:

```bash
kubectl port-forward -n sandboxes pod/<pod-name> 17777:7777
curl -s http://127.0.0.1:17777/version
```

Use the `pubkey_fingerprint` field to confirm which controller key the Pod trusts.

`kubectl exec` is not a reliable validation path here. libkrun-backed pods may reject it with `the handler does not support exec`.

## NetworkPolicy

Apply the checked-in policy with:

```bash
kubectl apply -f crates/sandbox-agent/k8s/networkpolicy.yaml
```

On local k3s with flannel, do not expect enforcement. That setup ignores `NetworkPolicy` objects.

## Common failures

### `/readyz` stays 503

Two common causes:

- the Pod was drained via `/shutdown`
- `sbx_agent_reaper_healthy` dropped to `0`

In either case, replace the Pod instead of trying to repair it in place.

### Every signed request returns 401

Check clock skew between the controller host and the k3s node. The agent uses a 5-second skew window.

Also inspect `/version` and the agent logs before rotating keys; a mismatched `pubkey_fingerprint` is usually the faster answer.

### The image is stuck in `ErrImagePull`

The checked-in Pod template expects a pinned digest. Load or publish that exact image before creating the Pod.

## Local end-to-end smoke

Use the checked-in harness:

```bash
cargo run --release -p zeroship-sandbox-agent --example e2e_k3s
```

That flow assumes:

- `kubectl` already points at the target cluster
- the agent image is already present in the cluster image store
- `RuntimeClass` `kvm-sandbox` already exists
