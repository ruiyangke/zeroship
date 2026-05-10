# Local k3s + crun + libkrun (microVM sandbox)

A single-node k8s cluster on a NixOS host where pods with
`runtimeClassName: kvm-sandbox` execute as **libkrun microVMs** instead
of plain containers. Used to prototype the production sandbox runtime
class for AI-agent (deepagents) workloads.

This setup lives entirely under `~/zsbx-cluster/`. The flake is
self-contained — copy that directory to any other NixOS host with
`/dev/kvm` and the steps below reproduce.

## What you get

| Layer | Component |
| --- | --- |
| Hypervisor | libkrun 1.15 + libkrunfw 4.10 (kernel 6.12.34) |
| OCI runtime | crun 1.24 built `--with-libkrun` |
| Container runtime | k3s-bundled containerd 2.1 |
| Orchestrator | k3s 1.34 (single-node, no traefik/servicelb) |

Default `runc` (handler=runc) and our `crun-krun` (handler=crun-krun)
both registered — pick per-pod via `RuntimeClass`.

## Preconditions

- NixOS (tested 25.11) on bare metal or KVM-passthrough VM
- `/dev/kvm` exists and your user is in the `kvm` group
- `nix` with `flakes` + `nix-command` experimental features
- Passwordless sudo (or willingness to type the password)

Verify:

```bash
ls -l /dev/kvm                         # crw-rw-rw-, group kvm
grep -c -E 'vmx|svm' /proc/cpuinfo     # > 0
groups | grep kvm                      # you're in kvm group
```

## Layout

```
~/zsbx-cluster/
├── flake.nix                       # crun overlay (--with-libkrun) + k3s pin
├── flake.lock
├── containerd-config.toml.tmpl     # registers crun-krun runtime in k3s containerd
├── runtimeclass.yaml               # kvm-sandbox -> handler=crun-krun
├── test-pod.yaml                   # smoke pod (annotation run.oci.handler: krun)
├── start-k3s.sh                    # launches k3s with our config
├── result-crun-krun -> /nix/store/...   # nix build output
├── result-k3s       -> /nix/store/...
├── kubeconfig.yaml                 # written by k3s
└── k3s-data/                       # cluster state (gitignored)
```

## One-time setup

```bash
mkdir ~/zsbx-cluster && cd ~/zsbx-cluster
# Copy flake.nix, containerd-config.toml.tmpl, runtimeclass.yaml,
# test-pod.yaml, start-k3s.sh from this repo (see source below).
nix --extra-experimental-features 'nix-command flakes' flake update
nix --extra-experimental-features 'nix-command flakes' build .#crun-krun -o result-crun-krun
nix --extra-experimental-features 'nix-command flakes' build .#k3s        -o result-k3s
./result-crun-krun/bin/crun --version | grep '+LIBKRUN'   # verify
```

## Day-to-day

```bash
# Terminal A: launch k3s (foreground; logs stream).
sudo ~/zsbx-cluster/start-k3s.sh

# Terminal B: drive kubectl.
export KUBECONFIG=~/zsbx-cluster/kubeconfig.yaml
kubectl get nodes
kubectl apply -f ~/zsbx-cluster/runtimeclass.yaml
kubectl apply -f ~/zsbx-cluster/test-pod.yaml
kubectl logs krun-smoke
# Expected: "Linux krun-smoke 6.12.34 ..." — different kernel than host.
```

## Verifying the microVM boundary

The smoke pod's container runs `uname -a` at startup. Compare:

```bash
# On the host
uname -r          # e.g. 6.12.80 (your NixOS kernel)

# Inside the pod (via stdout, NOT kubectl exec)
kubectl logs krun-smoke
# Linux krun-smoke 6.12.34 #1 SMP ... -- libkrunfw's kernel
```

`kubectl exec` into a libkrun pod returns
`the handler does not support exec` — this is **expected** and is
itself a proof of microVM behavior; runc-handled pods do support exec.

## Constraints

- **`kubectl exec` doesn't work into krun pods.** libkrun does not
  expose the runc OCI hooks needed for `runtime.Exec`. Capture output
  via `kubectl logs`, init scripts, or an in-VM agent over a published
  port.
- **No persistent virtio-fs share by default.** PVCs work via standard
  k8s; bind-mounts get translated to virtio-fs by libkrun.
- **No GPU.** libkrun upstream lacks GPU passthrough today.
- **macOS hosts unsupported here.** This setup is Linux/KVM only.

## Gotchas hit during initial setup

1. **k3s bundled containerd is a NixOS friend** — the package wraps
   the bundled binaries so they run on NixOS. Stock `services.k3s` is
   *not* required; the imperative path used by `start-k3s.sh` works.
2. **crun's BPF program pinning is broken in systemd-cgroup mode** on
   nested cgroup paths. crun's `add_bpf_program` only `mkdir`s the
   immediate `/sys/fs/bpf/crun/`, never the `kubepods/pod<uid>/`
   parents that `BPF_OBJ_PIN` needs. We sidestep by setting
   `SystemdCgroup = false` in our containerd runtime config —
   crun then takes the `cgroup-cgroupfs.c` path and never pins BPF
   programs to the bpffs.
3. **crun configure flag is `--with-libkrun`, not `--enable-krun`.**
   1.24's autotools uses `AC_ARG_WITH([libkrun])`. Watch the
   `crun --version` feature line for `+LIBKRUN`.
4. **Don't put `{{ template "base" . }}` inside a TOML comment.** k3s'
   Go-template renderer evaluates template directives anywhere in
   the file, including inside `#` comments — the rendered config
   ends up corrupt. Keep template directives on their own line.
5. **k3s data must live OUTSIDE the flake source tree.** Otherwise
   `nix build` tries to read root-owned files inside the flake's
   git tree and bails. We let k3s write under
   `~/zsbx-cluster/k3s-data/` and `.gitignore` it.

## Tearing down

```bash
sudo pkill -KILL -f 'k3s server'
sudo pkill -KILL -f 'k3s-containerd'
sudo pkill -KILL -f 'containerd-shim-runc-v2 -namespace k8s.io'
sudo rm -rf ~/zsbx-cluster/k3s-data ~/zsbx-cluster/kubeconfig.yaml
```

## Source files

### `flake.nix`

```nix
{
  description = "Local crun+libkrun + k3s sandbox cluster (NixOS)";
  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
  outputs = { self, nixpkgs }:
    let
      system = "x86_64-linux";
      pkgs = import nixpkgs { inherit system; };
      crun-krun = pkgs.crun.overrideAttrs (old: {
        pname = "crun-krun";
        buildInputs = (old.buildInputs or [ ]) ++ [ pkgs.libkrun ];
        configureFlags = (old.configureFlags or [ ]) ++ [ "--with-libkrun" ];
        nativeBuildInputs = (old.nativeBuildInputs or [ ]) ++ [ pkgs.makeWrapper ];
        postFixup = (old.postFixup or "") + ''
          wrapProgram $out/bin/crun \
            --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath [ pkgs.libkrun pkgs.libkrunfw ]}
        '';
      });
    in {
      packages.${system} = {
        inherit crun-krun;
        inherit (pkgs) libkrun libkrunfw k3s kubectl;
        default = crun-krun;
      };
    };
}
```

### `containerd-config.toml.tmpl`

```toml
{{ template "base" . }}

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes."crun-krun"]
  runtime_type = "io.containerd.runc.v2"
  pod_annotations = ["run.oci.handler"]
  privileged_without_host_devices = false

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes."crun-krun".options]
  BinaryName = "/home/ruiyang/zsbx-cluster/result-crun-krun/bin/crun"
  SystemdCgroup = false
```

### `runtimeclass.yaml`

```yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: kvm-sandbox
handler: crun-krun
scheduling:
  nodeSelector:
    zeroship.dev/sandbox: "true"
```

### `test-pod.yaml`

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: krun-smoke
  annotations:
    run.oci.handler: krun        # the magic — tells crun to use libkrun
spec:
  runtimeClassName: kvm-sandbox
  restartPolicy: Never
  containers:
    - name: shell
      image: docker.io/library/alpine:3.20
      command: ["sh", "-c", "uname -a; echo --- ; head -5 /proc/cpuinfo; sleep 600"]
      resources:
        limits: { memory: 256Mi, cpu: "1" }
```

### `start-k3s.sh`

```bash
#!/usr/bin/env bash
set -euo pipefail
CLUSTER_DIR=/home/ruiyang/zsbx-cluster
DATA_DIR=${CLUSTER_DIR}/k3s-data
TMPL_DST=${DATA_DIR}/agent/etc/containerd/config.toml.tmpl
mkdir -p "$(dirname "${TMPL_DST}")"
install -m 0644 "${CLUSTER_DIR}/containerd-config.toml.tmpl" "${TMPL_DST}"
exec "${CLUSTER_DIR}/result-k3s/bin/k3s" server \
    --data-dir="${DATA_DIR}" \
    --write-kubeconfig="${CLUSTER_DIR}/kubeconfig.yaml" \
    --write-kubeconfig-mode=0644 \
    --disable=traefik --disable=servicelb \
    --node-name=zsbx-local \
    --node-label=zeroship.dev/sandbox=true
```

## Next

- Wire this RuntimeClass into the `zeroship-sandbox` controller (next
  rewrite of `crates/sandbox/`): translate `POST /sessions` into a Pod
  create with `runtimeClassName: kvm-sandbox` + a per-session PVC.
- Bake an agent runtime image (`ghcr.io/zeroship/agent-runtime`) with
  node 22 + python 3.12 + vite/deepagents deps prewarmed, since
  libkrun lacks snapshot/restore (compensates for slower cold start).
- Add `CiliumNetworkPolicy` for FQDN egress allowlist when moving to
  multi-node.
