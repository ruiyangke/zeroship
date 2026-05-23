# nomad-driver-ch

A Nomad task driver plugin for [Cloud Hypervisor](https://www.cloudhypervisor.org/).
Forked from [`hashicorp/nomad-driver-virt`](https://github.com/hashicorp/nomad-driver-virt)
on 2026-05-25; the libvirt layer has been stripped and replaced with a
direct `cloud-hypervisor` + `ch-remote` integration.

## What this replaces

`crates/sandbox/scripts/nomad-vm-wrapper.sh` — a 666-LOC bash script that
spawns CH, sets up the tap, supervises the PID, and runs cleanup traps. The
driver replaces all five of its responsibilities with typed Go code talking
gRPC over a Unix socket to the Nomad client (the standard go-plugin path).

The motivation, lifecycle mapping, and migration plan all live in
[`docs/proposals/nomad-driver-ch.md`](../docs/proposals/nomad-driver-ch.md).
The gap analysis vs the abandoned `volantvm/nomad-driver-ch` prior art lives
in [`docs/reviews/nomad-driver-ch-volantvm-spike.md`](../docs/reviews/nomad-driver-ch-volantvm-spike.md).

## Status

**SCAFFOLDED.** Every lifecycle method exists, the plugin implements
`drivers.DriverPlugin`, the binary compiles, `go vet` is clean. Every stub
returns an error tagged with a sprint marker (`T-1`..`T-5`). No CH process
has been spawned by this code yet.

See [`DESIGN.md`](./DESIGN.md) for the gap-analysis table and the per-sprint
breakdown.

## Build

```bash
make build       # → ./bin/nomad-driver-ch
make test        # all stubs may fail; compilation must succeed
make vet         # static analysis
```

Go ≥ 1.25 (matches the Nomad SDK pin). CGO is disabled — the plugin is
pure-Go and produces a fully static binary out of the box.

## Development

Use the Nix flake to enter a reproducible dev shell with the pinned Go
toolchain (1.25.x), `delve`, `golangci-lint`, and `gopls`:

```bash
nix develop                                # spawn a shell with the toolchain on PATH
nix develop -c go version                  # one-shot: confirm the toolchain (currently go1.25.10)
nix develop -c go vet ./...                # lint
nix develop -c go build ./cmd/nomad-driver-ch
nix develop -c go test ./...
```

Or via `make` once inside the shell: `make vet test build`.

If the flake fails to evaluate (e.g. offline machine, nixpkgs attribute
renamed), the one-shot escape hatch is:

```bash
nix shell nixpkgs#go_1_25 -- go vet ./...
nix shell nixpkgs#go_1_25 -- go build -o /tmp/nomad-driver-ch ./cmd/nomad-driver-ch
nix shell nixpkgs#go_1_25 -- go test ./...
```

Pinned toolchain: `pkgs.go_1_25` from nixpkgs-unstable. Bumping the pin
means re-running `nix flake update` and committing the lock change.

## Deployment

(Placeholder — finalised once the controller-side feature flag and the
Nomad agent config wiring are designed. Sketch:)

1. `make build` → publish `bin/nomad-driver-ch` to
   `gs://zsbx-prod-artifacts/drivers/nomad-driver-ch-${git-sha}`.
2. Worker startup script downloads it to `/opt/nomad/plugins/nomad-driver-ch`,
   `chmod +x`.
3. Nomad client config gets a `plugin "ch"` block:

   ```hcl
   plugin "ch" {
     config {
       cloud_hypervisor_bin = "/usr/local/bin/cloud-hypervisor"
       virtiofsd_bin        = "/usr/local/bin/virtiofsd"
       vm_index_lockdir     = "/var/lib/zsbx/vm-index"
       run_dir              = "/var/lib/zsbx/run"
     }
   }
   ```

4. `systemctl restart nomad`.
5. Controller flips `SANDBOX_TASK_DRIVER=ch` for the cluster.

## Module layout

Self-contained Go module at `nomad-driver-ch/`:

```
.
├── cmd/nomad-driver-ch/main.go    # plugin.Serve entry point
├── ch/                            # CH-specific business logic
│   ├── driver.go                  # Plugin + Capabilities + Fingerprint
│   ├── task_config.go             # HCL schema + decoded TaskConfig
│   ├── task_state.go              # TaskState (persisted) + taskHandle (in-memory)
│   ├── start_task.go              # StartTask (T-1)
│   ├── stop_task.go               # StopTask + DestroyTask (T-2)
│   ├── wait_task.go               # WaitTask (T-2)
│   ├── recover_task.go            # RecoverTask (T-4)
│   ├── inspect_task.go            # InspectTask (done, returns from memory)
│   ├── task_stats.go              # TaskStats (T-5)
│   └── ch_client.go               # ch-remote subprocess + UDS HTTP idiom
└── tests/                         # cross-package tests
    ├── driver_test.go             # plugin handshake / capabilities
    ├── task_config_test.go        # HCL parsing
    └── recover_task_test.go       # RecoverTask scenario matrix
```

## License

MPL-2.0, inherited from the upstream `hashicorp/nomad-driver-virt` fork.
Every source file carries a `Copyright (c) HashiCorp, Inc.` line and an
`SPDX-License-Identifier: MPL-2.0` header. The fork date and the new-file
attribution live in each file's header comment.

## Forward-compat note

The module path is `github.com/zeroship/nomad-driver-ch` (not
`github.com/.../appbase/nomad-driver-ch`). The driver lives in-tree now; it
may be promoted to a standalone repository at `github.com/zeroship/nomad-driver-ch`
once the lifecycle is stable. No consumer outside this module imports it,
and the controller does not link against it — they communicate via Nomad's
HTTP API, identical to today's `raw_exec` path.
