# nomad-driver-ch sprint status

Auto-maintained by the 10-minute cron + sprint fixers.

## Completed

- [x] T-0 (scaffold) — `ee4a76c3` (`nomad-driver-ch: scaffold from hashicorp/nomad-driver-virt (T-0)`)
- [x] T-0.5 (flake + main.go) — see commits
  - `de35f49e` — `nomad-driver-ch: flake.nix + flake.lock for dev shell (T-0.5 part 1)`
  - `9f1a7e49` — `nomad-driver-ch: cmd/main.go go-plugin entry point (T-0.5 part 2)`
- [x] T-1 (StartTask cold-boot CH spawn) — `6a8d2fcf` (`nomad-driver-ch/start_task: implement cold-boot CH spawn (T-1)`)
- [x] T-2 (graceful stop ladder + DestroyTask + SignalTask) — `dd017c23` (`nomad-driver-ch/stop_task: graceful-stop ladder (T-2)`)
- [x] T-3 (per-VM /30 tap setup + teardown) — `31e05a3c` (`nomad-driver-ch/start_task,stop_task: per-VM /30 tap setup + teardown (T-3)`)
- [x] T-4 (RecoverTask via ch-remote API socket) — `nomad-driver-ch/recover_task: reattach to running CH via API socket (T-4)`
- [x] T-5 (TaskStats: per-task telemetry via host /proc) — `nomad-driver-ch/task_stats: per-task telemetry via /proc+/sys (T-5)`
- [x] T-6 (Restore path: --restore + ch-remote resume wake) — `nomad-driver-ch/restore_task: --restore + ch-remote resume wake path (T-6)`
- [x] T-7 (Controller integration: SANDBOX_TASK_DRIVER=ch_plugin flag) — sandbox worktree `5fe36805` (`sandbox/nomad-ch: SANDBOX_TASK_DRIVER=ch_plugin flag switches jobspec to typed Go driver (T-7)`)
  - Controller's `crates/sandbox/src/backend/nomad_ch.rs::build_nomad_job_json` now branches on env flag: default keeps `Driver: "raw_exec"`; `ch_plugin` emits `Driver: "ch"` + typed `Config{}` mapped to `nomad-driver-ch/ch/task_config.go::TaskConfig`.
  - Field mapping verified against `task_config.go` codec tags: vm_index, kernel, cpus, memory_mb, restore_from, sandbox_id, workspace_img, user_home_img, pubkey_hex, subnet_base_octet, disks/fs/net.
  - Sandbox-crate lib tests: 312 → 319 (+7 T-7 tests). Driver tests unaffected (separate worktree at HEAD `3531a58b`, 63 PASS + 0 SKIP).
  - Production default unchanged — flag stays off until T-8 cluster validation.

## In progress

- T-8a (deploy artifacts staged) — driver side landed; sandbox-side commit lands in the sandbox worktree under the same sprint header
  - `scripts/build-binary.sh` produces `dist/nomad-driver-ch` (stripped, CGO_ENABLED=0, `-trimpath`, `-X main.gitSHA=<short-sha>`). Verified reproducible: SHA stable across rebuilds of the same commit.
  - `scripts/upload-to-gcs.sh <tag>` is a dry-run printer — emits the `gcloud storage cp` invocation but never runs it (cost-control until release pipeline lands).
  - `cmd/nomad-driver-ch/main.go`: `var gitSHA = "dev"` overridable via `-ldflags="-X main.gitSHA=<sha>"`; `--version` prints `nomad-driver-ch <sha>` and exits 0 before plugins.Serve.
  - Tests: 63 → 64 PASS (+1 = `TestVersionFlag` exercises default + ldflags-override builds).
  - Static-link check: `file dist/nomad-driver-ch` → `... statically linked, ... stripped`. Size 19.3 MB (well under the 28 MB sprint upper bound).
  - Live cluster smoke is T-8b, dispatched separately on explicit OK.

## Up next

- T-8b: Live cluster cutover. With T-8a artifacts staged:
  1. Operator runs `scripts/upload-to-gcs.sh v1` output manually to push `nomad-driver-ch.v1` to `gs://suger-dev-zsbx-artifacts/`.
  2. Provision a worker fleet with `INSTALL_CH_PLUGIN_DRIVER=1` (env var consumed by the sandbox-side `gcp-worker-startup.sh` patch landed in this sprint) — pulls the binary, drops a plugin-dir HCL stanza, sets `SANDBOX_TASK_DRIVER=ch_plugin` on `zsbx-ctl.service`, restarts nomad + controller.
  3. Run smoke (full snap/wake cycle) end-to-end through the Go plugin.
  4. On green: drop `nomad-vm-wrapper.sh` from the sandbox crate's scripts and remove the `raw_exec` branch from `nomad_ch.rs::build_nomad_job_json`.
