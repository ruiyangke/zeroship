# Local k3s + crun + libkrun

Use this when you already have a k3s node whose `RuntimeClass` `kvm-sandbox` points at a libkrun-capable `crun` handler such as `crun-krun`.

This repo does **not** currently version the host-side k3s/crun/libkrun flake or startup scripts, and it no longer versions the sandbox-agent assets either: the sandbox/preview backend (controller, in-VM agent, and its k8s pod template, NetworkPolicy, and `e2e_k3s` harness) was extracted to the standalone `zeroship-sandbox` project. Everything below is host-side setup plus a self-contained smoke test; the agent-backed verification steps run from that project's checkout.

## Preconditions

- Linux host with `/dev/kvm`
- k3s cluster already running
- `kubectl` configured for that cluster
- `RuntimeClass` `kvm-sandbox` already present

Verify the node state first:

```bash
kubectl get nodes
kubectl get runtimeclass kvm-sandbox
```

## Smoke pod

Use a self-contained Pod manifest instead of a missing repo-local `test-pod.yaml`:

```bash
cat >/tmp/krun-smoke.yaml <<'EOF'
apiVersion: v1
kind: Pod
metadata:
  name: krun-smoke
  annotations:
    run.oci.handler: krun
spec:
  runtimeClassName: kvm-sandbox
  restartPolicy: Never
  containers:
    - name: shell
      image: docker.io/library/alpine:3.20
      command: ["sh", "-c", "uname -a; echo ---; head -5 /proc/cpuinfo; sleep 600"]
      resources:
        limits:
          memory: 256Mi
          cpu: "1"
EOF
kubectl apply -f /tmp/krun-smoke.yaml
kubectl wait --for=condition=Ready pod/krun-smoke --timeout=120s
kubectl logs krun-smoke
```

The guest kernel should differ from the host kernel. That is the quick proof that the pod is running inside a microVM, not as a plain container.

## Known constraints

- `kubectl exec` into a libkrun pod is expected to fail with `the handler does not support exec`.
- k3s with flannel does not enforce `NetworkPolicy`; use a CNI such as Calico or Cilium if you need policy enforcement.
- The sandbox-agent pod template is a template, not a ready-to-apply manifest. Fill in the image digest, ConfigMap name, and per-sandbox metadata first.

## Agent-backed verification

The sandbox-agent NetworkPolicy and the `e2e_k3s` end-to-end harness are not in
this repo. Run them from a `zeroship-sandbox` checkout against the cluster you
just verified above; this runbook only establishes that the host's
`kvm-sandbox` `RuntimeClass` really boots a microVM.

## Teardown

```bash
kubectl delete pod krun-smoke --ignore-not-found
rm -f /tmp/krun-smoke.yaml
```
