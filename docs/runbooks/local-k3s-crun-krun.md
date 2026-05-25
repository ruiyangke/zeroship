# Local k3s + crun + libkrun

Use this when you already have a k3s node whose `RuntimeClass` `kvm-sandbox` points at a libkrun-capable `crun` handler such as `crun-krun`.

This repo does **not** currently version the host-side k3s/crun/libkrun flake or startup scripts. The repo-backed assets for this workflow are:

- `crates/sandbox-agent/k8s/podtemplate.yaml`
- `crates/sandbox-agent/k8s/networkpolicy.yaml`
- `crates/sandbox-agent/examples/e2e_k3s.rs`

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
- The checked-in `podtemplate.yaml` is a template, not a ready-to-apply manifest. Fill in the image digest, ConfigMap name, and per-sandbox metadata first.

## Repo-backed verification

Apply the checked-in policy once your cluster has a CNI that enforces it:

```bash
kubectl apply -f crates/sandbox-agent/k8s/networkpolicy.yaml
```

Run the checked-in end-to-end harness against the cluster:

```bash
cargo run --release -p zeroship-sandbox-agent --example e2e_k3s
```

## Teardown

```bash
kubectl delete pod krun-smoke --ignore-not-found
rm -f /tmp/krun-smoke.yaml
```
