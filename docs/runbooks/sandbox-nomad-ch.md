# Nomad `ch` Backend Runbook

Operator runbook for the current sandbox backend in [`crates/sandbox/src/backend/nomad_ch.rs`](../../crates/sandbox/src/backend/nomad_ch.rs).

The controller submits one Nomad job per sandbox. Each job uses `Driver: "ch"` and a typed task `Config` matching [`nomad-driver-ch/ch/task_config.go`](../../nomad-driver-ch/ch/task_config.go).

Current in-tree components:

- controller backend: [`crates/sandbox/src/backend/nomad_ch.rs`](../../crates/sandbox/src/backend/nomad_ch.rs)
- task driver binary and Go sources: [`nomad-driver-ch/`](../../nomad-driver-ch/)
- cluster provisioning: [`provision-gcp-cluster.sh`](../../crates/sandbox/scripts/provision-gcp-cluster.sh)
- worker bootstrap: [`gcp-worker-startup.sh`](../../crates/sandbox/scripts/gcp-worker-startup.sh)
- teardown: [`teardown-gcp-cluster.sh`](../../crates/sandbox/scripts/teardown-gcp-cluster.sh)

## Prerequisites

- Nomad servers and client workers.
- Linux worker nodes with `/dev/kvm`; the worker startup script fails fast if nested virtualization is unavailable.
- Worker artifact storage for the controller binary, the guest kernel image, the guest rootfs image, and the `nomad-driver-ch` binary.
- If you are building the driver yourself, use [`nomad-driver-ch/scripts/build-binary.sh`](../../nomad-driver-ch/scripts/build-binary.sh). The companion [`upload-to-gcs.sh`](../../nomad-driver-ch/scripts/upload-to-gcs.sh) prints the manual GCS upload command.

Operational constraint from the current backend: worker `data_dir` must remain `/opt/nomad/data`. The controller derives alloc paths from that location during snapshot and wake flows.

## Provision a GCP cluster

The supported in-tree bootstrap path is:

```bash
./crates/sandbox/scripts/provision-gcp-cluster.sh
```

Important env overrides accepted by the script include:

- `PROJECT`
- `REGION`
- `ZONE`
- `PREFIX`
- `SERVER_COUNT`
- `WORKER_COUNT`
- `ARTIFACT_BUCKET`
- `CONTROLLER_OBJECT`
- `SNAPSHOT_BUCKET`
- `VM_INDEX_CEIL`

The provisioner creates the network, firewall rules, server VMs, worker VMs, and per-instance startup metadata. It also caches generated credentials in `/tmp/.zsbx-*` so reruns reuse the same PostgreSQL password and bearer tokens.

Workers bootstrap themselves with [`gcp-worker-startup.sh`](../../crates/sandbox/scripts/gcp-worker-startup.sh). That script:

- installs the `nomad-driver-ch` binary under `/etc/zeroship/nomad-plugins/`
- writes Nomad client config with `data_dir = "/opt/nomad/data"`
- writes the sandbox controller systemd unit with `SANDBOX_BACKEND=nomad-ch`
- waits for the `ch` driver to report `Detected=true` and `Healthy=true`
- emits the sentinel line `[startup] zsbx-worker-ready` on success

## Register the driver with Nomad

The public task-driver name is `ch` (`nomad-driver-ch/ch/driver.go`), but the plugin binary and Nomad plugin stanza are named `nomad-driver-ch`.

If you are not using the GCP worker bootstrap, install the binary manually and add a Nomad plugin file such as:

```hcl
plugin_dir = "/etc/zeroship/nomad-plugins"

plugin "nomad-driver-ch" {
  config {}
}
```

Then restart Nomad and verify the driver is live:

```bash
nomad node status -self -verbose | grep -E '^ch[[:space:]]+true[[:space:]]+true'
```

If you keep the binaries outside the driver's defaults, extend the `config {}` block using the fields defined in [`nomad-driver-ch/ch/driver.go`](../../nomad-driver-ch/ch/driver.go), for example `cloud_hypervisor_bin`, `ch_remote_bin`, `vm_index_lockdir`, `run_dir`, and `content_addressed_rootfs_roots`.

## Deploy the sandbox controller

The GCP worker startup script writes `zsbx-ctl.service` with the current Nomad backend settings. If you deploy manually, mirror the same core environment:

- `SANDBOX_BACKEND=nomad-ch`
- `SANDBOX_NOMAD_ADDR`
- `SANDBOX_NOMAD_DATACENTER`
- `SANDBOX_NOMAD_CH_RUNTIME_DIR`
- `SANDBOX_NOMAD_CH_HOST_STATE_DIR`
- `SANDBOX_NOMAD_CH_USER_HOME_ROOT`
- `SANDBOX_NOMAD_CH_VM_INDEX_FLOOR`
- `SANDBOX_NOMAD_CH_VM_INDEX_CEIL`
- `SANDBOX_NOMAD_CH_SUBNET_BASE_OCTET`
- `SANDBOX_NOMAD_CH_HOST_FENCE_TIMEOUT_SECS`
- `SANDBOX_DRIVER_STAGES_DISK_IMAGES=true`

The worker bootstrap also wires the controller's DB URL, bearer token, admin-token path, and snapshot-related settings. Use [`gcp-worker-startup.sh`](../../crates/sandbox/scripts/gcp-worker-startup.sh) as the source of truth for the full unit template.

If you override Nomad task resources manually, keep `MemoryMaxMB` at roughly `2 × MemoryMB`. Snapshot and wake flows on the current Cloud Hypervisor stack can fault the full guest RAM into the task memcg; collapsing `MemoryMaxMB` back toward `MemoryMB` can trigger cgroup OOM kills during snapshot or restore.

## Teardown

To delete the GCP validation cluster:

```bash
./crates/sandbox/scripts/teardown-gcp-cluster.sh
```

It deletes instances and reserved internal server IPs, then reports whether any `^${PREFIX}-` instances remain. Network, subnet, and firewall resources are intentionally left in place for reuse.

## Troubleshooting

- Driver never loads:
  check `journalctl -u nomad` and confirm the explicit `plugin "nomad-driver-ch" { config {} }` stanza is present. `plugin_dir` alone is not enough for the current Nomad bootstrap path used by [`gcp-worker-startup.sh`](../../crates/sandbox/scripts/gcp-worker-startup.sh).
- Worker never becomes ready:
  inspect `/var/log/zsbx-startup.log`, `systemctl status nomad`, and `nomad node status -self -verbose`.
- Controller is up but create returns backend unhealthy:
  the backend probes `${SANDBOX_NOMAD_ADDR}/v1/status/leader` and refuses new sandboxes while Nomad is unhealthy.
- Snapshot or wake flows break after Nomad config changes:
  confirm worker `data_dir` is still `/opt/nomad/data`.
- Single-replica cleanup after crashes:
  `SANDBOX_NOMAD_CH_STARTUP_ORPHAN_CLEANUP=true` enables boot-time orphan purge. Leave it off for HA or rolling-restart deployments.

## Driver behavior notes

- The driver implements `RecoverTask`, so a Nomad client or plugin restart re-attaches to a still-running Cloud Hypervisor process from the persisted task handle instead of treating it like a wrapper-style orphan. See [`nomad-driver-ch/ch/recover_task.go`](../../nomad-driver-ch/ch/recover_task.go).
- The driver implements `TaskStats`, so Nomad can surface per-task CPU and RSS usage for the Cloud Hypervisor process. See [`nomad-driver-ch/ch/task_stats.go`](../../nomad-driver-ch/ch/task_stats.go).
- `nomad alloc exec` is not supported for `Driver: "ch"`: the plugin advertises `Exec=false`, so workload interaction stays inside the guest VM rather than the Nomad task. See [`nomad-driver-ch/ch/driver.go`](../../nomad-driver-ch/ch/driver.go).
- The current driver-level `config {}` schema also includes `virtiofsd_bin`; the earlier inline field list is not exhaustive. See [`nomad-driver-ch/ch/driver.go`](../../nomad-driver-ch/ch/driver.go).
